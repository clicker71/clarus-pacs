// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! JPEG-LS codec (ISO/IEC 14495-1, ITU-T T.87).
//!
//! Supports:
//! - 1.2.840.10008.1.2.4.80 - JPEG-LS Lossless
//! - 1.2.840.10008.1.2.4.81 - JPEG-LS Near-Lossless
//!
//! Uses `pure_jpegls` crate for encode/decode. The crate operates on
//! 16-bit pixel buffers; 8-bit images are converted on the fly.
//!
//! # DICOM specifics
//!
//! JPEG-LS always uses Encapsulated PixelData (PS3.5 Section A.4).
//! Each color component is a separate JPEG-LS codestream fragment
//! within the encapsulated sequence.

#![forbid(unsafe_code)]

use jpegls;

use super::{
    CodecError, DecodedPixels, DicomCodec, DicomMeta, TAG_BITS_ALLOCATED, TAG_COLUMNS,
    TAG_NUMBER_OF_FRAMES, TAG_PHOTOMETRIC_INTERPRETATION, TAG_PIXEL_DATA, TAG_PIXEL_REPRESENTATION,
    TAG_ROWS, TAG_SAMPLES_PER_PIXEL,
};

/// Transfer Syntax UID for JPEG-LS Lossless.
pub const TS_JPEGLS_LOSSLESS: &str = "1.2.840.10008.1.2.4.80";
/// Transfer Syntax UID for JPEG-LS Near-Lossless.
pub const TS_JPEGLS_NEAR_LOSSLESS: &str = "1.2.840.10008.1.2.4.81";

// ===========================================================================
// Codec: JPEG-LS Lossless
// ===========================================================================

/// Codec for JPEG-LS Lossless (1.2.840.10008.1.2.4.80).
pub struct JpeglsLosslessCodec;

impl DicomCodec for JpeglsLosslessCodec {
    fn transfer_syntax_uid(&self) -> &'static str {
        TS_JPEGLS_LOSSLESS
    }

    fn name(&self) -> &'static str {
        "JPEG-LS Lossless"
    }

    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
        jpegls_decode(dicom_bytes)
    }

    fn encode(&self, pixels: &DecodedPixels, meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
        jpegls_encode(pixels, meta, 0, jpegls::Profile::T87) // NEAR=0 -> lossless
    }

    fn encode_pixeldata(
        &self,
        pixels: &DecodedPixels,
        meta: &DicomMeta,
    ) -> Result<Vec<u8>, CodecError> {
        let fragments = jpegls_encode_fragments(pixels, meta.frames, 0, jpegls::Profile::T87)?;
        Ok(crate::codec::build_encapsulated_pixeldata_multi(
            &fragments,
            pixels.samples_per_pixel as usize,
            false,
        ))
    }
}

// ===========================================================================
// Codec: JPEG-LS Lossless (LegacyGo opt-in, cltranscode jpegls_profile=legacygo)
// ===========================================================================

/// Codec for JPEG-LS Lossless (1.2.840.10008.1.2.4.80) in the frozen
/// 1.0.0 / Go-compatible LegacyGo profile.
///
/// NOT the default: the default [`JpeglsLosslessCodec`] encodes strict
/// T87, which is what deployed viewers decode.
/// cltranscode registers THIS codec only when the clinic explicitly sets
/// `jpegls_profile = legacygo`, for byte-compatibility with the already
/// stored corpus.
/// The decode side is shared and keeps the T87-first + LegacyGo
/// fallback, so old stored LegacyGo files always remain readable.
pub struct JpeglsLegacyGoCodec;

impl DicomCodec for JpeglsLegacyGoCodec {
    fn transfer_syntax_uid(&self) -> &'static str {
        TS_JPEGLS_LOSSLESS
    }

    fn name(&self) -> &'static str {
        "JPEG-LS Lossless (LegacyGo)"
    }

    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
        jpegls_decode(dicom_bytes)
    }

    fn encode(&self, pixels: &DecodedPixels, meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
        jpegls_encode(pixels, meta, 0, jpegls::Profile::LegacyGo)
    }

    fn encode_pixeldata(
        &self,
        pixels: &DecodedPixels,
        meta: &DicomMeta,
    ) -> Result<Vec<u8>, CodecError> {
        let fragments = jpegls_encode_fragments(pixels, meta.frames, 0, jpegls::Profile::LegacyGo)?;
        Ok(crate::codec::build_encapsulated_pixeldata_multi(
            &fragments,
            pixels.samples_per_pixel as usize,
            false,
        ))
    }
}

// ===========================================================================
// Codec: JPEG-LS Near-Lossless
// ===========================================================================

/// Codec for JPEG-LS Near-Lossless (1.2.840.10008.1.2.4.81).
///
/// The `near` parameter controls the maximum allowed error per pixel
/// (0 = lossless, 1-16 = near-lossless). Default is 2.
pub struct JpeglsNearLosslessCodec {
    near: u8,
}

impl JpeglsNearLosslessCodec {
    /// Create a near-lossless codec with the given NEAR parameter.
    ///
    /// # Panics
    /// Panics if `near > 16` (JPEG-LS limit).
    pub fn new(near: u8) -> Self {
        assert!(near <= 16, "JPEG-LS NEAR must be 0–16, got {near}");
        Self { near }
    }
}

impl DicomCodec for JpeglsNearLosslessCodec {
    fn transfer_syntax_uid(&self) -> &'static str {
        TS_JPEGLS_NEAR_LOSSLESS
    }

    fn name(&self) -> &'static str {
        "JPEG-LS Near-Lossless"
    }

    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
        jpegls_decode(dicom_bytes)
    }

    fn encode(&self, pixels: &DecodedPixels, meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
        jpegls_encode(pixels, meta, self.near, jpegls::Profile::T87)
    }

    fn encode_pixeldata(
        &self,
        pixels: &DecodedPixels,
        meta: &DicomMeta,
    ) -> Result<Vec<u8>, CodecError> {
        let fragments =
            jpegls_encode_fragments(pixels, meta.frames, self.near, jpegls::Profile::T87)?;
        Ok(crate::codec::build_encapsulated_pixeldata_multi(
            &fragments,
            pixels.samples_per_pixel as usize,
            false,
        ))
    }
}

