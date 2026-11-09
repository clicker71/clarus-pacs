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
}
