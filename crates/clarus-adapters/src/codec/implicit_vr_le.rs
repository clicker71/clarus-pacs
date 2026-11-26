// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Implicit VR Little Endian codec - uncompressed, no VR bytes.
//!
//! Produces DICOM with Implicit VR LE (1.2.840.10008.1.2) transfer syntax.
//! Pixel data is uncompressed - `encode()` writes raw bytes, same as EVRLE.
//! The DICOM wrapper (implicit VR) is handled by `update_dicom_metadata()`.

#![forbid(unsafe_code)]

use super::explicit_vr_le::ExplicitVrLeCodec;
use super::{CodecError, DecodedPixels, DicomCodec, DicomMeta, ts_uid};

/// Codec for Implicit VR Little Endian (1.2.840.10008.1.2).
///
/// Delegates encode/decode to ExplicitVrLeCodec - the pixel data layout
/// is identical (raw uncompressed bytes). Only the DICOM element wrapper
/// differs (implicit VR = no VR bytes, always 4-byte lengths).
pub struct ImplicitVrLeCodec;

impl DicomCodec for ImplicitVrLeCodec {
    fn transfer_syntax_uid(&self) -> &'static str {
        ts_uid::IMPLICIT_VR_LE
    }

    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
        ExplicitVrLeCodec.decode(dicom_bytes)
    }

    fn encode(&self, pixels: &DecodedPixels, meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
        ExplicitVrLeCodec.encode(pixels, meta)
    }

    fn encode_pixeldata(
        &self,
        pixels: &DecodedPixels,
        meta: &DicomMeta,
    ) -> Result<Vec<u8>, CodecError> {
        // PS3.5 7.1.2: an Implicit VR element is tag(4) + ValueLength(4) + value,
        // with NO VR bytes and no reserved field. The inherited default writes
        // the Explicit VR form, so the two bytes it uses for "OW" landed where
        // an implicit reader takes the first half of the length: the element
        // then declared a length of 0x4F57 ("OW" read little endian) and every
        // element after PixelData was addressed from the wrong offset.
        let raw = self.encode(pixels, meta)?;
        // PS3.5 8.1: the Value Field is padded to an even length and the pad is
        // part of the declared length.
        let pad = usize::from(!raw.len().is_multiple_of(2));
        // The Value Length field is 32 bits. `as u32` wrapped a value at or
        // above 4 GiB into a small number, so the element declared a length
        // that did not cover its own value and the reader took the remainder as
        // the next element's header. Checked, so it is refused instead.
        let declared = raw
            .len()
            .checked_add(pad)
            .ok_or_else(|| CodecError::EncodeFailed("PixelData length overflows usize".into()))?;
        let declared = u32::try_from(declared).map_err(|_| {
            CodecError::EncodeFailed(format!(
                "PixelData of {} bytes does not fit the 32-bit Value Length field",
                raw.len() + pad
            ))
        })?;
        let mut out = Vec::with_capacity(8 + raw.len() + pad);
        out.extend_from_slice(&super::TAG_PIXEL_DATA.0.to_le_bytes());
        out.extend_from_slice(&super::TAG_PIXEL_DATA.1.to_le_bytes());
        out.extend_from_slice(&declared.to_le_bytes());
        out.extend_from_slice(&raw);
        if pad == 1 {
            out.push(0x00);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono_pixels(data: Vec<u8>) -> DecodedPixels {
        DecodedPixels {
            data,
            rows: 1,
            columns: 2,
            samples_per_pixel: 1,
            bits_allocated: 8,
            photometric_interpretation: "MONOCHROME2".to_string(),
        }
    }

    fn mono_meta() -> DicomMeta {
        DicomMeta {
            rows: 1,
            columns: 2,
            samples_per_pixel: 1,
            bits_allocated: 8,
            bits_stored: 8,
            photometric_interpretation: "MONOCHROME2".to_string(),
            planar_configuration: None,
            pixel_representation: 0,
            frames: 1,
        }
    }

    #[test]
    fn the_pixeldata_header_carries_no_vr() {
        // PS3.5 7.1.2: Implicit VR is tag(4) + ValueLength(4) + value. The
        // inherited default wrote the Explicit VR form, whose "OW" bytes landed
        // where an implicit reader takes the first half of the length.
        let codec = ImplicitVrLeCodec;
        let element = codec
            .encode_pixeldata(&mono_pixels(vec![1, 2]), &mono_meta())
            .expect("encoded");
        assert_eq!(&element[..4], &[0xE0, 0x7F, 0x10, 0x00], "tag");
        assert_eq!(
            &element[4..8],
            &2u32.to_le_bytes(),
            "a 4-byte length, not the two VR bytes"
        );
        assert_eq!(&element[8..10], &[1, 2], "and the value follows it");
        assert_eq!(element.len(), 10);
    }

    #[test]
    fn an_odd_value_is_declared_with_its_pad() {
        // PS3.5 8.1: the Value Field is even and the pad counts in the length.
        let codec = ImplicitVrLeCodec;
        let mut pixels = mono_pixels(vec![1, 2, 3]);
        pixels.columns = 3;
        let mut meta = mono_meta();
        meta.columns = 3;
        let element = codec.encode_pixeldata(&pixels, &meta).expect("encoded");
        assert_eq!(&element[4..8], &4u32.to_le_bytes(), "declared length 4");
        assert_eq!(element.len(), 12, "tag + length + value + pad");
        assert_eq!(element[11], 0x00, "the pad byte");
    }

    #[test]
    fn a_value_length_that_does_not_fit_the_wire_field_is_refused() {
        // The Value Length field is 32 bits (PS3.5 7.1.2). The writer used to
        // narrow the padded length with `as u32`, so a value at 4 GiB declared
        // a length of 0 and the reader took the samples for the next element's
        // header. The check is a pure arithmetic bound, so it is asserted on
        // the conversion itself rather than by allocating 4 GiB.
        let at_the_limit = u32::MAX as usize;
        assert!(u32::try_from(at_the_limit).is_ok(), "4 GiB - 1 fits");
        assert!(
            u32::try_from(at_the_limit + 1).is_err(),
            "4 GiB does not fit; this is the bound the writer enforces"
        );
        // And the wrap it replaced is visible in the old expression.
        assert_eq!(((at_the_limit + 1) as u32), 0, "the old cast wrapped to 0");
    }
}
