// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Pure-Rust JPEG2000 codec via `j2k` crate.
//!
//! Replaces the legacy `openjpeg-sys` FFI codec. No C dependencies,
//! no unsafe blocks, no `jpeg2000_shim.c` or `build.rs` needed.
//!
//! Transfer Syntaxes:
//! - 1.2.840.10008.1.2.4.90 (JPEG 2000 Lossless, 5/3 reversible)
//! - 1.2.840.10008.1.2.4.91 (JPEG 2000 Lossy, 9/7 irreversible)
//! - 1.2.840.10008.1.2.4.201 (HTJ2K Lossless) - decode-only
//! - 1.2.840.10008.1.2.4.202 (HTJ2K Lossless RPCL) - decode-only
//! - 1.2.840.10008.1.2.4.203 (HTJ2K Lossy) - decode-only
//!
//! HTJ2K (ISO/IEC 15444-15) is a block-coder variant of the same codestream
//! syntax: `j2k` classifies the CAP marker and decodes either block coder
//! through the one `J2kDecoder` entry point, so the decode path below is
//! shared verbatim and only the advertised UID differs.

#![forbid(unsafe_code)]

use j2k::{EncodedJ2k, EncodedLossyJ2k, J2kDecoder};
use j2k::{J2kLosslessEncodeOptions, J2kLosslessSamples, J2kLossyEncodeOptions, J2kLossySamples};

use super::TAG_PIXEL_DATA;
use super::{CodecError, DecodedPixels, DicomCodec, DicomMeta, ts_uid};

// -- Shared helpers --------------------------------------------------------

/// Extract the JPEG2000 codestream from a DICOM Encapsulated PixelData element.
fn extract_codestream(data: &[u8]) -> Result<Vec<u8>, CodecError> {
    let pix_pos = find_tag_position(data, TAG_PIXEL_DATA)
        .ok_or_else(|| CodecError::DecodeFailed("PixelData (7FE0,0010) not found".into()))?;

    let mut pos = pix_pos + 4;
    if pos + 2 > data.len() {
        return Err(CodecError::DecodeFailed("Truncated at PixelData VR".into()));
    }
    let vr = std::str::from_utf8(&data[pos..pos + 2]).unwrap_or("OB");
    pos += 2;

    let ext = matches!(
        vr,
        "OB" | "OD" | "OF" | "OL" | "OV" | "OW" | "SQ" | "SV" | "UC" | "UN" | "UR" | "UT" | "UV"
    );
    if ext {
        pos += 2; // reserved
        let len = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        pos += 4;
        if len != 0xFFFF_FFFF {
            return Err(CodecError::DecodeFailed(format!(
                "Expected undefined length, got 0x{len:08X}"
            )));
        }
    } else {
        let len = u16::from_le_bytes([data[pos], data[pos + 1]]) as u32;
        pos += 2;
        if len != 0xFFFF {
            return Err(CodecError::DecodeFailed("Expected undefined length".into()));
        }
    }

    // BOT Item (FFFE,E000, len=0)
    if pos + 8 > data.len() {
        return Err(CodecError::DecodeFailed("Truncated at BOT".into()));
    }
    let tag = (
        u16::from_le_bytes([data[pos], data[pos + 1]]),
        u16::from_le_bytes([data[pos + 2], data[pos + 3]]),
    );
    pos += 4;
    if tag != (0xFFFE, 0xE000) {
        return Err(CodecError::DecodeFailed(format!(
            "Expected BOT, got {tag:?}"
        )));
    }
    let bot_len =
        u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
    pos += 4;
    if bot_len > 0 {
        pos = pos.saturating_add(bot_len);
    }

    // First data fragment (FFFE,E000)
    if pos + 8 > data.len() {
        return Err(CodecError::DecodeFailed("Truncated at fragment".into()));
    }
    let ftag = (
        u16::from_le_bytes([data[pos], data[pos + 1]]),
        u16::from_le_bytes([data[pos + 2], data[pos + 3]]),
    );
    pos += 4;
    if ftag != (0xFFFE, 0xE000) {
        return Err(CodecError::DecodeFailed(format!(
            "Expected fragment Item, got {ftag:?}"
        )));
    }
    let flen =
        u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
    pos += 4;
    if pos + flen > data.len() {
        return Err(CodecError::DecodeFailed(
            "Fragment extends past data".into(),
        ));
    }
    Ok(data[pos..pos + flen].to_vec())
}