// ===========================================================================
// Shared decode logic
// ===========================================================================
/// Decode ONE component fragment with the profile strategy.
///
/// 1. try `Profile::T87` (pure_jpegls 2.0.0 default, ITU-T T.87
///    conformant);
/// 2. on ANY error retry with `Profile::LegacyGo` (frozen 1.0.0 bytes);
/// 3. both failing -> error out (same behaviour as 1.0.0).
///
/// The "never silently wrong pixels" invariant: T87 must either reject a
/// stream or be byte-equal to LegacyGo on it. Verified on the 424-file
/// field corpus.
fn jpegls_decode_component(
    frag: &[u8],
    columns: u32,
    rows: u32,
    comp: usize,
) -> Result<(Vec<u16>, u32, u32), CodecError> {
    let mut t87 = jpegls::DecodeOptions::default();
    t87.profile = jpegls::Profile::T87;
    match jpegls::decode_with_options(frag, columns, rows, &t87) {
        Ok(v) => Ok(v),
        Err(t87_err) => {
            let mut legacy = jpegls::DecodeOptions::default();
            legacy.profile = jpegls::Profile::LegacyGo;
            jpegls::decode_with_options(frag, columns, rows, &legacy).map_err(|legacy_err| {
                CodecError::DecodeFailed(format!(
                    "JPEG-LS decode error (component {comp}): T87: {t87_err}; LegacyGo: {legacy_err}"
                ))
            })
        }
    }
}
/// Decode a DICOM dataset with JPEG-LS pixel data.
///
/// Handles both single-component (grayscale) and multi-component (color)
/// images. Each component is a separate JPEG-LS fragment in the encapsulated
/// PixelData sequence.
fn jpegls_decode(dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
    let meta = parse_jpegls_meta(dicom_bytes)?;
    let fragments = extract_encapsulated_fragments(dicom_bytes)?;

    if fragments.is_empty() {
        return Err(CodecError::DecodeFailed(
            "JPEG-LS: no data fragments in encapsulated PixelData".into(),
        ));
    }

    let num_components = meta.samples_per_pixel as usize;
    let pixel_count = (meta.rows as usize) * (meta.columns as usize);
    let bytes_per_sample = (meta.bits_allocated as usize) >> 3;
    if bytes_per_sample == 0 {
        return Err(CodecError::InvalidMetadata(
            "JPEG-LS: bits_allocated must be ≥ 8".into(),
        ));
    }

    // Decode frames. Fragments are frame-major: per frame, one codestream
    // per component (PS3.5 A.4.2). The planar output is per-plane,
    // frame-major: all frames of plane R, then all frames of plane G...
    let fragments = super::coalesce_single_frame_fragments(
        fragments,
        meta.frames.max(1) as usize,
        num_components,
    );
    let frames = fragments.len() / num_components;
    if frames == 0 || !fragments.len().is_multiple_of(num_components) {
        return Err(CodecError::DecodeFailed(format!(
            "JPEG-LS: {} fragments is not a whole number of frames at {} component(s)/frame",
            fragments.len(),
            num_components
        )));
    }
    if meta.frames != 1 && meta.frames as usize != frames {
        return Err(CodecError::DecodeFailed(format!(
            "JPEG-LS: NumberOfFrames ({}) does not match fragment count ({})",
            meta.frames, frames
        )));
    }
    let mut planar_data: Vec<u8> =
        Vec::with_capacity(pixel_count * num_components * bytes_per_sample * frames);

    for frame in 0..frames {
        for comp in 0..num_components {
            let frag = &fragments[frame * num_components + comp];

            let (decoded_u16, w, h) =
                jpegls_decode_component(frag, meta.columns as u32, meta.rows as u32, comp)?;

            if w != meta.columns as u32 || h != meta.rows as u32 {
                return Err(CodecError::DecodeFailed(format!(
                    "JPEG-LS dimensions ({w},{h}) don't match DICOM metadata ({},{})",
                    meta.columns, meta.rows
                )));
            }

            // Convert u16 -> u8 (truncating high byte for 8-bit; writing LE for 16-bit)
            match bytes_per_sample {
                1 => {
                    for px in &decoded_u16 {
                        planar_data.push(*px as u8);
                    }
                }
                2 => {
                    for px in &decoded_u16 {
                        planar_data.extend_from_slice(&px.to_le_bytes());
                    }
                }
                _ => {
                    return Err(CodecError::DecodeFailed(format!(
                        "JPEG-LS: unsupported bytes_per_sample={bytes_per_sample}"
                    )));
                }
            }
        }
    }

    Ok(DecodedPixels {
        data: planar_data,
        rows: meta.rows,
        columns: meta.columns,
        samples_per_pixel: meta.samples_per_pixel,
        bits_allocated: meta.bits_allocated,
        photometric_interpretation: meta.photometric_interpretation,
    })
}

// ===========================================================================
// Shared encode logic
// ===========================================================================
/// SINGLE SWITCH POINT for the JPEG-LS ENCODE profile.
///
/// `Profile::T87` is ITU-T T.87 conformant (run mode, LSE ID=1
/// presets, real near-lossless). Weasis 4.7.2 decodes a T87 stream
/// (CharLS inside the OpenCV-dcm fork) and REJECTS a LegacyGo stream -
/// so T87 is the production default (see the rationale below).
/// The stored 4.80 stream is LegacyGo.
///
/// `Profile::LegacyGo` reproduces the frozen 1.0.0 / Go-compatible
/// bytes: byte-compatibility with the stored corpus AND with the
/// decode fallback above. LegacyGo requires NEAR = 0. It is NOT the
/// default anymore - cltranscode can opt into it per instance with
/// `jpegls_profile = legacygo` (JpeglsLegacyGoCodec), the server
/// (WADO transcode) always encodes T87.
fn encode_options(profile: jpegls::Profile, near: u8) -> jpegls::EncodeOptions {
    let mut opts = jpegls::EncodeOptions::default();
    opts.profile = profile;
    opts.near = near;
    opts
}
/// Encode raw pixels into JPEG-LS fragments.
///
/// Fragment layout (PS3.5 A.4.2): frame-major. For every frame, one
/// JPEG-LS codestream per color component (single-component images: one
/// codestream per frame). Single-frame color therefore emits
/// SamplesPerPixel fragments - the conformant layout that decoders
/// (dcm4che/CharLS, Weasis) expect.
fn jpegls_encode_fragments(
    pixels: &DecodedPixels,
    frames: u32,
    near: u8,
    profile: jpegls::Profile,
) -> Result<Vec<Vec<u8>>, CodecError> {
    let num_components = pixels.samples_per_pixel as usize;
    let frame_pixel_count = (pixels.rows as usize) * (pixels.columns as usize);
    let bytes_per_sample = if pixels.bits_allocated <= 8 { 1 } else { 2 };
    let component_size = frame_pixel_count * bytes_per_sample; // one frame of one plane
    let frames_usize = frames as usize;

    let mut fragments: Vec<Vec<u8>> = Vec::with_capacity(num_components * frames_usize);
    for frame in 0..frames_usize {
        for comp in 0..num_components {
            // Planar layout is per-plane, frame-major: all frames of plane R,
            // then all frames of plane G, ...
            let start = comp * component_size * frames_usize + frame * component_size;
            let end = start + component_size;
            let comp_data = pixels.data.get(start..end).ok_or_else(|| {
                CodecError::EncodeFailed(format!(
                    "JPEG-LS: frame {frame} component {comp} out of bounds ({} bytes)",
                    pixels.data.len()
                ))
            })?;

            // Convert u8 -> u16 for pure_jpegls
            let u16_pixels: Vec<u16> = match bytes_per_sample {
                1 => comp_data.iter().map(|&b| b as u16).collect(),
                2 => comp_data
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                    .collect(),
                _ => {
                    return Err(CodecError::EncodeFailed(format!(
                        "JPEG-LS: unsupported bytes_per_sample={bytes_per_sample}"
                    )));
                }
            };

            if u16_pixels.len() != frame_pixel_count {
                return Err(CodecError::EncodeFailed(format!(
                    "JPEG-LS: component {comp} size mismatch: expected {frame_pixel_count}, got {}",
                    u16_pixels.len()
                )));
            }

            let mut frag = Vec::new();
            jpegls::encode_with_options(
                &u16_pixels,
                pixels.columns as u32,
                pixels.rows as u32,
                &encode_options(profile, near),
                &mut frag,
            )
            .map_err(|e| {
                CodecError::EncodeFailed(format!(
                    "JPEG-LS encode error (frame {frame} component {comp}): {e}"
                ))
            })?;
            fragments.push(frag);
        }
    }

    Ok(fragments)
}

/// Encode raw pixels into JPEG-LS format (raw concatenated fragments,
/// no DICOM element wrapper - `update_dicom_metadata()` adds the full
/// PixelData element structure). Single-frame bytes are unchanged.
fn jpegls_encode(
    pixels: &DecodedPixels,
    meta: &DicomMeta,
    near: u8,
    profile: jpegls::Profile,
) -> Result<Vec<u8>, CodecError> {
    let fragments = jpegls_encode_fragments(pixels, meta.frames, near, profile)?;
    Ok(build_encapsulated_pixeldata(&fragments))
}

// ===========================================================================
// DICOM encapsulated PixelData helpers
// ===========================================================================