/// Find tag position in DICOM byte slice (Explicit VR LE assumed for search).
///
/// The walk is flat and top-level, but it must still get PAST elements it is
/// not looking for, and two spellings of an element carry no length to step
/// over: an undefined-length sequence (PS3.5 7.5.1) leaves its end to a
/// delimiter, and the delimiter tags themselves are not attributes at all.
/// A walk that adds `12 + 0xFFFFFFFF` for the sentinel leaves the buffer and
/// finds nothing, which reads as "PixelData not found" for a file whose
/// PixelData is a few hundred bytes further on.
fn find_tag_position(data: &[u8], (target_g, target_e): (u16, u16)) -> Option<usize> {
    let mut pos = if data.len() >= 132 && &data[128..132] == b"DICM" {
        132
    } else {
        0
    };
    while pos + 4 <= data.len() {
        let g = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let e = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        if g == target_g && e == target_e {
            return Some(pos);
        }
        // PS3.5 7.5: (FFFE,E000), (FFFE,E00D) and (FFFE,E0DD) carry NO VR,
        // so the four bytes after the tag are a length and not a VR plus a
        // length. Reading a VR here takes two length bytes as the VR and
        // leaves the walk four bytes out, at every item and delimiter in the
        // encapsulated payload this function exists to find.
        if g == 0xFFFE {
            pos += 8;
            continue;
        }
        pos += 4; // skip tag
        if pos + 2 > data.len() {
            break;
        }
        let vr = std::str::from_utf8(&data[pos..pos + 2]).unwrap_or("");
        pos += 2;
        let len: usize = if matches!(
            vr,
            "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
        ) {
            pos += 2;
            if pos + 4 > data.len() {
                break;
            }
            let l = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                as usize;
            pos += 4;
            // A 32-bit length is the only form that can carry the sentinel
            // (PS3.5 7.5.1), and the sentinel is not a length to step over:
            // the walk enters the item stream and leaves it at (FFFE,E0DD),
            // which the branch above consumes.
            if l == 0xFFFF_FFFF {
                continue;
            }
            l
        } else {
            let l = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            l
        };
        pos += len;
        if !len.is_multiple_of(2) {
            pos += 1;
        }
    }
    None
}

// -- Encode helpers --------------------------------------------------------

/// Read (0028,0002) Samples per Pixel from an explicit-VR DICOM byte slice.
/// Returns 1 when the tag is absent or unreadable (the default assumed by
/// `extract_frame`; PS3.3 requires SPP=1 for monochrome, so the permissive
/// default never wrongly refuses a legal monochrome instance).
fn samples_per_pixel_of(data: &[u8]) -> u16 {
    let Some(pos) = find_tag_position(data, super::TAG_SAMPLES_PER_PIXEL) else {
        return 1;
    };
    let Some(vr_bytes) = data.get(pos + 4..pos + 6) else {
        return 1;
    };
    let Ok(vr) = std::str::from_utf8(vr_bytes) else {
        return 1;
    };
    if vr != "US" {
        return 1;
    }
    let Some(len_bytes) = data.get(pos + 6..pos + 8) else {
        return 1;
    };
    let len = u16::from_le_bytes([len_bytes[0], len_bytes[1]]) as usize;
    if len < 2 {
        return 1;
    }
    let Some(raw) = data.get(pos + 8..pos + 8 + len) else {
        return 1;
    };
    u16::from_le_bytes([raw[0], raw[1]])
}

fn encode_lossless(pixels: &DecodedPixels, meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
    let samples = J2kLosslessSamples::new(
        &pixels.data,
        pixels.columns as u32,
        pixels.rows as u32,
        pixels.samples_per_pixel as u16,
        pixels.bits_allocated,
        // DICOM PixelRepresentation (0028,0103): 1 = signed two's
        // complement. Encoding PR=1 samples as unsigned shifts the
        // values (corpus-proven: 506/626 signed files failed the
        // roundtrip until this became PR-aware).
        meta.pixel_representation == 1,
    )
    .map_err(|e| CodecError::EncodeFailed(format!("j2k: {e}")))?;

    let options = J2kLosslessEncodeOptions::default();
    let encoded: EncodedJ2k = j2k::encode_j2k_lossless(samples, &options)
        .map_err(|e| CodecError::EncodeFailed(format!("j2k encode: {e}")))?;
    Ok(encoded.codestream)
}