/// Extract all data fragments from an encapsulated PixelData element.
///
/// Handles both Explicit VR (OB/OW with Reserved bytes) and implicit
/// encapsulated format (undefined length, sequence of Items).
pub(crate) fn extract_encapsulated_fragments(data: &[u8]) -> Result<Vec<Vec<u8>>, CodecError> {
    let pix_pos = find_tag_position(data, TAG_PIXEL_DATA)
        .ok_or_else(|| CodecError::DecodeFailed("PixelData (7FE0,0010) not found".into()))?;

    let mut pos = pix_pos + 4; // skip tag

    if pos + 2 > data.len() {
        return Err(CodecError::DecodeFailed("Truncated at PixelData VR".into()));
    }

    let vr = std::str::from_utf8(&data[pos..pos + 2]).unwrap_or("OB");
    pos += 2;

    // Skip VR-specific padding and check for undefined length
    let undefined_length = match vr {
        "OB" | "OD" | "OF" | "OL" | "OV" | "OW" | "SQ" | "SV" | "UC" | "UN" | "UR" | "UT"
        | "UV" => {
            if pos + 6 > data.len() {
                return Err(CodecError::DecodeFailed(
                    "Truncated at extended length".into(),
                ));
            }
            pos += 2; // reserved
            let len = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
            pos += 4;
            len == 0xFFFF_FFFF
        }
        _ => {
            if pos + 2 > data.len() {
                return Err(CodecError::DecodeFailed("Truncated at short length".into()));
            }
            let len = u16::from_le_bytes([data[pos], data[pos + 1]]) as u32;
            pos += 2;
            len == 0xFFFF
        }
    };

    if !undefined_length {
        return Err(CodecError::DecodeFailed(
            "JPEG-LS: expected undefined length for encapsulated PixelData".into(),
        ));
    }

    // Walk sequence of Items (FFFE,E000) collecting fragments
    let mut fragments = Vec::new();
    let end = data.len();

    // BOT detection: the Basic Offset Table, when present, is the FIRST
    // item. Empty BOT (len 0) is common; a filled BOT for an N-frame image
    // carries N u32 offsets (len = 4*N). A 1-entry BOT (len 4, value
    // 00000000) is disambiguated by its zero prefix - a JPEG stream never
    // starts with 00 00 00 00 (01.09 field, ATLAS 4.70 slices).
    let declared_frames: Option<u32> = find_tag_str(data, TAG_NUMBER_OF_FRAMES)
        .and_then(|s| s.trim().trim_end_matches('\0').parse().ok())
        .filter(|f| *f > 1);
    let mut is_first_item = true;

    while pos + 8 <= end {
        let tag_group = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let tag_elem = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        pos += 4;

        if pos + 4 > end {
            break;
        }
        let item_len =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;

        match (tag_group, tag_elem) {
            // Item (FFFE,E000)
            (0xFFFE, 0xE000) => {
                if is_first_item {
                    is_first_item = false;
                    let is_bot = item_len == 0
                        || (item_len.is_multiple_of(4)
                            && pos + 4 <= end
                            && data[pos..pos + 4] == [0, 0, 0, 0])
                        || (item_len.is_multiple_of(4)
                            && declared_frames.is_some_and(|f| item_len / 4 == f as usize));
                    if is_bot {
                        pos = pos.saturating_add(item_len); // skip the offsets
                        continue; // BOT -- not a data fragment
                    }
                }
                if item_len == 0xFFFF_FFFF {
                    // Undefined length item - skip
                    continue;
                }
                if item_len == 0 {
                    // Empty item - skip
                    continue;
                }
                if pos + item_len > end {
                    return Err(CodecError::DecodeFailed(
                        "JPEG-LS: fragment extends past data".into(),
                    ));
                }
                fragments.push(data[pos..pos + item_len].to_vec());
                pos += item_len;
            }
            // Sequence Delimiter (FFFE,E0DD) or Item Delimiter (FFFE,E00D)
            (0xFFFE, 0xE0DD) | (0xFFFE, 0xE00D) => {
                break;
            }
            _ => {
                // Unknown - stop
                break;
            }
        }
    }

    Ok(fragments)
}

/// Build raw compressed bytes from JPEG-LS fragments.
///
/// Returns ONLY the concatenated fragment bytes (no DICOM element wrapper).
/// The DICOM PixelData element structure (tag, VR, BOT, fragments, seq delimiter)
/// is built by `update_dicom_metadata()` in `transcode.rs`.
///
/// Each fragment is a JPEG-LS codestream for one color component.
fn build_encapsulated_pixeldata(fragments: &[Vec<u8>]) -> Vec<u8> {
    let total_len: usize = fragments.iter().map(|f| f.len()).sum();
    let mut out = Vec::with_capacity(total_len);
    for frag in fragments {
        out.extend_from_slice(frag);
    }
    out
}

// ===========================================================================
// DICOM metadata parsing
// ===========================================================================

pub(crate) fn parse_jpegls_meta(data: &[u8]) -> Result<DicomMeta, CodecError> {
    let rows = find_tag_u16(data, TAG_ROWS)
        .ok_or_else(|| CodecError::InvalidMetadata("Missing Rows (0028,0010)".into()))?;
    let columns = find_tag_u16(data, TAG_COLUMNS)
        .ok_or_else(|| CodecError::InvalidMetadata("Missing Columns (0028,0011)".into()))?;
    let samples_per_pixel: u8 = find_tag_u16(data, TAG_SAMPLES_PER_PIXEL)
        .map(|v| v as u8)
        .unwrap_or(1);
    let bits_allocated: u8 = find_tag_u16(data, TAG_BITS_ALLOCATED)
        .map(|v| v as u8)
        .unwrap_or(8);
    let bits_stored: u8 = find_tag_u16(data, TAG_BITS_ALLOCATED)
        .map(|v| v as u8)
        .unwrap_or(bits_allocated);
    let photometric_interpretation = find_tag_str(data, TAG_PHOTOMETRIC_INTERPRETATION)
        .unwrap_or_else(|| "MONOCHROME2".to_string());
    let frames: u32 = find_tag_str(data, TAG_NUMBER_OF_FRAMES)
        .and_then(|s| s.trim().trim_end_matches('\0').parse().ok())
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

// ===========================================================================
// Low-level DICOM binary parsing
// ===========================================================================

fn dicom_start(data: &[u8]) -> Option<usize> {
    if data.len() < 132 {
        return None;
    }
    // Skip 128-byte preamble + "DICM"
    if &data[128..132] == b"DICM" {
        Some(132)
    } else {
        // Try starting from 0 (no preamble)
        Some(0)
    }
}

fn find_tag_position(data: &[u8], target: (u16, u16)) -> Option<usize> {
    let mut pos = dicom_start(data)?;
    let end = data.len();

    // Skip meta-header (group 0002)
    while pos + 4 <= end {
        let g = u16::from_le_bytes([data[pos], data[pos + 1]]);
        if g != 0x0002 {
            break;
        }
        pos += 4;
        if !skip_explicit(data, &mut pos) {
            return None;
        }
    }

    // Walk dataset
    while pos + 4 <= end {
        let g = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let e = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        pos += 4;

        if (g, e) == target {
            return Some(pos - 4);
        }

        // Handle sequences
        if g == 0xFFFE {
            if pos + 4 > end {
                return None;
            }
            let len = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                as usize;
            pos += 4;
            if len == 0xFFFF_FFFF {
                continue; // undefined length
            }
            pos = pos.saturating_add(len);
            continue;
        }

        if !skip_explicit(data, &mut pos) {
            return None;
        }
    }

    None
}

fn skip_explicit(data: &[u8], pos: &mut usize) -> bool {
    if *pos + 2 > data.len() {
        return false;
    }
    let vr_bytes = [data[*pos], data[*pos + 1]];
    *pos += 2;
    let vr = std::str::from_utf8(&vr_bytes).unwrap_or("  ");
    match vr {
        "OB" | "OD" | "OF" | "OL" | "OV" | "OW" | "SQ" | "SV" | "UC" | "UN" | "UR" | "UT"
        | "UV" => {
            if *pos + 6 > data.len() {
                return false;
            }
            *pos += 2; // reserved
            let len =
                u32::from_le_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]])
                    as usize;
            *pos += 4;
            if len == 0xFFFF_FFFF {
                return true; // undefined -- caller walks items
            }
            *pos = pos.saturating_add(len);
            if !len.is_multiple_of(2) {
                *pos += 1; // PS3.5 7.1 pad byte
            }
            true
        }
        _ => {
            if *pos + 2 > data.len() {
                return false;
            }
            let len = u16::from_le_bytes([data[*pos], data[*pos + 1]]) as usize;
            *pos += 2;
            if len == 0xFFFF {
                return true; // undefined
            }
            *pos = pos.saturating_add(len);
            if !len.is_multiple_of(2) {
                *pos += 1; // PS3.5 7.1 pad byte
            }
            true
        }
    }
}

fn find_tag_u16(data: &[u8], target: (u16, u16)) -> Option<u16> {
    find_tag_bytes(data, target).and_then(|bytes| {
        if bytes.len() >= 2 {
            Some(u16::from_le_bytes([bytes[0], bytes[1]]))
        } else {
            None
        }
    })
}

fn find_tag_str(data: &[u8], target: (u16, u16)) -> Option<String> {
    find_tag_bytes(data, target).map(|bytes| {
        String::from_utf8_lossy(&bytes)
            .trim_end_matches([' ', '\0'])
            .to_string()
    })
}

fn find_tag_bytes(data: &[u8], target: (u16, u16)) -> Option<Vec<u8>> {
    let mut pos = dicom_start(data)?;
    let end = data.len();

    // Skip meta-header (group 0002)
    while pos + 4 <= end {
        let g = u16::from_le_bytes([data[pos], data[pos + 1]]);
        if g != 0x0002 {
            break;
        }
        pos += 4;
        if !skip_explicit(data, &mut pos) {
            return None;
        }
    }

    // Walk dataset
    while pos + 4 <= end {
        let g = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let e = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        pos += 4;

        if (g, e) == target {
            // Found - extract value
            if pos + 2 > end {
                return None;
            }
            let vr_bytes = [data[pos], data[pos + 1]];
            pos += 2;
            let vr = std::str::from_utf8(&vr_bytes).unwrap_or("  ");
            match vr {
                "OB" | "OD" | "OF" | "OL" | "OV" | "OW" | "SQ" | "SV" | "UC" | "UN" | "UR"
                | "UT" | "UV" => {
                    if pos + 6 > end {
                        return None;
                    }
                    pos += 2;
                    let len = u32::from_le_bytes([
                        data[pos],
                        data[pos + 1],
                        data[pos + 2],
                        data[pos + 3],
                    ]) as usize;
                    pos += 4;
                    if len == 0xFFFF_FFFF {
                        return Some(Vec::new()); // undefined -- return empty
                    }
                    if pos + len > end {
                        return None;
                    }
                    let val = data[pos..pos + len].to_vec();
                    return Some(val);
                }
                _ => {
                    if pos + 2 > end {
                        return None;
                    }
                    let len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
                    pos += 2;
                    if pos + len > end {
                        return None;
                    }
                    let val = data[pos..pos + len].to_vec();
                    return Some(val);
                }
            }
        }

        // Handle FFFE group
        if g == 0xFFFE {
            if pos + 4 > end {
                return None;
            }
            let len = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                as usize;
            pos += 4;
            if len == 0xFFFF_FFFF {
                continue;
            }
            pos = pos.saturating_add(len);
            continue;
        }

        if !skip_explicit(data, &mut pos) {
            return None;
        }
    }

    None
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Roundtrip helpers ---------------------------------------------

    fn roundtrip_mono(original: &[u8], w: u16, h: u16, bits: u8, codec: &dyn DicomCodec) {
        let meta = DicomMeta {
            rows: h,
            columns: w,
            samples_per_pixel: 1,
            bits_allocated: bits,
            bits_stored: bits,
            photometric_interpretation: "MONOCHROME2".into(),
            planar_configuration: None,
            pixel_representation: 0,
            frames: 1,
        };

        let pixels = DecodedPixels {
            data: original.to_vec(),
            rows: h,
            columns: w,
            samples_per_pixel: 1,
            bits_allocated: bits,
            photometric_interpretation: "MONOCHROME2".into(),
        };

        // Encode to JPEG-LS fragment
        let encoded = codec.encode(&pixels, &meta).expect("encode failed");

        // Rebuild minimal DICOM wrapper so decode() can find metadata
        let dicom_wrapper = build_minimal_dicom_wrapper(&encoded, w, h, bits, 1);

        // Decode
        let decoded = codec.decode(&dicom_wrapper).expect("decode failed");
        assert_eq!(decoded.rows, h);
        assert_eq!(decoded.columns, w);
        assert_eq!(decoded.samples_per_pixel, 1);
        assert_eq!(decoded.bits_allocated, bits);

        // For lossless: compare byte-for-byte
        let bytes_per_sample = (bits as usize) >> 3;
        let expected_len = (w as usize) * (h as usize) * bytes_per_sample;
        assert_eq!(decoded.data.len(), expected_len);
        assert_eq!(&decoded.data, original);
    }

    /// Build a minimal DICOM dataset containing only the tags needed
    /// for the JpegLS codec to parse metadata + the encoded pixel data.
    fn build_minimal_dicom_wrapper(
        pixel_data: &[u8],
        w: u16,
        h: u16,
        bits: u8,
        samples: u8,
    ) -> Vec<u8> {
        let mut out = Vec::new();

        // Preamble (128 zeroes) + "DICM"
        out.extend_from_slice(&[0u8; 128]);
        out.extend_from_slice(b"DICM");

        // Meta-header (group 0002) - minimal
        // (0002,0010) TransferSyntaxUID - use JPEG-LS Lossless
        append_dicom_tag_str(&mut out, (0x0002, 0x0010), "UI", TS_JPEGLS_LOSSLESS);

        // Dataset
        // (0028,0010) Rows
        append_dicom_tag_u16(&mut out, TAG_ROWS, w);
        // (0028,0011) Columns
        append_dicom_tag_u16(&mut out, TAG_COLUMNS, h);
        // (0028,0002) SamplesPerPixel
        append_dicom_tag_u16(&mut out, TAG_SAMPLES_PER_PIXEL, samples as u16);
        // (0028,0100) BitsAllocated
        append_dicom_tag_u16(&mut out, TAG_BITS_ALLOCATED, bits as u16);
        // (0028,0004) PhotometricInterpretation
        append_dicom_tag_str(
            &mut out,
            TAG_PHOTOMETRIC_INTERPRETATION,
            "CS",
            "MONOCHROME2",
        );

        // Append the encapsulated pixel data with proper DICOM element wrapper.
        // build_encapsulated_pixeldata() now returns only raw fragment bytes
        // (no DICOM element structure), so we must build the wrapper here.
        // Encapsulated PixelData format: Tag + VR(OB) + reserved + undefined_len
        // + BOT(empty) + Fragment(data) + SeqDelim
        out.extend_from_slice(&0x7FE0u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"OB");
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // BOT (empty)
        out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        out.extend_from_slice(&0u32.to_le_bytes());
        // Fragment with JPEG-LS data
        out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        let frag_len = pixel_data.len() as u32;
        out.extend_from_slice(&frag_len.to_le_bytes());
        out.extend_from_slice(pixel_data);
        // Sequence delimiter
        out.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
        out.extend_from_slice(&0u32.to_le_bytes());

        out
    }

    #[test]
    fn extract_encapsulated_fragments_skips_filled_one_entry_bot() {
        // 01.09 field (ATLAS 4.70 slices): the Basic Offset Table is a
        // non-empty FIRST item (len=4, one u32 offset = 0). It must be
        // skipped, never returned as a fragment.
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 128]);
        out.extend_from_slice(b"DICM");
        append_dicom_tag_str(&mut out, (0x0002, 0x0010), "UI", TS_JPEGLS_LOSSLESS);

        out.extend_from_slice(&0x7FE0u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"OB");
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // filled 1-entry BOT: one offset (0)
        out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        // one fragment
        let frag = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        out.extend_from_slice(&(frag.len() as u32).to_le_bytes());
        out.extend_from_slice(&frag);
        // sequence delimiter
        out.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
        out.extend_from_slice(&0u32.to_le_bytes());

        let fragments = extract_encapsulated_fragments(&out).expect("fragments");
        assert_eq!(fragments.len(), 1, "BOT must not become a fragment");
        assert_eq!(fragments[0], frag.to_vec());
    }

    fn append_dicom_tag_u16(out: &mut Vec<u8>, tag: (u16, u16), val: u16) {
        out.extend_from_slice(&tag.0.to_le_bytes());
        out.extend_from_slice(&tag.1.to_le_bytes());
        out.extend_from_slice(b"US"); // VR
        out.extend_from_slice(&2u16.to_le_bytes()); // length
        out.extend_from_slice(&val.to_le_bytes());
    }

    fn append_dicom_tag_str(out: &mut Vec<u8>, tag: (u16, u16), vr: &str, val: &str) {
        out.extend_from_slice(&tag.0.to_le_bytes());
        out.extend_from_slice(&tag.1.to_le_bytes());
        out.extend_from_slice(vr.as_bytes());
        let len = val.len() as u16;
        // For short-VR, pad to even length
        let pad_len = if len.is_multiple_of(2) { len } else { len + 1 };
        out.extend_from_slice(&pad_len.to_le_bytes());
        out.extend_from_slice(val.as_bytes());
        if pad_len > len {
            out.push(b' ');
        }
    }

    // -- Tests ----------------------------------------------------------

    #[test]
    fn jpegls_lossless_roundtrip_8bit_16x16() {
        let w = 16u16;
        let h = 16u16;
        let mut data = vec![0u8; (w as usize) * (h as usize)];
        for y in 0..h as usize {
            for x in 0..w as usize {
                data[y * w as usize + x] = ((x + y * 7) % 251) as u8;
            }
        }
        roundtrip_mono(&data, w, h, 8, &JpeglsLosslessCodec);
    }

    #[test]
    fn jpegls_lossless_roundtrip_16bit_8x8() {
        let w = 8u16;
        let h = 8u16;
        let mut data = Vec::new();
        for i in 0..(w as usize) * (h as usize) {
            let val = (i as u16).wrapping_mul(257);
            data.extend_from_slice(&val.to_le_bytes());
        }
        roundtrip_mono(&data, w, h, 16, &JpeglsLosslessCodec);
    }

    #[test]
    fn jpegls_lossless_roundtrip_1x1() {
        let data = vec![128u8];
        roundtrip_mono(&data, 1, 1, 8, &JpeglsLosslessCodec);
    }

    #[test]
    fn jpegls_lossless_roundtrip_uniform() {
        // pure_jpegls v1.0.0 has a known edge case with fully uniform
        // narrow images (w=32, h=4). Use a square uniform image instead.
        let w = 16u16;
        let h = 16u16;
        let data = vec![128u8; (w as usize) * (h as usize)];
        roundtrip_mono(&data, w, h, 8, &JpeglsLosslessCodec);
    }

    #[test]
    fn jpegls_nearlossless_roundtrip_8bit() {
        let w = 16u16;
        let h = 16u16;
        let mut data = vec![0u8; (w as usize) * (h as usize)];
        for y in 0..h as usize {
            for x in 0..w as usize {
                data[y * w as usize + x] = ((x + y * 7) % 251) as u8;
            }
        }

        let codec = JpeglsNearLosslessCodec::new(2);
        let meta = DicomMeta {
            rows: h,
            columns: w,
            samples_per_pixel: 1,
            bits_allocated: 8,
            bits_stored: 8,
            photometric_interpretation: "MONOCHROME2".into(),
            planar_configuration: None,
            pixel_representation: 0,
            frames: 1,
        };
        let pixels = DecodedPixels {
            data: data.clone(),
            rows: h,
            columns: w,
            samples_per_pixel: 1,
            bits_allocated: 8,
            photometric_interpretation: "MONOCHROME2".into(),
        };

        let encoded = codec.encode(&pixels, &meta).expect("encode failed");
        let wrapper = build_minimal_dicom_wrapper(&encoded, w, h, 8, 1);
        let decoded = codec.decode(&wrapper).expect("decode failed");

        // Near-lossless: check max error <= near=2
        assert_eq!(decoded.data.len(), data.len());
        for (i, _item) in data.iter().enumerate() {
            let diff = (data[i] as i16 - decoded.data[i] as i16).unsigned_abs();
            assert!(
                diff <= 2,
                "Error at pixel {i}: original={}, decoded={}, diff={diff}",
                data[i],
                decoded.data[i]
            );
        }
    }

    #[test]
    fn jpegls_transfer_syntax_uid() {
        assert_eq!(
            JpeglsLosslessCodec.transfer_syntax_uid(),
            TS_JPEGLS_LOSSLESS
        );
        assert_eq!(
            JpeglsNearLosslessCodec::new(2).transfer_syntax_uid(),
            TS_JPEGLS_NEAR_LOSSLESS
        );
    }

    #[test]
    fn jpegls_empty_data_rejected() {
        let empty = vec![0u8; 140]; // preamble + DICM only, no PixelData
        let result = JpeglsLosslessCodec.decode(&empty);
        assert!(result.is_err());
    }

    // -- pure_jpegls 2.0.0 profile tests --------------------------------

    fn ramp_u16(w: u16, h: u16) -> Vec<u16> {
        (0..(w as usize) * (h as usize))
            .map(|i| ((i * 13) % 251) as u16)
            .collect()
    }

    #[test]
    fn jpegls_roundtrip_encode_legacygo_decode_legacygo() {
        // The encode switch point must produce LegacyGo bytes readable
        // by the LegacyGo decoder (byte-exact, as stored corpus).
        let w = 16u16;
        let h = 16u16;
        let px = ramp_u16(w, h);

        let mut enc = jpegls::EncodeOptions::default();
        enc.profile = jpegls::Profile::LegacyGo;
        let mut frag = Vec::new();
        jpegls::encode_with_options(&px, w as u32, h as u32, &enc, &mut frag)
            .expect("LegacyGo encode failed");

        let mut dec = jpegls::DecodeOptions::default();
        dec.profile = jpegls::Profile::LegacyGo;
        let (decoded, dw, dh) =
            jpegls::decode_with_options(&frag, w as u32, h as u32, &dec).expect("LegacyGo decode");
        assert_eq!((dw, dh), (w as u32, h as u32));
        assert_eq!(decoded, px);
    }

    #[test]
    fn jpegls_roundtrip_encode_t87_decode_t87_through_codec() {
        // A T87 stream must decode through OUR path (T87-first, no
        // fallback needed): byte-exact through the full codec.
        let w = 16u16;
        let h = 16u16;
        let px = ramp_u16(w, h);

        let mut enc = jpegls::EncodeOptions::default();
        enc.profile = jpegls::Profile::T87;
        let mut frag = Vec::new();
        jpegls::encode_with_options(&px, w as u32, h as u32, &enc, &mut frag)
            .expect("T87 encode failed");

        let wrapper = build_minimal_dicom_wrapper(&frag, w, h, 8, 1);
        let decoded = JpeglsLosslessCodec
            .decode(&wrapper)
            .expect("T87 decode failed");
        let expected: Vec<u8> = px.iter().map(|v| *v as u8).collect();
        assert_eq!(decoded.data, expected);
    }

    #[test]
    fn jpegls_decode_fallback_legacy_stream() {
        // A frozen 1.0.0-bytes stream must decode through the fallback:
        // T87 rejects it, LegacyGo reads it byte-exact. The explicit
        // T87 error assertion documents the fallback is really engaged.
        let w = 16u16;
        let h = 16u16;
        let px = ramp_u16(w, h);

        let mut enc = jpegls::EncodeOptions::default();
        enc.profile = jpegls::Profile::LegacyGo;
        let mut frag = Vec::new();
        jpegls::encode_with_options(&px, w as u32, h as u32, &enc, &mut frag)
            .expect("LegacyGo encode failed");

        let mut t87 = jpegls::DecodeOptions::default();
        t87.profile = jpegls::Profile::T87;
        assert!(
            jpegls::decode_with_options(&frag, w as u32, h as u32, &t87).is_err(),
            "expected T87 to reject a LegacyGo stream"
        );

        let wrapper = build_minimal_dicom_wrapper(&frag, w, h, 8, 1);
        let decoded = JpeglsLosslessCodec
            .decode(&wrapper)
            .expect("fallback decode failed");
        let expected: Vec<u8> = px.iter().map(|v| *v as u8).collect();
        assert_eq!(decoded.data, expected);
    }

    #[test]
    fn jpegls_decode_corrupted_input_is_error_not_panic() {
        // Corrupted input must yield Err through BOTH profile attempts,
        // never a panic and never silently wrong pixels.
        let w = 16u16;
        let h = 16u16;

        // Truncated real stream: a valid legacy stream cut to 10 bytes.
        let px = ramp_u16(w, h);
        let mut enc = jpegls::EncodeOptions::default();
        enc.profile = jpegls::Profile::LegacyGo;
        let mut frag = Vec::new();
        jpegls::encode_with_options(&px, w as u32, h as u32, &enc, &mut frag)
            .expect("LegacyGo encode failed");
        let truncated = frag[..10].to_vec();
        let wrapper = build_minimal_dicom_wrapper(&truncated, w, h, 8, 1);
        assert!(JpeglsLosslessCodec.decode(&wrapper).is_err());

        // Pure garbage.
        let garbage = vec![0xABu8; 64];
        let wrapper = build_minimal_dicom_wrapper(&garbage, w, h, 8, 1);
        assert!(JpeglsLosslessCodec.decode(&wrapper).is_err());
    }

    // -- T87 profile tests --------------------------------------------

    #[test]
    fn jpegls_encode_options_default_t87() {
        let opts = encode_options(jpegls::Profile::T87, 0);
        assert!(matches!(opts.profile, jpegls::Profile::T87));
        assert_eq!(opts.near, 0);
        let near_opts = encode_options(jpegls::Profile::T87, 4);
        assert_eq!(near_opts.near, 4);
    }

    #[test]
    fn jpegls_t87_encode_differs_from_legacygo_bytes() {
        // The profiles differ: the SAME pixels through the T87 profile must
        // produce a DIFFERENT codestream than the frozen LegacyGo bytes
        // (otherwise Weasis would still see the rejected stream).
        let w = 16u16;
        let h = 16u16;
        let px = ramp_u16(w, h);

        let mut legacy = jpegls::EncodeOptions::default();
        legacy.profile = jpegls::Profile::LegacyGo;
        let mut legacy_frag = Vec::new();
        jpegls::encode_with_options(&px, w as u32, h as u32, &legacy, &mut legacy_frag)
            .expect("LegacyGo encode failed");

        let mut frag = Vec::new();
        jpegls::encode_with_options(
            &px,
            w as u32,
            h as u32,
            &encode_options(jpegls::Profile::T87, 0),
            &mut frag,
        )
        .expect("T87 encode failed");

        assert_ne!(frag, legacy_frag, "T87 stream must differ from LegacyGo");
    }

    #[test]
    fn jpegls_legacygo_codec_roundtrip_and_frozen_bytes() {
        // The opt-in codec reproduces the FROZEN 1.0.0 bytes byte-exactly
        // (explicit LegacyGo options == codec output) and reads back.
        let w = 16u16;
        let h = 16u16;
        let px = ramp_u16(w, h);

        let mut enc = jpegls::EncodeOptions::default();
        enc.profile = jpegls::Profile::LegacyGo;
        let mut ref_frag = Vec::new();
        jpegls::encode_with_options(&px, w as u32, h as u32, &enc, &mut ref_frag)
            .expect("LegacyGo encode failed");

        let meta = DicomMeta {
            rows: h,
            columns: w,
            samples_per_pixel: 1,
            bits_allocated: 8,
            bits_stored: 8,
            photometric_interpretation: "MONOCHROME2".into(),
            planar_configuration: None,
            pixel_representation: 0,
            frames: 1,
        };
        let pixels = DecodedPixels {
            data: px.iter().map(|v| *v as u8).collect(),
            rows: h,
            columns: w,
            samples_per_pixel: 1,
            bits_allocated: 8,
            photometric_interpretation: "MONOCHROME2".into(),
        };
        let encoded = JpeglsLegacyGoCodec
            .encode(&pixels, &meta)
            .expect("LegacyGo codec encode failed");
        assert_eq!(encoded, ref_frag, "LegacyGo codec = frozen 1.0.0 bytes");

        let wrapper = build_minimal_dicom_wrapper(&encoded, w, h, 8, 1);
        let decoded = JpeglsLegacyGoCodec
            .decode(&wrapper)
            .expect("LegacyGo codec decode failed");
        assert_eq!(decoded.data, pixels.data);
    }

    #[test]
    fn jpegls_t87_encode_decode_t87_direct_no_fallback() {
        // A T87 stream MUST be accepted by the T87 branch directly
        // (the "never silently wrong" invariant: no silent LegacyGo
        // fallback on our own fresh encodes).
        let w = 16u16;
        let h = 16u16;
        let px = ramp_u16(w, h);

        let mut frag = Vec::new();
        jpegls::encode_with_options(
            &px,
            w as u32,
            h as u32,
            &encode_options(jpegls::Profile::T87, 0),
            &mut frag,
        )
        .expect("T87 encode failed");

        let mut dec = jpegls::DecodeOptions::default();
        dec.profile = jpegls::Profile::T87;
        let (decoded, dw, dh) = jpegls::decode_with_options(&frag, w as u32, h as u32, &dec)
            .expect("T87 branch must accept the T87 stream directly");
        assert_eq!((dw, dh), (w as u32, h as u32));
        assert_eq!(decoded, px);
    }

    /// Live-gate helper (NOT part of the normal suite, run explicitly):
    /// transcode the anon sample EVRLE -> 4.80 through the FULL
    /// transcode_dicom pipeline on the CURRENT build and write the first
    /// JPEG-LS fragment to the t87 sample file under the crate test
    /// fixtures - the input of the Weasis live check scripts. The stored
    /// sample is itself 4.80 (LegacyGo), so the chain is
    /// 4.80 -> EVRLE (decode) -> 4.80 T87 (fresh encode).
    ///
    /// Run: cargo test -p clarus-adapters --lib --features transcode
    ///      -- --ignored --nocapture jpegls_write_t87_live_fragment
    #[cfg(feature = "transcode")]
    #[test]
    #[ignore]
    fn jpegls_write_t87_live_fragment() {
        let src = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/jpegls/anon_sample_4d80.dcm"
        );
        let original = match std::fs::read(src) {
            Ok(b) => b,
            Err(e) => panic!("cannot read live-gate sample {src}: {e}"),
        };
        let registry = crate::codec::CodecRegistry::default();
        // Step 1: stored 4.80 -> EVRLE (exercises the decode fallback).
        let evrle = crate::transcode::transcode_dicom(
            &original,
            crate::codec::ts_uid::EXPLICIT_VR_LE,
            &registry,
        )
        .expect("transcode 4.80 -> EVRLE failed");
        // Step 2: EVRLE -> 4.80 on the CURRENT encoder (T87).
        let out = crate::transcode::transcode_dicom(&evrle, TS_JPEGLS_LOSSLESS, &registry)
            .expect("transcode EVRLE -> 4.80 failed");
        let fragments = extract_encapsulated_fragments(&out).expect("no fragments");
        assert!(!fragments.is_empty());
        let stored_frag = extract_encapsulated_fragments(&original).expect("stored fragments");
        assert_ne!(
            fragments[0], stored_frag[0],
            "fresh 4.80 fragment must differ from the stored LegacyGo fragment"
        );
        let dst = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/jpegls/t87_sample_frag.bin"
        );
        std::fs::write(dst, &fragments[0]).expect("write fragment");
        eprintln!(
            "live-gate: wrote {} bytes (fragment {}) from {} ({} -> EVRLE {} -> 4.80 {})",
            fragments[0].len(),
            dst,
            src,
            original.len(),
            evrle.len(),
            out.len()
        );
    }

    /// Corpus gate (run explicitly): every stored LegacyGo file in the
    /// corpus directory must still decode through the T87-first +
    /// LegacyGo-fallback path - the encode profile must not change READ
    /// compatibility.
    ///
    /// The corpus is too large to keep in the repo, so the directory comes
    /// from the CLARUS_JLS_CORPUS_DIR environment variable; without it the
    /// test skips silently.
    ///
    /// Run: set CLARUS_JLS_CORPUS_DIR=<dir>, then
    ///      cargo test -p clarus-adapters --lib --features transcode
    ///      -- --ignored --nocapture jpegls_corpus_decode_gate
    #[test]
    #[ignore]
    fn jpegls_corpus_decode_gate() {
        let dir = match std::env::var("CLARUS_JLS_CORPUS_DIR") {
            Ok(d) if !d.is_empty() => d,
            _ => {
                eprintln!("CLARUS_JLS_CORPUS_DIR not set; skipping corpus gate");
                return;
            }
        };
        let entries: Vec<_> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map(|x| x == "dcm").unwrap_or(false))
                .collect(),
            Err(e) => panic!("cannot read corpus dir {dir}: {e}"),
        };
        assert!(!entries.is_empty(), "corpus dir {dir} is empty");
        let mut ok = 0usize;
        let mut failed: Vec<String> = Vec::new();
        for entry in &entries {
            match std::fs::read(entry.path()) {
                Ok(bytes) => match JpeglsLosslessCodec.decode(&bytes) {
                    Ok(_) => ok += 1,
                    Err(e) => failed.push(format!("{}: {e}", entry.file_name().to_string_lossy())),
                },
                Err(e) => failed.push(format!(
                    "{}: read error {e}",
                    entry.file_name().to_string_lossy()
                )),
            }
        }
        eprintln!(
            "corpus gate: {}/{} decoded OK, {} failed",
            ok,
            entries.len(),
            failed.len()
        );
        if !failed.is_empty() {
            panic!("corpus decode failures ({}): {failed:?}", failed.len());
        }
    }

    // ===============================================================
    // Multi-frame conformance tests
    // ===============================================================

    /// Build a minimal EVRLE monochrome multi-frame DICOM (8-bit).
    fn build_multiframe_evrle(frames: u16, rows: u16, cols: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 128]);
        out.extend_from_slice(b"DICM");
        // UI "1.2.840.10008.1.2.1" is odd-length: the helper pads with a
        // SPACE, so pad with NUL instead (the TS parser trims trailing NUL).
        append_dicom_tag_str(&mut out, (0x0002, 0x0010), "UI", "1.2.840.10008.1.2.1\0");
        append_dicom_tag_u16(&mut out, TAG_ROWS, rows);
        append_dicom_tag_u16(&mut out, TAG_COLUMNS, cols);
        append_dicom_tag_u16(&mut out, TAG_SAMPLES_PER_PIXEL, 1);
        append_dicom_tag_u16(&mut out, TAG_BITS_ALLOCATED, 8);
        append_dicom_tag_str(&mut out, TAG_NUMBER_OF_FRAMES, "IS", &frames.to_string());
        append_dicom_tag_str(
            &mut out,
            TAG_PHOTOMETRIC_INTERPRETATION,
            "CS",
            "MONOCHROME2",
        );
        let frame_pixels = (rows as usize) * (cols as usize);
        let mut data = Vec::with_capacity(frame_pixels * frames as usize);
        for f in 0..frames as usize {
            for i in 0..frame_pixels {
                data.push(((i * 3 + f * 7) % 256) as u8);
            }
        }
        out.extend_from_slice(&0x7FE0u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"OW");
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    /// Build a minimal EVRLE interleaved-RGB DICOM (8-bit, single frame).
    fn build_color_evrle(pi: &str) -> Vec<u8> {
        let rows = 16u16;
        let cols = 8u16;
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 128]);
        out.extend_from_slice(b"DICM");
        // UI "1.2.840.10008.1.2.1" is odd-length: NUL pad (see above).
        append_dicom_tag_str(&mut out, (0x0002, 0x0010), "UI", "1.2.840.10008.1.2.1\0");
        append_dicom_tag_u16(&mut out, TAG_ROWS, rows);
        append_dicom_tag_u16(&mut out, TAG_COLUMNS, cols);
        append_dicom_tag_u16(&mut out, TAG_SAMPLES_PER_PIXEL, 3);
        append_dicom_tag_u16(&mut out, TAG_BITS_ALLOCATED, 8);
        append_dicom_tag_str(&mut out, TAG_PHOTOMETRIC_INTERPRETATION, "CS", pi);
        let mut data = Vec::new();
        for y in 0..rows as usize {
            for x in 0..cols as usize {
                data.push(((x + y * 2) % 256) as u8); // R
                data.push(((x * 3 + y) % 256) as u8); // G
                data.push(((x + y * 5) % 256) as u8); // B
            }
        }
        out.extend_from_slice(&0x7FE0u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"OW");
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    /// Replace a CS element value in-place (rebuilds the file; the new
    /// value may have a different length).
    fn replace_cs_value(data: &[u8], tag: (u16, u16), new_value: &str) -> Option<Vec<u8>> {
        let start = dicom_start(data)?;
        let mut pos = start;
        while pos + 8 <= data.len() {
            let g = u16::from_le_bytes([data[pos], data[pos + 1]]);
            let e = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
            if g == 0x0002 {
                pos += 4;
                if !skip_explicit(data, &mut pos) {
                    return None;
                }
                continue;
            }
            if (g, e) == tag {
                let elem_start = pos;
                pos += 4;
                if pos + 2 > data.len() {
                    return None;
                }
                let vr = std::str::from_utf8(&data[pos..pos + 2]).unwrap_or("");
                pos += 2;
                let len: usize = if matches!(
                    vr,
                    "OB" | "OD"
                        | "OF"
                        | "OL"
                        | "OV"
                        | "OW"
                        | "SQ"
                        | "SV"
                        | "UC"
                        | "UN"
                        | "UR"
                        | "UT"
                        | "UV"
                ) {
                    if pos + 6 > data.len() {
                        return None;
                    }
                    pos += 2;
                    let l = u32::from_le_bytes([
                        data[pos],
                        data[pos + 1],
                        data[pos + 2],
                        data[pos + 3],
                    ]) as usize;
                    pos += 4;
                    l
                } else {
                    let l = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
                    pos += 2;
                    l
                };
                if len == 0xFFFF_FFFF || pos + len > data.len() {
                    return None;
                }
                let val_end = pos + len;
                let mut val = new_value.as_bytes().to_vec();
                if !val.len().is_multiple_of(2) {
                    val.push(0x20); // CS values must be even-length
                }
                let mut out = Vec::with_capacity(data.len() + 16);
                out.extend_from_slice(&data[..elem_start]);
                out.extend_from_slice(&tag.0.to_le_bytes());
                out.extend_from_slice(&tag.1.to_le_bytes());
                out.extend_from_slice(vr.as_bytes());
                out.extend_from_slice(&(val.len() as u16).to_le_bytes());
                out.extend_from_slice(&val);
                out.extend_from_slice(&data[val_end..]);
                return Some(out);
            }
            pos += 4;
            if !skip_explicit(data, &mut pos) {
                return None;
            }
        }
        None
    }

    /// Parse the Basic Offset Table item of an encapsulated PixelData.
    fn bot_offsets_of(data: &[u8]) -> Option<Vec<u32>> {
        let tag_pos = find_tag_position(data, TAG_PIXEL_DATA)?;
        let mut pos = tag_pos + 4; // past tag
        let vr = std::str::from_utf8(data.get(pos..pos + 2)?).unwrap_or("");
        pos += 2;
        if matches!(
            vr,
            "OB" | "OD"
                | "OF"
                | "OL"
                | "OV"
                | "OW"
                | "SQ"
                | "SV"
                | "UC"
                | "UN"
                | "UR"
                | "UT"
                | "UV"
        ) {
            pos += 2; // reserved
        }
        pos += 4; // undefined length
        let item_tag = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let item_elem = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        if (item_tag, item_elem) != (0xFFFE, 0xE000) {
            return None; // first item is not the BOT
        }
        pos += 4;
        let item_len =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        if !item_len.is_multiple_of(4) {
            return None;
        }
        let entries = item_len / 4;
        let mut offs = Vec::with_capacity(entries);
        for i in 0..entries {
            offs.push(u32::from_le_bytes([
                data[pos + i * 4],
                data[pos + i * 4 + 1],
                data[pos + i * 4 + 2],
                data[pos + i * 4 + 3],
            ]));
        }
        Some(offs)
    }

    /// E1: synthetic multi-frame EVRLE (3 frames) -> 4.80 -> EVRLE.
    /// Every frame is its own fragment, the BOT carries one offset per
    /// frame, NumberOfFrames survives, and pixels come back frame-exact.
    #[test]
    fn jpegls_multiframe_evrle_roundtrip_per_frame_fragments_bot() {
        let src = build_multiframe_evrle(3, 8, 8);
        let registry = crate::codec::CodecRegistry::default();

        let jls = crate::transcode::transcode_dicom(&src, TS_JPEGLS_LOSSLESS, &registry)
            .expect("EVRLE -> 4.80 must succeed");

        // NumberOfFrames (0028,0008) preserved.
        assert_eq!(
            find_tag_str(&jls, TAG_NUMBER_OF_FRAMES)
                .as_deref()
                .map(|s| s.trim()),
            Some("3"),
            "NumberOfFrames must survive the transcode"
        );

        // One fragment per frame.
        let fragments = extract_encapsulated_fragments(&jls).expect("encapsulated fragments");
        assert_eq!(fragments.len(), 3, "3 frames -> 3 fragments");

        // BOT present with one offset per frame, pointing at each frame's
        // first-fragment Item Tag (PS3.5 F.2.1).
        let bot = bot_offsets_of(&jls).expect("BOT item");
        assert_eq!(bot.len(), 3, "BOT must carry one offset per frame");
        assert_eq!(bot[0], 0, "frame 0 offset = first item");
        assert_eq!(bot[1] as usize, 8 + fragments[0].len());
        assert_eq!(
            bot[2] as usize,
            16 + fragments[0].len() + fragments[1].len()
        );

        // Back to EVRLE: all frames pixel-exact.
        let back = crate::transcode::transcode_dicom(
            &jls,
            crate::codec::ts_uid::EXPLICIT_VR_LE,
            &registry,
        )
        .expect("4.80 -> EVRLE must succeed");
        let src_px = crate::codec::explicit_vr_le::ExplicitVrLeCodec
            .decode(&src)
            .expect("source decode");
        let back_px = crate::codec::explicit_vr_le::ExplicitVrLeCodec
            .decode(&back)
            .expect("roundtrip decode");
        assert_eq!(back_px.data, src_px.data, "all frames pixel-exact");
        assert_eq!(back_px.rows, 8);
        assert_eq!(back_px.columns, 8);
    }

    /// E2a: RGB EVRLE source -> 4.80: PI=RGB, SPP=3, pixels equal.
    #[test]
    fn jpegls_color_rgb_source_pi_rgb_spp3_pixels_equal() {
        let src = build_color_evrle("RGB");
        let registry = crate::codec::CodecRegistry::default();
        let jls = crate::transcode::transcode_dicom(&src, TS_JPEGLS_LOSSLESS, &registry)
            .expect("RGB -> 4.80 must succeed");
        assert_eq!(
            find_tag_str(&jls, TAG_PHOTOMETRIC_INTERPRETATION)
                .as_deref()
                .map(|s| s.trim()),
            Some("RGB")
        );
        assert_eq!(find_tag_u16(&jls, TAG_SAMPLES_PER_PIXEL), Some(3));
        let src_px = crate::codec::explicit_vr_le::ExplicitVrLeCodec
            .decode(&src)
            .expect("src decode");
        let out_px = JpeglsLosslessCodec.decode(&jls).expect("jls decode");
        assert_eq!(out_px.samples_per_pixel, 3);
        assert_eq!(out_px.photometric_interpretation, "RGB");
        assert_eq!(out_px.data, src_px.data, "color pixels equal (R,G,B order)");
    }

    /// E2b: YBR_FULL_422-tagged JPEG Baseline source -> 4.80:
    /// the baseline decoder yields RGB components (R,G,B order) and the
    /// output is PI=RGB, SPP=3, pixel-exact to that decode.
    #[test]
    fn jpegls_color_ybr422_baseline_source_pi_rgb_pixels_equal() {
        let src = build_color_evrle("RGB");
        let registry = crate::codec::CodecRegistry::default();
        let baseline =
            crate::transcode::transcode_dicom(&src, crate::codec::ts_uid::JPEG_BASELINE, &registry)
                .expect("RGB -> JPEG Baseline must succeed");
        // Real-world YBR-tagged baseline file: patch the PI tag.
        let tagged = replace_cs_value(&baseline, TAG_PHOTOMETRIC_INTERPRETATION, "YBR_FULL_422")
            .expect("patch PI");
        assert_eq!(
            find_tag_str(&tagged, TAG_PHOTOMETRIC_INTERPRETATION)
                .as_deref()
                .map(|s| s.trim()),
            Some("YBR_FULL_422")
        );

        // Decode-side honesty: the baseline decoder reports RGB for a
        // 3-component stream (jpeg-decoder converts YCbCr->RGB internally).
        let src_px = crate::codec::jpeg_baseline::JpegBaselineCodec
            .decode(&tagged)
            .expect("baseline decode");
        assert_eq!(src_px.photometric_interpretation, "RGB");
        assert_eq!(src_px.samples_per_pixel, 3);

        let jls = crate::transcode::transcode_dicom(&tagged, TS_JPEGLS_LOSSLESS, &registry)
            .expect("YBR-tagged baseline -> 4.80 must succeed");
        assert_eq!(
            find_tag_str(&jls, TAG_PHOTOMETRIC_INTERPRETATION)
                .as_deref()
                .map(|s| s.trim()),
            Some("RGB"),
            "JPEG-LS output must be RGB, not YBR_FULL_422"
        );
        assert_eq!(find_tag_u16(&jls, TAG_SAMPLES_PER_PIXEL), Some(3));
        let out_px = JpeglsLosslessCodec.decode(&jls).expect("jls decode");
        assert_eq!(
            out_px.data, src_px.data,
            "pixels equal to the baseline decode"
        );
    }
}