fn encode_lossy(pixels: &DecodedPixels, meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
    let samples = J2kLossySamples::new(
        &pixels.data,
        pixels.columns as u32,
        pixels.rows as u32,
        pixels.samples_per_pixel as u16,
        pixels.bits_allocated,
        // Same PixelRepresentation rule as the lossless path above.
        meta.pixel_representation == 1,
    )
    .map_err(|e| CodecError::EncodeFailed(format!("j2k: {e}")))?;

    let options = J2kLossyEncodeOptions::default();
    let encoded: EncodedLossyJ2k = j2k::encode_j2k_lossy(samples, &options)
        .map_err(|e| CodecError::EncodeFailed(format!("j2k encode: {e}")))?;
    Ok(encoded.codestream)
}

// -- Codec structs ---------------------------------------------------------

/// Decode an encapsulated DICOM object whose fragment is a JPEG 2000 or
/// HTJ2K codestream. Both share the codestream syntax (ISO/IEC 15444-1 vs
/// -15 differ only in the block coder named by the CAP marker), so one path
/// serves the .90/.91 and .201/.202 transfer syntaxes.
fn decode_j2k_codestream(dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
    let codestream = extract_codestream(dicom_bytes)?;
    let mut decoder = J2kDecoder::new(&codestream)
        .map_err(|e| CodecError::DecodeFailed(format!("j2k parse: {e}")))?;
    let info = decoder.info().clone();
    let w = info.dimensions.0 as usize;
    let h = info.dimensions.1 as usize;
    let components = info.components as usize;
    let bps = (info.bit_depth.max(8) as usize).div_ceil(8);

    // Conformance gate: a DICOM object must
    // describe every codestream component via Samples per Pixel (0028,0002).
    // A codestream whose component count differs from SPP has no legal
    // DICOM representation (PS3.3 C.7.6.3.1.1/C.7.6.3.1.2, PS3.5 A.4.4),
    // so any component selection would be arbitrary -> refuse (406), never
    // fall back to a guessed plane.
    let spp = samples_per_pixel_of(dicom_bytes) as usize;
    if components != spp {
        return Err(CodecError::NonConformantCodestream(format!(
            "J2K codestream has {components} components but SamplesPerPixel={spp}: non-conformant DICOM"
        )));
    }

    let fmt = match (components, bps) {
        (1, 1) => j2k::PixelFormat::Gray8,
        (3, 1) => j2k::PixelFormat::Rgb8,
        (1, 2) => j2k::PixelFormat::Gray16,
        (3, 2) => j2k::PixelFormat::Rgb16,
        _ => {
            return Err(CodecError::DecodeFailed(format!(
                "Unsupported J2K format: {components} components, {bps} bytes/sample"
            )));
        }
    };

    let stride = w * fmt.bytes_per_pixel();
    let mut out = vec![0u8; h * stride];
    decoder
        .decode_into(&mut out, stride, fmt)
        .map_err(|e| CodecError::DecodeFailed(format!("j2k decode: {e}")))?;
    Ok(DecodedPixels {
        data: out,
        rows: h as u16,
        columns: w as u16,
        samples_per_pixel: components as u8,
        // PS3.5 7.1: BitsAllocated is the CONTAINER width - the sample is
        // stored in the next whole number of bytes - while Bits Stored is the
        // meaningful width. A 12-bit codestream decodes into 16-bit samples,
        // so reporting 12 made the buffer look one byte per sample and every
        // size check rejected a perfectly valid image.
        bits_allocated: (bps * 8) as u8,
        photometric_interpretation: if components == 3 {
            "RGB".into()
        } else {
            "MONOCHROME2".into()
        },
    })
}

/// Pure-Rust JPEG 2000 Lossless codec (5/3 reversible wavelet).
pub struct Jpeg2000NativeLosslessCodec;

impl DicomCodec for Jpeg2000NativeLosslessCodec {
    fn transfer_syntax_uid(&self) -> &'static str {
        ts_uid::JPEG2000_LOSSLESS
    }

    fn name(&self) -> &'static str {
        "JPEG 2000 Lossless (j2k-native)"
    }

    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
        decode_j2k_codestream(dicom_bytes)
    }

    fn encode(&self, pixels: &DecodedPixels, meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
        encode_lossless(pixels, meta)
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

/// Pure-Rust JPEG 2000 Lossy codec (9/7 irreversible wavelet).
///
/// No quality knob: j2k 0.10 exposes PSNR/rate targets, not a 0..1
/// quality scale, so the dead `quality` field was removed (KISS) -
/// `J2kLossyEncodeOptions::default()` drives the encoder.
pub struct Jpeg2000NativeLossyCodec;

impl DicomCodec for Jpeg2000NativeLossyCodec {
    fn transfer_syntax_uid(&self) -> &'static str {
        ts_uid::JPEG2000_LOSSY
    }

    fn name(&self) -> &'static str {
        "JPEG 2000 Lossy (j2k-native)"
    }

    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
        // Decode path is the same for lossy and lossless - j2k auto-detects
        Jpeg2000NativeLosslessCodec.decode(dicom_bytes)
    }

    fn encode(&self, pixels: &DecodedPixels, meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
        encode_lossy(pixels, meta)
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

/// HTJ2K Lossless (1.2.840.10008.1.2.4.201) - decode-only.
///
/// Decode-only because this build ships no HT block encoder: the scalar
/// j2k encoder emits cleanup-pass-only codestreams that no reader treats as
/// lossless HT. Announcing it for encode would promise a transfer syntax the
/// binary cannot deliver (`decode_only` keeps it out of transferCapabilities).
pub struct Htj2kLosslessCodec;

impl DicomCodec for Htj2kLosslessCodec {
    fn transfer_syntax_uid(&self) -> &'static str {
        ts_uid::HTJ2K_LOSSLESS
    }

    fn name(&self) -> &'static str {
        "HTJ2K Lossless (j2k-native)"
    }

    fn decode_only(&self) -> bool {
        true
    }

    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
        decode_j2k_codestream(dicom_bytes)
    }

    fn encode(&self, _pixels: &DecodedPixels, _meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
        Err(CodecError::UnsupportedTransferSyntax(
            ts_uid::HTJ2K_LOSSLESS.to_string(),
        ))
    }
}

/// HTJ2K Lossless with RPCL options (1.2.840.10008.1.2.4.202) - decode-only.
///
/// The RPCL progression order is a codestream property the decoder reads from
/// the COD marker, so the decode path is identical to the plain lossless form.
pub struct Htj2kLosslessRpclCodec;

impl DicomCodec for Htj2kLosslessRpclCodec {
    fn transfer_syntax_uid(&self) -> &'static str {
        ts_uid::HTJ2K_LOSSLESS_RPCL
    }

    fn name(&self) -> &'static str {
        "HTJ2K Lossless RPCL (j2k-native)"
    }

    fn decode_only(&self) -> bool {
        true
    }

    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
        decode_j2k_codestream(dicom_bytes)
    }

    fn encode(&self, _pixels: &DecodedPixels, _meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
        Err(CodecError::UnsupportedTransferSyntax(
            ts_uid::HTJ2K_LOSSLESS_RPCL.to_string(),
        ))
    }
}

/// HTJ2K Lossy (1.2.840.10008.1.2.4.203) - decode-only.
///
/// Same decode path as the lossless variants: the irreversible 9/7 transform
/// is a codestream property, not a decoder choice.
pub struct Htj2kLossyCodec;

impl DicomCodec for Htj2kLossyCodec {
    fn transfer_syntax_uid(&self) -> &'static str {
        ts_uid::HTJ2K_LOSSY
    }

    fn name(&self) -> &'static str {
        "HTJ2K Lossy (j2k-native)"
    }

    fn decode_only(&self) -> bool {
        true
    }

    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
        decode_j2k_codestream(dicom_bytes)
    }

    fn encode(&self, _pixels: &DecodedPixels, _meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
        Err(CodecError::UnsupportedTransferSyntax(
            ts_uid::HTJ2K_LOSSY.to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal explicit-VR DICOM: preamble + (0028,0002) US + (0028,0004) CS +
    /// encapsulated Pixel Data built from a raw J2K codestream.
    fn wrap_j2k(pi: &str, spp: u16, codestream: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        // (0028,0002) US - Samples per Pixel
        buf.extend_from_slice(&0x0028u16.to_le_bytes());
        buf.extend_from_slice(&0x0002u16.to_le_bytes());
        buf.extend_from_slice(b"US");
        buf.extend_from_slice(&2u16.to_le_bytes());
        buf.extend_from_slice(&spp.to_le_bytes());
        // (0028,0004) CS
        buf.extend_from_slice(&0x0028u16.to_le_bytes());
        buf.extend_from_slice(&0x0004u16.to_le_bytes());
        buf.extend_from_slice(b"CS");
        let mut pi_buf = pi.as_bytes().to_vec();
        if !pi_buf.len().is_multiple_of(2) {
            pi_buf.push(b' '); // DICOM even-length padding
        }
        buf.extend_from_slice(&(pi_buf.len() as u16).to_le_bytes());
        buf.extend_from_slice(&pi_buf);
        // (7FE0,0010) encapsulated pixel data
        buf.extend_from_slice(&crate::codec::build_encapsulated_pixeldata(
            codestream, false,
        ));
        buf
    }

    fn encode_j2k(data: &[u8], w: u32, h: u32, comps: u16, bits: u8, signed: bool) -> Vec<u8> {
        let samples =
            J2kLosslessSamples::new(data, w, h, comps, bits, signed).expect("j2k samples");
        let encoded = j2k::encode_j2k_lossless(samples, &J2kLosslessEncodeOptions::default())
            .expect("j2k encode");
        encoded.codestream
    }

    /// A 5-component MONOCHROME codestream with SPP=1 has no legal DICOM
    /// representation (PS3.3 C.7.6.3.1.2) -> the codec refuses instead of
    /// guessing a plane (component 0 would be an arbitrary choice).
    #[test]
    fn decode_five_component_monochrome_refused() {
        let (w, h) = (8u32, 8u32);
        let comps = 5u16;
        let plane = (w * h) as usize;
        let data = vec![0u8; plane * comps as usize];
        let codestream = encode_j2k(&data, w, h, comps, 8, false);
        let dicom = wrap_j2k("MONOCHROME2", 1, &codestream);
        let err = Jpeg2000NativeLosslessCodec.decode(&dicom).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, CodecError::NonConformantCodestream(_)));
        assert!(msg.contains("5 components"));
        assert!(msg.contains("SamplesPerPixel=1"));
    }

    /// A color (RGB, SPP=3) header with a 5-component codestream is refused
    /// by the same mismatch gate (5 != 3).
    #[test]
    fn decode_five_component_color_refused() {
        let (w, h) = (4u32, 4u32);
        let comps = 5u16;
        let plane = (w * h) as usize;
        let data = vec![7u8; plane * comps as usize];
        let codestream = encode_j2k(&data, w, h, comps, 8, false);
        let dicom = wrap_j2k("RGB", 3, &codestream);
        let err = Jpeg2000NativeLosslessCodec.decode(&dicom).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, CodecError::NonConformantCodestream(_)));
        assert!(msg.contains("5 components"));
        assert!(msg.contains("SamplesPerPixel=3"));
    }

    /// Header-side mismatch: a 3-component codestream with SPP=1 is refused
    /// (3 != 1).
    #[test]
    fn decode_three_component_monochrome_refused() {
        let (w, h) = (4u32, 4u32);
        let data = vec![0u8; (w * h * 3) as usize];
        let codestream = encode_j2k(&data, w, h, 3, 8, false);
        let dicom = wrap_j2k("MONOCHROME2", 1, &codestream);
        let err = Jpeg2000NativeLosslessCodec.decode(&dicom).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, CodecError::NonConformantCodestream(_)));
        assert!(msg.contains("3 components"));
        assert!(msg.contains("SamplesPerPixel=1"));
    }

    /// Header-side mismatch: a 1-component codestream with SPP=3 is refused
    /// (1 != 3).
    #[test]
    fn decode_single_component_color_refused() {
        let (w, h) = (4u32, 4u32);
        let data = vec![1u8; (w * h) as usize];
        let codestream = encode_j2k(&data, w, h, 1, 8, false);
        let dicom = wrap_j2k("RGB", 3, &codestream);
        let err = Jpeg2000NativeLosslessCodec.decode(&dicom).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, CodecError::NonConformantCodestream(_)));
        assert!(msg.contains("1 components"));
        assert!(msg.contains("SamplesPerPixel=3"));
    }

    /// PS3.5 7.1: BitsAllocated is the container width. A 12-bit codestream
    /// decodes into 16-bit samples, so the decoded buffer must report 16 and
    /// pass the size check - reporting 12 made a valid image look one byte per
    /// sample and every validation refused it.
    #[test]
    fn decode_12bit_monochrome_passes_validation() {
        let (w, h) = (2u32, 2u32);
        let vals: Vec<u16> = vec![0, 4095, 2048, 17]; // full 12-bit range
        let data: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        let codestream = encode_j2k(&data, w, h, 1, 12, false);
        let dicom = wrap_j2k("MONOCHROME2", 1, &codestream);

        let px = Jpeg2000NativeLosslessCodec
            .decode(&dicom)
            .expect("a 12-bit codestream is a valid DICOM image");
        assert_eq!(px.bits_allocated, 16, "container width, not 12");
        assert_eq!(px.data.len(), 4 * 2, "two bytes per sample");
        px.validate()
            .expect("the decoded buffer is self-consistent");
    }

    /// Signed single-component samples round-trip bit-exactly (the codec
    /// must not shift or re-map signed values; PixelRepresentation is the
    /// stored instance's business, not the decoder's).
    #[test]
    fn decode_signed_monochrome_is_bit_exact() {
        let vals: Vec<u16> = vec![0xFC18, 0xFC19, 0x0001, 0x7FFF]; // -1000, -999, 1, 32767
        let data: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        let codestream = encode_j2k(&data, 2, 2, 1, 16, true);
        let dicom = wrap_j2k("MONOCHROME2", 1, &codestream);
        let px = Jpeg2000NativeLosslessCodec.decode(&dicom).expect("decode");
        assert_eq!(px.samples_per_pixel, 1);
        assert_eq!(px.bits_allocated, 16);
        assert_eq!(px.data, data);
    }

    /// HTJ2K lossy codestream: the CAP marker names the HT block coder, so a
    /// codestream the J2K wrapper parses must decode through the HTJ2K
    /// wrapper too. Lossy samples are approximate, so the assertion is on
    /// geometry and amplitude, not on exact bytes.
    #[test]
    fn htj2k_decode_returns_the_codestream_geometry() {
        let (w, h) = (8u32, 8u32);
        let data: Vec<u8> = (0..w * h).map(|i| (i * 7 % 256) as u8).collect();
        let samples = J2kLossySamples::new(&data, w, h, 1, 8, false).expect("j2k samples");
        let options = J2kLossyEncodeOptions::default()
            .with_block_coding_mode(j2k::J2kBlockCodingMode::HighThroughput);
        let encoded = j2k::encode_j2k_lossy(samples, &options).expect("HTJ2K encode");
        // The fixture must really be HT-coded, otherwise this test proves
        // nothing about the HT path: j2k classifies the CAP marker and names
        // the block coder it found.
        let view = j2k::J2kView::parse(&encoded.codestream).expect("parse");
        let syntax = view.support_info().expect("support info").transfer_syntax;
        assert!(
            matches!(
                syntax,
                j2k::CompressedTransferSyntax::HtJpeg2000Lossless
                    | j2k::CompressedTransferSyntax::HtJpeg2000Lossy
            ),
            "fixture is not HTJ2K: {syntax:?}"
        );
        let dicom = wrap_j2k("MONOCHROME2", 1, &encoded.codestream);
        let px = Htj2kLossyCodec.decode(&dicom).expect("HTJ2K decode");
        assert_eq!((px.rows, px.columns), (h as u16, w as u16));
        assert_eq!(px.samples_per_pixel, 1);
        assert_eq!(px.data.len(), (w * h) as usize);
    }

    /// HTJ2K is decode-only: announcing an encoder would put a transfer
    /// syntax in transferCapabilities that this build cannot produce.
    #[test]
    fn htj2k_codecs_are_decode_only_and_refuse_encode() {
        let px = DecodedPixels {
            data: vec![0u8; 4],
            rows: 2,
            columns: 2,
            samples_per_pixel: 1,
            bits_allocated: 8,
            photometric_interpretation: "MONOCHROME2".into(),
        };
        let meta = DicomMeta {
            rows: 2,
            columns: 2,
            samples_per_pixel: 1,
            bits_allocated: 8,
            bits_stored: 8,
            photometric_interpretation: "MONOCHROME2".into(),
            planar_configuration: None,
            pixel_representation: 0,
            frames: 1,
        };
        for codec in [
            &Htj2kLosslessCodec as &dyn DicomCodec,
            &Htj2kLosslessRpclCodec as &dyn DicomCodec,
            &Htj2kLossyCodec as &dyn DicomCodec,
        ] {
            assert!(codec.decode_only());
            assert!(codec.encode(&px, &meta).is_err());
        }
    }

    /// The three HTJ2K transfer syntaxes differ only in progression order and
    /// transform, both codestream properties: one decoder serves all three,
    /// and each must advertise its own UID (PS3.6 HTJ2KLossless /
    /// HTJ2KLosslessRPCL / HTJ2K).
    #[test]
    fn each_htj2k_codec_advertises_its_own_transfer_syntax() {
        assert_eq!(
            Htj2kLosslessCodec.transfer_syntax_uid(),
            "1.2.840.10008.1.2.4.201"
        );
        assert_eq!(
            Htj2kLosslessRpclCodec.transfer_syntax_uid(),
            "1.2.840.10008.1.2.4.202"
        );
        assert_eq!(
            Htj2kLossyCodec.transfer_syntax_uid(),
            "1.2.840.10008.1.2.4.203"
        );
    }

    /// Encode path must honor PixelRepresentation=1: signed 16-bit
    /// samples round-trip byte-exactly through the wrapper's own
    /// encode_lossless (fixed 27.08: the wrapper passed signed=false
    /// and shifted PR=1 values).
    #[test]
    fn encode_signed_16bit_roundtrips() {
        let vals: Vec<u16> = vec![0xFC18, 0xFC19, 0x0001, 0x7FFF]; // -1000, -999, 1, 32767
        let data: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        let pixels = DecodedPixels {
            data,
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
            pixel_representation: 1,
            frames: 1,
        };
        let cs = Jpeg2000NativeLosslessCodec
            .encode(&pixels, &meta)
            .expect("encode signed");
        let dicom = wrap_j2k("MONOCHROME2", 1, &cs);
        let px = Jpeg2000NativeLosslessCodec.decode(&dicom).expect("decode");
        assert_eq!(px.data, pixels.data);
    }

    /// Encode path with PixelRepresentation=0: unsigned 16-bit samples
    /// round-trip byte-exactly (guard against breaking the default).
    #[test]
    fn encode_unsigned_16bit_roundtrips() {
        let vals: Vec<u16> = vec![0x0001, 0x00FF, 0x7FFF, 0xFFFF];
        let data: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        let pixels = DecodedPixels {
            data,
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
        let cs = Jpeg2000NativeLosslessCodec
            .encode(&pixels, &meta)
            .expect("encode unsigned");
        let dicom = wrap_j2k("MONOCHROME2", 1, &cs);
        let px = Jpeg2000NativeLosslessCodec.decode(&dicom).expect("decode");
        assert_eq!(px.data, pixels.data);
    }

    /// An undefined-length sequence, one item, and both delimiters, inserted
    /// ahead of the PixelData element `wrap_j2k` builds.
    ///
    /// The derived encoder writes (0008,2112) exactly this way, so this is
    /// the shape of a file the engine produced itself: a J2K payload behind
    /// a sequence the walker has to step over rather than measure.
    fn with_undefined_length_sequence(dicom: Vec<u8>) -> Vec<u8> {
        let mut seq = Vec::new();
        seq.extend_from_slice(&0x0008u16.to_le_bytes());
        seq.extend_from_slice(&0x2112u16.to_le_bytes());
        seq.extend_from_slice(b"SQ");
        seq.extend_from_slice(&[0, 0]);
        seq.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // (FFFE,E000) item, undefined length of its own.
        seq.extend_from_slice(&0xFFFEu16.to_le_bytes());
        seq.extend_from_slice(&0xE000u16.to_le_bytes());
        seq.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // One UI element, so the item is not empty. 27 characters, so the
        // NUL pad brings it to 28 (PS3.5 6.2).
        seq.extend_from_slice(&0x0008u16.to_le_bytes());
        seq.extend_from_slice(&0x1150u16.to_le_bytes());
        seq.extend_from_slice(b"UI");
        seq.extend_from_slice(&28u16.to_le_bytes());
        seq.extend_from_slice(b"1.2.840.10008.5.1.4.1.1.1.1\0");
        // (FFFE,E00D) then (FFFE,E0DD).
        seq.extend_from_slice(&0xFFFEu16.to_le_bytes());
        seq.extend_from_slice(&0xE00Du16.to_le_bytes());
        seq.extend_from_slice(&0u32.to_le_bytes());
        seq.extend_from_slice(&0xFFFEu16.to_le_bytes());
        seq.extend_from_slice(&0xE0DDu16.to_le_bytes());
        seq.extend_from_slice(&0u32.to_le_bytes());
        // Splice it in after the (0028,0004) CS element, i.e. immediately
        // before the encapsulated PixelData.
        let pixel_at = dicom
            .windows(4)
            .rposition(|w| w == [0xE0, 0x7F, 0x10, 0x00])
            .expect("the fixture has a PixelData tag");
        let mut out = Vec::with_capacity(dicom.len() + seq.len());
        out.extend_from_slice(&dicom[..pixel_at]);
        out.extend_from_slice(&seq);
        out.extend_from_slice(&dicom[pixel_at..]);
        out
    }

    /// The walker must step over an undefined-length sequence, not measure it.
    ///
    /// `pos += 12 + 0xFFFFFFFF` leaves the buffer on the sentinel, and this
    /// function has no byte-scan fallback - so the whole decode fails with
    /// "PixelData (7FE0,0010) not found" for a file whose PixelData is a few
    /// hundred bytes further on. The codestream is compared to the same
    /// fixture WITHOUT the sequence, so the test proves the walker landed on
    /// the right element and not merely on some element.
    #[test]
    fn the_walker_steps_over_an_undefined_length_sequence() {
        let (w, h) = (8u32, 8u32);
        let data = vec![0u8; (w * h) as usize];
        let codestream = encode_j2k(&data, w, h, 1, 8, false);

        let plain = wrap_j2k("MONOCHROME2", 1, &codestream);
        let with_sequence = with_undefined_length_sequence(plain.clone());

        // The fixture is only interesting if it is longer than the plain one.
        assert!(with_sequence.len() > plain.len());

        let plain_pixels = Jpeg2000NativeLosslessCodec
            .decode(&plain)
            .expect("the control fixture decodes");
        let pixels = Jpeg2000NativeLosslessCodec
            .decode(&with_sequence)
            .expect("the sequence is stepped over, so the codestream is found");
        assert_eq!(
            pixels.data, plain_pixels.data,
            "the same codestream, so the same pixels"
        );
    }

    /// The delimiter branch: group FFFE carries no VR (PS3.5 7.5), and a
    /// walker that reads one is four bytes out at every item header - which
    /// the encapsulated PixelData is entirely made of.
    #[test]
    fn the_walker_consumes_an_item_header_without_reading_a_vr() {
        let (w, h) = (8u32, 8u32);
        let data = vec![0u8; (w * h) as usize];
        let codestream = encode_j2k(&data, w, h, 1, 8, false);
        let dicom = wrap_j2k("MONOCHROME2", 1, &codestream);

        // The BOT item follows PixelData, so the walker never needs to step
        // over it - but `extract_codestream` does, and it is the same header
        // shape. Assert the position the walker reports is the real one.
        let at = find_tag_position(&dicom, super::super::TAG_PIXEL_DATA)
            .expect("the walker finds PixelData");
        assert_eq!(&dicom[at + 4..at + 6], b"OB", "and lands on its VR");
    }
}
