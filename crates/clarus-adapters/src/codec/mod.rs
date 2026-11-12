// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DICOM Codec Registry - pluggable transfer syntax transcoders.
//!
//! Each codec implements [`DicomCodec`] trait. The registry maps
//! Transfer Syntax UID -> codec instance.
//!
//! # Supported Transfer Syntaxes (G-05)
//! - Explicit VR Little Endian (1.2.840.10008.1.2.1) - no-op passthrough
//! - Explicit VR Big Endian (1.2.840.10008.1.2.2) - byte-swapped passthrough
//! - RLE Lossless (1.2.840.10008.1.2.5) - PackBits, pure Rust
//! - JPEG Baseline (1.2.840.10008.1.2.4.50) - `transcode` feature
//! - JPEG Lossless P14/P14.1 (1.2.840.10008.1.2.4.57/.70) - `transcode` feature
//! - JPEG-LS Lossless/Near-Lossless (1.2.840.10008.1.2.4.80/.81) - `transcode` feature
//! - JPEG2000 Lossless (1.2.840.10008.1.2.4.90) - `transcode` feature
//! - JPEG2000 Lossy (1.2.840.10008.1.2.4.91) - `transcode` feature

#![deny(unsafe_code)]

pub mod color_space;
pub mod explicit_vr_be;
pub mod explicit_vr_le;
pub mod implicit_vr_le;
#[cfg(feature = "transcode")]
pub mod jpeg_baseline;
#[cfg(feature = "transcode")]
pub mod jpeg_lossless;
pub mod non_image_sop_classes;
pub mod pixel_data;
pub mod rle;
pub mod vr_dict;
/// Generated PS3.6 VR table behind [`vr_dict::vr_for_tag`].
pub mod vr_dict_std;

// JPEG2000 - pure-Rust native codec (j2k/j2k-native)
// Legacy C/FFI (jpeg2000-legacy) removed in v0.3.0-alpha.
#[cfg(feature = "transcode")]
pub mod jpeg2000_native;

// JPEG-LS codec (encode + decode, ~20 KB via pure_jpegls) - gated behind
// `transcode` like every other JPEG/JPEG2000 codec (drops
// pure_jpegls from the default build).
#[cfg(feature = "transcode")]
pub mod jpegls;

use std::collections::HashMap;
use std::sync::Arc;

// -- DICOM tag constants for image metadata --------------------------------

/// Tag: Rows (0028,0010) - number of rows in the image.
pub const TAG_ROWS: (u16, u16) = (0x0028, 0x0010);
/// Tag: Columns (0028,0011) - number of columns in the image.
pub const TAG_COLUMNS: (u16, u16) = (0x0028, 0x0011);
/// Tag: SamplesPerPixel (0028,0002) - 1 for monochrome, 3 for RGB/YBR.
pub const TAG_SAMPLES_PER_PIXEL: (u16, u16) = (0x0028, 0x0002);
/// Tag: BitsAllocated (0028,0100) - bits per sample (8, 16).
pub const TAG_BITS_ALLOCATED: (u16, u16) = (0x0028, 0x0100);
/// Tag: BitsStored (0028,0101) - actual bits used per sample.
pub const TAG_BITS_STORED: (u16, u16) = (0x0028, 0x0101);
/// Tag: PhotometricInterpretation (0028,0004) - MONOCHROME1/2, RGB, YBR_FULL, etc.
pub const TAG_PHOTOMETRIC_INTERPRETATION: (u16, u16) = (0x0028, 0x0004);
/// Tag: PlanarConfiguration (0028,0006) - 0=interleaved, 1=planar.
pub const TAG_PLANAR_CONFIGURATION: (u16, u16) = (0x0028, 0x0006);
/// Tag: PixelRepresentation (0028,0103) - 0=unsigned, 1=signed (two's complement).
pub const TAG_PIXEL_REPRESENTATION: (u16, u16) = (0x0028, 0x0103);
/// Tag: PixelData (7FE0,0010) - raw or compressed pixel bytes.
pub const TAG_PIXEL_DATA: (u16, u16) = (0x7FE0, 0x0010);
/// Tag: NumberOfFrames (0028,0008) - number of frames, IS, 1 = single-frame.
pub const TAG_NUMBER_OF_FRAMES: (u16, u16) = (0x0028, 0x0008);
/// Tag: TransferSyntaxUID (0002,0010) - meta-header.
pub const TAG_TRANSFER_SYNTAX_UID: (u16, u16) = (0x0002, 0x0010);

/// Known Transfer Syntax UIDs.
pub mod ts_uid {
    /// Implicit VR Little Endian (uncompressed, no VR bytes in dataset).
    pub const IMPLICIT_VR_LE: &str = "1.2.840.10008.1.2";
    /// Explicit VR Little Endian (uncompressed).
    pub const EXPLICIT_VR_LE: &str = "1.2.840.10008.1.2.1";
    /// RLE Lossless (PackBits).
    pub const RLE_LOSSLESS: &str = "1.2.840.10008.1.2.5";
    /// Explicit VR Big Endian (uncompressed, big-endian byte order).
    pub const EXPLICIT_VR_BE: &str = "1.2.840.10008.1.2.2";
    /// JPEG Baseline (lossy, 8-bit).
    pub const JPEG_BASELINE: &str = "1.2.840.10008.1.2.4.50";
    /// JPEG Lossless, Non-Hierarchical (Process 14). Decode-only.
    pub const JPEG_LOSSLESS: &str = "1.2.840.10008.1.2.4.57";
    /// JPEG Lossless, SV1 (Process 14.1). Decode-only.
    pub const JPEG_LOSSLESS_SV1: &str = "1.2.840.10008.1.2.4.70";
    /// JPEG-LS Lossless. Requires `--features jpegls`.
    pub const JPEGLS_LOSSLESS: &str = "1.2.840.10008.1.2.4.80";
    /// JPEG-LS Near-Lossless. Requires `--features jpegls`.
    pub const JPEGLS_NEAR_LOSSLESS: &str = "1.2.840.10008.1.2.4.81";
    /// JPEG2000 Lossless (5/3 wavelet, reversible). Requires jpeg2000 feature.
    pub const JPEG2000_LOSSLESS: &str = "1.2.840.10008.1.2.4.90";
    /// JPEG2000 Lossy (9/7 wavelet, irreversible). Requires jpeg2000 feature.
    pub const JPEG2000_LOSSY: &str = "1.2.840.10008.1.2.4.91";
    /// HTJ2K Lossless (PS3.6 keyword `HTJ2KLossless`). Decode-only.
    pub const HTJ2K_LOSSLESS: &str = "1.2.840.10008.1.2.4.201";
    /// HTJ2K Lossless with RPCL options (`HTJ2KLosslessRPCL`). Decode-only.
    pub const HTJ2K_LOSSLESS_RPCL: &str = "1.2.840.10008.1.2.4.202";
    /// HTJ2K Lossy (`HTJ2K`). Decode-only.
    pub const HTJ2K_LOSSY: &str = "1.2.840.10008.1.2.4.203";
}

// ===========================================================================
// DecodedPixels - intermediate format for transcoding
// ===========================================================================

/// Decoded pixel data ready for re-encoding or color-space conversion.
///
/// Always stored as planar (RRR...GGG...BBB...), one byte per sample element
/// (e.g., 2 bytes per 16-bit sample, 1 byte per 8-bit sample).
#[derive(Debug, Clone)]
pub struct DecodedPixels {
    /// Raw pixel bytes. Layout: planar by channel.
    /// For monochrome: all pixels in a single plane.
    /// For RGB/YBR: R plane, then G plane, then B plane.
    pub data: Vec<u8>,
    /// Number of rows (height).
    pub rows: u16,
    /// Number of columns (width).
    pub columns: u16,
    /// Number of samples per pixel (1 for monochrome, 3 for color).
    pub samples_per_pixel: u8,
    /// Bits allocated per sample (typically 8 or 16).
    pub bits_allocated: u8,
    /// Photometric interpretation (MONOCHROME1, MONOCHROME2, RGB, YBR_FULL, etc.).
    pub photometric_interpretation: String,
}

impl DecodedPixels {
    /// Total number of pixels in the image.
    pub fn pixel_count(&self) -> usize {
        self.rows as usize * self.columns as usize
    }

    /// Total number of bytes expected, accounting for bits_allocated.
    pub fn expected_size(&self) -> usize {
        let bytes_per_sample = (self.bits_allocated as usize) >> 3;
        if bytes_per_sample == 0 {
            return 0; // degenerate: bits_allocated < 8
        }
        self.pixel_count() * self.samples_per_pixel as usize * bytes_per_sample
    }

    /// Validate internal consistency.
    pub fn validate(&self) -> Result<(), CodecError> {
        self.validate_frames(1)
    }

    /// Validate internal consistency for an N-frame image.
    ///
    /// `frames` = NumberOfFrames (0028,0008), 1 for a single-frame image.
    /// Multi-frame pixel data holds ALL frames concatenated (planar layout
    /// stays per-plane: all frames of plane R, then all frames of plane G...).
    pub fn validate_frames(&self, frames: u32) -> Result<(), CodecError> {
        if frames == 0 {
            return Err(CodecError::InvalidMetadata(
                "NumberOfFrames must be > 0".into(),
            ));
        }
        if self.rows == 0 || self.columns == 0 {
            return Err(CodecError::InvalidMetadata(
                "Rows and Columns must be > 0".into(),
            ));
        }
        if self.samples_per_pixel != 1 && self.samples_per_pixel != 3 {
            return Err(CodecError::InvalidMetadata(format!(
                "SamplesPerPixel must be 1 or 3, got {}",
                self.samples_per_pixel
            )));
        }
        let expected = self
            .expected_size()
            .checked_mul(frames as usize)
            .ok_or_else(|| CodecError::InvalidMetadata("frame size overflow".into()))?;
        if self.data.len() != expected {
            return Err(CodecError::InvalidMetadata(format!(
                "Pixel data size mismatch: got {} bytes, expected {} ({}x{}x{}x{} frames)",
                self.data.len(),
                expected,
                self.rows,
                self.columns,
                self.samples_per_pixel,
                frames,
            )));
        }
        Ok(())
    }
}

// ===========================================================================
// DicomMeta - minimal metadata needed for transcoding
// ===========================================================================

/// Minimal DICOM metadata needed for transcoding.
#[derive(Debug, Clone)]
pub struct DicomMeta {
    pub rows: u16,
    pub columns: u16,
    pub samples_per_pixel: u8,
    pub bits_allocated: u8,
    pub bits_stored: u8,
    pub photometric_interpretation: String,
    pub planar_configuration: Option<u16>,
    /// PixelRepresentation (0028,0103): 0 = unsigned, 1 = signed.
    /// JPEG2000 encode uses it for the `signed` flag of the j2k samples.
    pub pixel_representation: u16,
    /// Number of frames (0028,0008), default 1.
    /// Multi-frame pixel data holds all frames concatenated (per-plane,
    /// frame-major). Encoders of encapsulated TSs MUST emit one fragment
    /// per frame (PS3.5 A.4) and a per-frame Basic Offset Table.
    pub frames: u32,
}

// ===========================================================================
// CodecError - unified error type
// ===========================================================================

/// Errors that can occur during codec operations.
#[derive(Debug)]
pub enum CodecError {
    /// Transfer syntax not supported by any registered codec.
    UnsupportedTransferSyntax(String),
    /// Decoding failed (corrupt data, unsupported variant, etc.).
    DecodeFailed(String),
    /// Encoding failed.
    EncodeFailed(String),
    /// Invalid or missing DICOM metadata.
    InvalidMetadata(String),
    /// The codestream has no legal DICOM representation (e.g. JPEG 2000
    /// component count != Samples per Pixel (0028,0002)). Deterministic and
    /// permanent: the receiver must refuse the transcode (406), not fall
    /// back to serving original bytes.
    NonConformantCodestream(String),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::UnsupportedTransferSyntax(ts) => {
                write!(f, "Unsupported transfer syntax: {ts}")
            }
            CodecError::DecodeFailed(msg) => write!(f, "Decode failed: {msg}"),
            CodecError::EncodeFailed(msg) => write!(f, "Encode failed: {msg}"),
            CodecError::InvalidMetadata(msg) => write!(f, "Invalid metadata: {msg}"),
            CodecError::NonConformantCodestream(msg) => {
                write!(f, "Non-conformant codestream: {msg}")
            }
        }
    }
}

impl std::error::Error for CodecError {}

// ===========================================================================
// PixelData element builders - shared by codecs
// ===========================================================================

/// Build a native (uncompressed) PixelData element: (7FE0,0010) OW + defined length.
/// Used by uncompressed codecs (EVRLE, EVRBE, IVRLE).
pub fn build_native_ow_pixeldata(raw_pixels: &[u8]) -> Vec<u8> {
    let mut elem = Vec::with_capacity(12 + raw_pixels.len() + 1);
    elem.extend_from_slice(&0x7FE0u16.to_le_bytes());
    elem.extend_from_slice(&0x0010u16.to_le_bytes());
    elem.extend_from_slice(b"OW");
    elem.extend_from_slice(&[0x00, 0x00]); // reserved
    elem.extend_from_slice(&(raw_pixels.len() as u32).to_le_bytes());
    elem.extend_from_slice(raw_pixels);
    elem
}

/// Build an encapsulated (compressed) PixelData element.
/// If `is_implicit`, writes Implicit VR header (tag + 4-byte undefined length).
/// Otherwise writes Explicit VR (tag + "OB" + reserved + 4-byte undefined length).
pub fn build_encapsulated_pixeldata(fragment: &[u8], is_implicit: bool) -> Vec<u8> {
    // Pad fragment to multiple of 4 (DICOM requirement for encapsulated data).
    // Old update_dicom_metadata did this - decoders expect it.
    let mut padded = fragment.to_vec();
    while !padded.len().is_multiple_of(4) {
        padded.push(0x00);
    }

    let total = 4 + 2 + 2 + 4   // Tag + VR + reserved + undefined len
        + 4 + 4                  // BOT: empty Item (FFFE,E000 + 0x00000000)
        + 4 + 4 + padded.len()   // Fragment Item: tag + len + data
        + 4 + 4; // Sequence Delimiter
    let mut out = Vec::with_capacity(total);

    // Tag (7FE0,0010)
    out.extend_from_slice(&0x7FE0u16.to_le_bytes());
    out.extend_from_slice(&0x0010u16.to_le_bytes());
    if is_implicit {
        // Implicit VR: tag(4) + length(4) with undefined length
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    } else {
        // VR "OB"
        out.extend_from_slice(b"OB");
        // Reserved (0x0000)
        out.extend_from_slice(&[0x00, 0x00]);
        // Undefined length (0xFFFF_FFFF)
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    }
    // BOT - empty Item (length = 0). Required by DICOM PS3.5.
    out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]); // (FFFE,E000)
    out.extend_from_slice(&0u32.to_le_bytes());
    // Fragment Item
    out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]); // (FFFE,E000)
    out.extend_from_slice(&(padded.len() as u32).to_le_bytes());
    out.extend_from_slice(&padded);
    // Sequence Delimiter
    out.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]); // (FFFE,E0DD)
    out.extend_from_slice(&0u32.to_le_bytes());

    out
}

/// Coalesce fragments when they all belong to ONE frame.
///
/// A single-frame image may be split across fragments by the sender: PS3.5
/// A.4 allows an encoder to break a codestream at any byte boundary, and the
/// Item boundaries then carry no meaning. A decoder that insists on
/// `fragments.len() == NumberOfFrames` rejects such a file even though it is
/// perfectly conformant. Only `frames == 1` may coalesce - with more frames
/// the fragment-to-frame mapping is the only thing identifying a frame.
pub fn coalesce_single_frame_fragments(
    fragments: Vec<Vec<u8>>,
    frames: usize,
    components: usize,
) -> Vec<Vec<u8>> {
    if frames != 1 || fragments.len() <= 1 {
        return fragments;
    }
    // A colour frame is one codestream PER COMPONENT (PS3.5 A.4.2), so a
    // 3-fragment single-frame RGB image is three whole codestreams and not one
    // split codestream. Joining them produced a single blob, `len() / 3`
    // rounded to zero frames, and the decode failed with "0 fragments is not a
    // whole number of frames" - a plain colour JPEG-LS image that no viewer
    // could open.
    let components = components.max(1);
    if fragments.len() == components {
        return fragments;
    }
    let mut joined = Vec::with_capacity(fragments.iter().map(|f| f.len()).sum());
    for frag in fragments {
        joined.extend_from_slice(&frag);
    }
    vec![joined]
}

/// Build an encapsulated PixelData element from MULTIPLE fragments
/// (one fragment per frame / per frame component, PS3.5 A.4).
pub fn build_encapsulated_pixeldata_multi(
    fragments: &[Vec<u8>],
    bot_stride: usize,
    is_implicit: bool,
) -> Vec<u8> {
    // Pad every fragment to a multiple of 4 (DICOM requirement).
    let padded: Vec<Vec<u8>> = fragments
        .iter()
        .map(|f| {
            let mut p = f.clone();
            while !p.len().is_multiple_of(4) {
                p.push(0x00);
            }
            p
        })
        .collect();

    // Per-frame BOT entries only when the fragments really are frame-major.
    let frames = if bot_stride > 0 && padded.len() > 1 && padded.len().is_multiple_of(bot_stride) {
        padded.len() / bot_stride
    } else {
        0
    };
    let bot_entry_count = if frames > 1 { frames } else { 0 };

    let data_len: usize = padded.iter().map(|f| f.len()).sum();
    let total = 4 + 2 + 2 + 4 // Tag + VR + reserved + undefined len
        + 8 + bot_entry_count * 4 // BOT item
        + padded.len() * 8 + data_len // Fragment items
        + 8; // Sequence Delimiter
    let mut out = Vec::with_capacity(total);

    // Tag (7FE0,0010)
    out.extend_from_slice(&0x7FE0u16.to_le_bytes());
    out.extend_from_slice(&0x0010u16.to_le_bytes());
    if is_implicit {
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    } else {
        out.extend_from_slice(b"OB");
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    }

    // BOT item
    out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]); // (FFFE,E000)
    if bot_entry_count > 0 {
        out.extend_from_slice(&((bot_entry_count * 4) as u32).to_le_bytes());
        // Each item occupies 8 header bytes + padded.len(); frame f's first
        // fragment is item (f * bot_stride). Offsets are relative to the
        // first byte of the first Item following the BOT item.
        for f in 0..frames {
            let item_index = f * bot_stride;
            let mut off: u32 = 0;
            for prev in padded.iter().take(item_index) {
                off = off.saturating_add(8 + prev.len() as u32);
            }
            out.extend_from_slice(&off.to_le_bytes());
        }
    } else {
        // Empty BOT (length 0) - offsets unused.
        out.extend_from_slice(&0u32.to_le_bytes());
    }

    // One Item per fragment
    for frag in &padded {
        out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]); // (FFFE,E000)
        out.extend_from_slice(&(frag.len() as u32).to_le_bytes());
        out.extend_from_slice(frag);
    }

    // Sequence Delimiter
    out.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]); // (FFFE,E0DD)
    out.extend_from_slice(&0u32.to_le_bytes());

    out
}

// ===========================================================================
// DicomCodec trait
// ===========================================================================

/// A DICOM transfer syntax transcoder.
///
/// # Contract
/// - `decode`: raw DICOM bytes -> decoded pixels (planar layout)
/// - `encode`: decoded pixels -> compressed DICOM pixel bytes
/// - `supported_source_ts`: additional TS UIDs this codec can decode
pub trait DicomCodec: Send + Sync {
    /// Return the primary Transfer Syntax UID this codec handles.
    fn transfer_syntax_uid(&self) -> &'static str;

    /// Human-readable name for diagnostics and logging.
    fn name(&self) -> &'static str {
        ""
    }

    /// Return additional source Transfer Syntax UIDs this codec can decode.
    ///
    /// Used when a codec can handle multiple source formats that share
    /// the same pixel data layout (e.g. Implicit VR LE -> Explicit VR LE).
    /// The registry checks these as a fallback when the exact TS UID
    /// is not found.
    fn supported_source_ts(&self) -> &[&str] {
        &[]
    }

    /// Whether this codec is decode-only (cannot encode).
    ///
    /// Decode-only codecs are NOT announced in transferCapabilities
    /// - we don't promise what we can't deliver.
    fn decode_only(&self) -> bool {
        false
    }

    /// Decode DICOM pixel data into raw uncompressed bytes.
    ///
    /// The `dicom_bytes` parameter is a reference to the full DICOM dataset.
    /// The implementation must extract and decompress the pixel data.
    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError>;

    /// Encode raw pixel bytes into this codec's transfer syntax.
    fn encode(&self, pixels: &DecodedPixels, meta: &DicomMeta) -> Result<Vec<u8>, CodecError>;

    /// Encode pixels into a **complete DICOM PixelData element**.
    ///
    /// Returns the full (7FE0,0010) element: tag + VR + length + value,
    /// properly formatted for this codec (native OW for uncompressed,
    /// encapsulated OB + Items for compressed).
    ///
    /// Default implementation: native OW with defined length (correct for
    /// EVRLE, EVRBE, IVRLE). Compressed codecs MUST override this to
    /// produce encapsulated format (OB + undefined length + Items).
    ///
    /// nginx-way: transcode_direct doesn't know about native vs encapsulated.
    /// Each codec encapsulates its own format knowledge.
    fn encode_pixeldata(
        &self,
        pixels: &DecodedPixels,
        meta: &DicomMeta,
    ) -> Result<Vec<u8>, CodecError> {
        let raw = self.encode(pixels, meta)?;
        Ok(build_native_ow_pixeldata(&raw))
    }
}

// ===========================================================================
// CodecRegistry - maps TS UID -> codec
// ===========================================================================

/// Registry mapping Transfer Syntax UID -> codec.
///
/// Thread-safe: all codecs implement `Send + Sync`.
/// Cheap to clone - uses `Arc` internally.
#[derive(Clone)]
pub struct CodecRegistry {
    codecs: HashMap<String, Arc<dyn DicomCodec>>,
}

impl CodecRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            codecs: HashMap::new(),
        }
    }

    /// Register a codec for its transfer syntax.
    pub fn register(&mut self, codec: Box<dyn DicomCodec>) {
        let ts_uid = codec.transfer_syntax_uid().to_string();
        self.codecs.insert(ts_uid, Arc::from(codec));
    }

    /// Get a codec by transfer syntax UID.
    ///
    /// First checks the exact TS UID. If not found, checks each codec's
    /// `supported_source_ts()` as a fallback (e.g. Implicit VR LE ->
    /// ExplicitVrLeCodec).
    pub fn get(&self, ts_uid: &str) -> Option<&dyn DicomCodec> {
        if let Some(codec) = self.codecs.get(ts_uid) {
            return Some(codec.as_ref());
        }
        // Fallback: check supported_source_ts for each codec
        for codec in self.codecs.values() {
            if codec.supported_source_ts().contains(&ts_uid) {
                return Some(codec.as_ref());
            }
        }
        None
    }

    /// Check if a transfer syntax is supported for transcoding.
    pub fn supports(&self, ts_uid: &str) -> bool {
        if self.codecs.contains_key(ts_uid) {
            return true;
        }
        // Fallback: check supported_source_ts
        self.codecs
            .values()
            .any(|c| c.supported_source_ts().contains(&ts_uid))
    }

    /// List all supported transfer syntax UIDs.
    pub fn supported_transfer_syntaxes(&self) -> Vec<&str> {
        self.codecs.keys().map(|s| s.as_str()).collect()
    }

    /// Return transfer syntax UIDs suitable for DICOMweb capabilities announcement.
    ///
    /// Filters out:
    /// - Uncompressed TS (always implicit per PS3.18 convention)
    /// - Decode-only codecs (don't promise what we can't deliver)
    ///
    /// Only codecs supporting BOTH decode AND encode are announced.
    ///
    /// The result is sorted lexicographically by Transfer Syntax UID so the
    /// announced list is deterministic across processes and builds. The
    /// underlying `HashMap` iteration order is per-process random (RandomState
    /// seed), which leaked into the rendered `/dicomweb/` capabilities JSON and
    /// broke byte-exact freeze tests in transcode builds.
    pub fn announceable_transfer_syntaxes(&self) -> Vec<&str> {
        let uncompressed = [
            ts_uid::IMPLICIT_VR_LE,
            ts_uid::EXPLICIT_VR_LE,
            ts_uid::EXPLICIT_VR_BE,
        ];
        let mut out: Vec<&str> = self
            .codecs
            .values()
            .filter(|c| !c.decode_only())
            .map(|c| c.transfer_syntax_uid())
            .filter(|ts| !uncompressed.contains(ts))
            .collect();
        out.sort_unstable();
        out
    }
}

// ===========================================================================
// Known unsupported Transfer Syntaxes (FR-029e)
// ===========================================================================

/// Known but unsupported Transfer Syntax UIDs with human-readable names.
/// Used for diagnostic error messages.
// NOTE: This is a best-effort hint for error messages, NOT a source of truth.
// Always check CodecRegistry::supports() before relying on this list.
// Some entries here may already be supported when features are enabled.
pub const KNOWN_UNSUPPORTED_TS: &[(&str, &str)] = &[
    ("1.2.840.10008.1.2.4.51", "JPEG Extended (Process 2 & 4)"),
    (
        "1.2.840.10008.1.2.4.92",
        "JPEG 2000 Part 2 Lossless (JPX) — not supported by any open-source library",
    ),
    (
        "1.2.840.10008.1.2.4.93",
        "JPEG 2000 Part 2 Lossy (JPX) — not supported by any open-source library",
    ),
    (
        "1.2.840.10008.1.2.4.100",
        "MPEG2 Main Profile / Main Level — video, not supported",
    ),
    (
        "1.2.840.10008.1.2.4.101",
        "MPEG2 Main Profile / High Level — video, not supported",
    ),
    (
        "1.2.840.10008.1.2.4.102",
        "MPEG-4 AVC/H.264 High Profile — video, not supported",
    ),
    (
        "1.2.840.10008.1.2.4.103",
        "MPEG-4 AVC/H.264 BD Profile — video, not supported",
    ),
    (
        "1.2.840.10008.1.2.1.99",
        "Deflated Explicit VR Little Endian — rarely used",
    ),
];

/// Look up a human-readable name for an unsupported Transfer Syntax UID.
pub fn unsupported_ts_name(uid: &str) -> Option<&'static str> {
    KNOWN_UNSUPPORTED_TS
        .iter()
        .find(|(u, _)| *u == uid)
        .map(|(_, name)| *name)
}

impl Default for CodecRegistry {
    fn default() -> Self {
        let mut registry = Self::new();
        // Base codecs - always present (decode raw pixels).
        registry.register(Box::new(explicit_vr_le::ExplicitVrLeCodec));
        registry.register(Box::new(explicit_vr_be::BigEndianCodec));

        // Compressed codecs - ALL behind transcode feature.
        // Advertise only what this build can actually deliver: the default
        // build reports CAPS = [] and passes bytes through untouched.
        #[cfg(feature = "transcode")]
        {
            // Implicit VR LE (encode target for Weasis SCP compatibility)
            registry.register(Box::new(implicit_vr_le::ImplicitVrLeCodec));

            // RLE Lossless (encode + decode)
            registry.register(Box::new(rle::RleCodec));

            // JPEG-LS Lossless + Near-Lossless (pure Rust, ~20 KB).
            registry.register(Box::new(jpegls::JpeglsLosslessCodec));
            registry.register(Box::new(jpegls::JpeglsNearLosslessCodec::new(2)));

            // JPEG Baseline + Lossless P14/P14.1 (decode + encode, ~300 KB)
            registry.register(Box::new(jpeg_baseline::JpegBaselineCodec));
            registry.register(Box::new(jpeg_lossless::JpegLosslessCodec));
            registry.register(Box::new(jpeg_lossless::JpegLosslessSv1Codec));

            // JPEG 2000 (lossless + lossy, ~280 KB)
            registry.register(Box::new(jpeg2000_native::Jpeg2000NativeLosslessCodec));
            registry.register(Box::new(jpeg2000_native::Jpeg2000NativeLossyCodec));

            // HTJ2K (decode-only: the same codestream parser, CAP-classified
            // block coder). .201 Lossless, .202 Lossless RPCL, .203 Lossy
            // (PS3.6 keywords HTJ2KLossless / HTJ2KLosslessRPCL / HTJ2K).
            // .204/.205 are JPIP reference forms, not a pixel-data encoding,
            // so they stay unregistered and 406 with an Available: list.
            registry.register(Box::new(jpeg2000_native::Htj2kLosslessCodec));
            registry.register(Box::new(jpeg2000_native::Htj2kLosslessRpclCodec));
            registry.register(Box::new(jpeg2000_native::Htj2kLossyCodec));
        }
        registry
    }
}

// ===========================================================================
// is_image_sop_class - non-image SOP skip for transcoding path
// ===========================================================================

/// Re-export from the authoritative source in `non_image_sop_classes`.
///
/// Returns true if the SOP Class UID represents an image class (has pixels).
/// Non-image classes (SR, PR, SEG, RTSTRUCT, Encapsulated PDF/CDA) should
/// skip transcoding - they have no pixel data to transcode.
///
/// See PS3.6 Table A.1-1 and PS3.4 Annex B.5.
pub use non_image_sop_classes::is_image_sop_class;

// ===========================================================================
// Shared parser helper - single place for DICOM PS3.5 Section 7.1 pad-byte skip
// ===========================================================================

/// Skip `len` value bytes + trailing pad byte if `len` is odd (DICOM PS3.5 Section 7.1).
#[inline]
pub(crate) fn skip_value(pos: &mut usize, len: usize) {
    *pos += len;
    if !len.is_multiple_of(2) {
        *pos += 1;
    }
}

/// Read BitsStored (0028,0101) US from an Explicit VR (LE) DICOM byte slice.
///
/// Returns `None` when the element is absent or unreadable. PS3.3
/// C.7.6.3.1.1 makes BitsStored the number of meaningful bits, which is not
/// BitsAllocated: a 12-bit CT is stored in 16 bits, and rewriting the declared
/// width as 16 changes what every consumer believes about the values.
pub(crate) fn bits_stored_of(data: &[u8]) -> Option<u8> {
    first_us_of(data, TAG_BITS_STORED).map(|v| v as u8)
}

/// Read PixelRepresentation (0028,0103) US from an Explicit VR (LE) DICOM
/// byte slice. Returns 0 (unsigned) when the tag is absent or unreadable -
/// PS3.3 C.7.6.3.1.1 defines 0 as the default for monochrome images.
///
/// Used by the transcode encode path: JPEG2000 encoding must pass
/// `signed = (pixel_representation == 1)` to j2k, otherwise signed
/// (PR=1) sources are encoded as unsigned and the pixel values shift.
pub(crate) fn pixel_representation_of(data: &[u8]) -> u16 {
    first_us_of(data, TAG_PIXEL_REPRESENTATION).unwrap_or(0)
}

/// Read the first `US` value of `target` from a DICOM dataset.
///
/// PS3.5 6.2 / 7.1: the element grammar is Implicit VR or Explicit VR, and the
/// byte order follows the transfer syntax; only one combination was read here
/// before, so an Implicit VR or Big Endian source yielded no value and the
/// caller silently fell back to 0 (unsigned), inverting every signed image.
fn first_us_of(data: &[u8], target: (u16, u16)) -> Option<u16> {
    let (implicit, be) = syntax_grammar(data);
    let u16_at = |s: &[u8]| {
        if be {
            u16::from_be_bytes([s[0], s[1]])
        } else {
            u16::from_le_bytes([s[0], s[1]])
        }
    };
    // PS3.10 7.1: the File Meta group is Explicit VR LITTLE ENDIAN whatever
    // the dataset syntax is, and it is not part of the dataset. Walking it
    // with the dataset grammar read its short-form lengths in the wrong byte
    // order (Big Endian source) or as a 4-byte length (Implicit VR source),
    // and in the latter case the walk never left the meta group at all.
    let mut pos = dataset_start(data);
    while pos + 8 <= data.len() {
        let g = u16_at(&data[pos..pos + 2]);
        let e = u16_at(&data[pos + 2..pos + 4]);
        let (len, value_start) = if implicit {
            // PS3.5 7.1: the value length follows the dataset byte order too.
            // Only the TAG order was honoured here, so a Big Endian source had
            // its 4-byte length read as little endian and the walk jumped to a
            // meaningless offset.
            (read_u32_grammar(data, pos + 4, be) as usize, pos + 8)
        } else {
            let vr = std::str::from_utf8(&data[pos + 4..pos + 6]).unwrap_or("");
            let ext = matches!(
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
            );
            if ext {
                if pos + 12 > data.len() {
                    break;
                }
                // The short-form lengths follow the dataset's byte order as
                // well (PS3.5 7.1) - only the VR letters are order-free.
                (read_u32_grammar(data, pos + 8, be) as usize, pos + 12)
            } else {
                (read_u16_grammar(data, pos + 6, be) as usize, pos + 8)
            }
        };
        if (g, e) == target {
            if len >= 2 && value_start + 2 <= data.len() {
                return Some(u16_at(&data[value_start..value_start + 2]));
            }
            return None;
        }
        if len == 0xFFFF_FFFF {
            // PS3.5 7.5: an undefined-length element is followed by an item
            // stream that ends at the Sequence Delimitation Item. Stopping
            // here answered `None` for every attribute after such a sequence,
            // so a file with e.g. a Referenced Image Sequence before
            // PixelRepresentation lost its signedness and every value was
            // reinterpreted as unsigned. Step over the stream instead.
            let mut at = value_start;
            if !skip_undefined_sequence(data, &mut at, implicit, be) {
                break;
            }
            pos = at;
            continue;
        }
        match value_start.checked_add(len) {
            Some(next) => pos = next + (len % 2),
            None => break,
        }
    }
    None
}

/// Step `pos` past an undefined-length element's item stream.
///
/// `pos` is expected to point at the first item tag. Returns false when the
/// stream is truncated or the delimiter is missing, which the caller treats as
/// "no more attributes" - the same conservative answer as reaching EOF.
fn skip_undefined_sequence(data: &[u8], pos: &mut usize, implicit: bool, be: bool) -> bool {
    let mut depth: u32 = 0;
    loop {
        if *pos + 8 > data.len() {
            return false;
        }
        let g = read_u16_grammar(data, *pos, be);
        let e = read_u16_grammar(data, *pos + 2, be);
        *pos += 4;
        match (g, e) {
            (0xFFFE, 0xE000) => {
                let item_len = read_u32_grammar(data, *pos, be) as usize;
                *pos += 4;
                if item_len == 0xFFFF_FFFF {
                    depth += 1;
                } else if *pos + item_len > data.len() {
                    return false;
                } else {
                    *pos += item_len;
                }
            }
            (0xFFFE, 0xE00D) => {
                *pos += 4;
                depth = depth.saturating_sub(1);
            }
            (0xFFFE, 0xE0DD) => {
                *pos += 4;
                if depth == 0 {
                    return true;
                }
                depth -= 1;
            }
            _ => {
                // A plain element inside an item: its own length, in the
                // dataset's grammar, tells where it ends.
                if implicit {
                    if *pos + 4 > data.len() {
                        return false;
                    }
                    let l = read_u32_grammar(data, *pos, be) as usize;
                    *pos += 4;
                    if l == 0xFFFF_FFFF {
                        depth += 1;
                    } else if *pos + l > data.len() {
                        return false;
                    } else {
                        *pos += l + (l % 2);
                    }
                } else {
                    if *pos + 2 > data.len() {
                        return false;
                    }
                    let vr = std::str::from_utf8(&data[*pos..*pos + 2]).unwrap_or("");
                    *pos += 2;
                    let ext = matches!(
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
                    );
                    let l = if ext {
                        if *pos + 6 > data.len() {
                            return false;
                        }
                        let l = read_u32_grammar(data, *pos + 2, be) as usize;
                        *pos += 6;
                        l
                    } else {
                        if *pos + 2 > data.len() {
                            return false;
                        }
                        let l = read_u16_grammar(data, *pos, be) as usize;
                        *pos += 2;
                        l
                    };
                    if l == 0xFFFF_FFFF {
                        depth += 1;
                    } else if *pos + l > data.len() {
                        return false;
                    } else {
                        *pos += l + (l % 2);
                    }
                }
            }
        }
    }
}

#[inline]
fn read_u16_grammar(data: &[u8], at: usize, be: bool) -> u16 {
    let pair = [data[at], data[at + 1]];
    if be {
        u16::from_be_bytes(pair)
    } else {
        u16::from_le_bytes(pair)
    }
}

#[inline]
fn read_u32_grammar(data: &[u8], at: usize, be: bool) -> u32 {
    let quad = [data[at], data[at + 1], data[at + 2], data[at + 3]];
    if be {
        u32::from_be_bytes(quad)
    } else {
        u32::from_le_bytes(quad)
    }
}

/// Byte offset of the first dataset element.
///
/// PS3.10 7.1: 128-byte preamble + "DICM", then group 0002 (File Meta
/// Information, Explicit VR LE, each element listed at 4-byte granularity).
/// A file without the prefix is treated as a bare dataset from offset 0.
fn dataset_start(data: &[u8]) -> usize {
    if data.len() < 132 || &data[128..132] != b"DICM" {
        return 0;
    }
    let mut pos = 132;
    while pos + 8 <= data.len() {
        let g = u16::from_le_bytes([data[pos], data[pos + 1]]);
        if g != 0x0002 {
            break;
        }
        let vr = std::str::from_utf8(&data[pos + 4..pos + 6]).unwrap_or("");
        let extended = matches!(
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
        );
        let (len, value_start) = if extended {
            if pos + 12 > data.len() {
                return pos;
            }
            (
                u32::from_le_bytes([data[pos + 8], data[pos + 9], data[pos + 10], data[pos + 11]])
                    as usize,
                pos + 12,
            )
        } else {
            (
                u16::from_le_bytes([data[pos + 6], data[pos + 7]]) as usize,
                pos + 8,
            )
        };
        pos = value_start.saturating_add(len + (len % 2));
    }
    pos
}

/// `(is_implicit_vr, is_big_endian)` for a Part 10 file.
///
/// PS3.10 7.1: the meta group is Explicit VR LE, so it is read first.
fn syntax_grammar(data: &[u8]) -> (bool, bool) {
    if data.len() < 132 || &data[128..132] != b"DICM" {
        return (false, false);
    }
    let mut pos = 132;
    while pos + 8 <= data.len() {
        let g = u16::from_le_bytes([data[pos], data[pos + 1]]);
        if g != 0x0002 {
            break;
        }
        let e = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        let vr = std::str::from_utf8(&data[pos + 4..pos + 6]).unwrap_or("");
        let (len, vstart) = if matches!(
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
            if pos + 12 > data.len() {
                break;
            }
            (
                u32::from_le_bytes([data[pos + 8], data[pos + 9], data[pos + 10], data[pos + 11]])
                    as usize,
                pos + 12,
            )
        } else {
            (
                u16::from_le_bytes([data[pos + 6], data[pos + 7]]) as usize,
                pos + 8,
            )
        };
        if (g, e) == (0x0002, 0x0010) && vstart + len <= data.len() {
            let ts = String::from_utf8_lossy(&data[vstart..vstart + len]);
            return match ts.trim().trim_end_matches('\0') {
                "1.2.840.10008.1.2" => (true, false),
                "1.2.840.10008.1.2.2" => (false, true),
                _ => (false, false),
            };
        }
        pos = vstart.saturating_add(len + (len % 2));
    }
    (false, false)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// PS3.5 A.4.2: a colour frame is one codestream per component, so a
    /// 3-fragment single-frame RGB image is three WHOLE codestreams. Joining
    /// them left one blob, `len() / 3` rounded to zero frames, and the decode
    /// failed - an ordinary colour JPEG-LS image no viewer could open.
    #[test]
    fn coalescing_keeps_one_codestream_per_component() {
        let r = vec![1u8, 1, 1];
        let g = vec![2u8, 2, 2];
        let b = vec![3u8, 3, 3];

        // Three fragments, three components, one frame: no coalescing.
        let kept = coalesce_single_frame_fragments(vec![r.clone(), g.clone(), b.clone()], 1, 3);
        assert_eq!(kept.len(), 3, "one codestream per component");

        // One component split across three fragments: coalesce into one.
        let joined =
            coalesce_single_frame_fragments(vec![vec![1u8, 2], vec![3u8, 4], vec![5u8, 6]], 1, 1);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0], vec![1u8, 2, 3, 4, 5, 6]);

        // More than one frame: the fragment-to-frame mapping is the only thing
        // identifying a frame, so nothing is joined.
        let multi = coalesce_single_frame_fragments(vec![r, g, b], 3, 1);
        assert_eq!(multi.len(), 3);
    }

    #[test]
    fn test_registry_default_contains_base_codecs() {
        let registry = CodecRegistry::default();
        // Base codecs - always present (no feature flag needed).
        assert!(registry.supports(ts_uid::EXPLICIT_VR_LE));
        assert!(registry.supports(ts_uid::EXPLICIT_VR_BE));
        // Compressed codecs (RLE, JPEG-LS, etc.) - only with transcode feature.
        #[cfg(feature = "transcode")]
        assert!(registry.supports(ts_uid::RLE_LOSSLESS));
    }

    /// Append one Explicit VR LE element with a `US` value.
    fn push_us(buf: &mut Vec<u8>, tag: (u16, u16), value: u16) {
        buf.extend_from_slice(&tag.0.to_le_bytes());
        buf.extend_from_slice(&tag.1.to_le_bytes());
        buf.extend_from_slice(b"US");
        buf.extend_from_slice(&2u16.to_le_bytes());
        buf.extend_from_slice(&value.to_le_bytes());
    }

    /// A Part 10 header with the Explicit VR LE transfer syntax.
    fn evrle_header() -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&[0u8; 128]);
        d.extend_from_slice(b"DICM");
        let ts = ts_uid::EXPLICIT_VR_LE.as_bytes();
        d.extend_from_slice(&0x0002u16.to_le_bytes());
        d.extend_from_slice(&0x0010u16.to_le_bytes());
        d.extend_from_slice(b"UI");
        d.extend_from_slice(&(ts.len() as u16).to_le_bytes());
        d.extend_from_slice(ts);
        // PS3.5 7.1: an odd-length UI value carries a NUL pad byte OUTSIDE the
        // declared length. The transfer syntax UID is 19 characters, so
        // without it every following element is off by one byte.
        if !ts.len().is_multiple_of(2) {
            d.push(0);
        }
        d
    }

    /// A top-level undefined-length sequence with one item, then `tail`.
    ///
    /// PS3.5 7.5: the item stream is delimited by (FFFE,E00D) per item and
    /// (FFFE,E0DD) for the sequence itself.
    fn push_undefined_sq(buf: &mut Vec<u8>, tag: (u16, u16), tail: &[u8]) {
        buf.extend_from_slice(&tag.0.to_le_bytes());
        buf.extend_from_slice(&tag.1.to_le_bytes());
        buf.extend_from_slice(b"SQ");
        buf.extend_from_slice(&[0u8, 0u8]);
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // Item, undefined length, one NESTED undefined-length sequence inside
        // it, then the item delimiter.
        buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        buf.extend_from_slice(&0x0008u16.to_le_bytes());
        buf.extend_from_slice(&0x1140u16.to_le_bytes());
        buf.extend_from_slice(b"SQ");
        buf.extend_from_slice(&[0u8, 0u8]);
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        buf.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&[0xFE, 0xFF, 0x0D, 0xE0]);
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(tail);
        buf.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
        buf.extend_from_slice(&0u32.to_le_bytes());
    }

    /// An undefined-length sequence must not end the walk.
    ///
    /// PS3.5 7.5 puts attributes after a sequence all the time. Stopping at
    /// the first (FFFE,E0DD) - which belongs to the sequence - made every
    /// later attribute invisible, so a signed image whose
    /// PixelRepresentation followed a Referenced Image Sequence was encoded as
    /// unsigned and every value shifted.
    #[test]
    fn an_undefined_sequence_does_not_hide_the_attributes_after_it() {
        let mut d = evrle_header();
        push_undefined_sq(&mut d, (0x0008, 0x1115), &[]);
        push_us(&mut d, TAG_PIXEL_REPRESENTATION, 1);
        push_us(&mut d, TAG_BITS_STORED, 12);

        assert_eq!(
            pixel_representation_of(&d),
            1,
            "the attribute after the sequence is read"
        );
        assert_eq!(bits_stored_of(&d), Some(12));
    }

    /// The nested sequence's delimiter must not be taken for the outer one.
    ///
    /// The inner (FFFE,E0DD) closes a sequence INSIDE an item; counting it as
    /// the end of the outer element left the cursor in the middle of the item
    /// stream and the next read started from a value byte.
    #[test]
    fn a_nested_sequence_delimiter_does_not_close_the_outer_one() {
        let mut d = evrle_header();
        push_us(&mut d, TAG_BITS_STORED, 8);
        push_undefined_sq(&mut d, (0x0008, 0x1115), &[]);
        push_us(&mut d, TAG_PIXEL_REPRESENTATION, 1);

        assert_eq!(bits_stored_of(&d), Some(8));
        assert_eq!(
            pixel_representation_of(&d),
            1,
            "the nested delimiter is consumed, not mistaken for the outer end"
        );
    }

    /// A truncated sequence stops the walk instead of guessing.
    #[test]
    fn a_truncated_undefined_sequence_yields_no_value() {
        let mut d = evrle_header();
        d.extend_from_slice(&0x0008u16.to_le_bytes());
        d.extend_from_slice(&0x1115u16.to_le_bytes());
        d.extend_from_slice(b"SQ");
        d.extend_from_slice(&[0u8, 0u8]);
        d.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // An item claiming 0x100 bytes with nothing behind it.
        d.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        d.extend_from_slice(&0x100u32.to_le_bytes());
        d.extend_from_slice(&[0u8; 4]);

        assert_eq!(pixel_representation_of(&d), 0, "no value is invented");
    }

    #[cfg(feature = "transcode")]
    #[test]
    fn test_registry_default_contains_transcode_codecs() {
        let registry = CodecRegistry::default();
        assert!(registry.supports(ts_uid::JPEG_BASELINE));
        assert!(registry.supports(ts_uid::JPEGLS_LOSSLESS));
        assert!(registry.supports(ts_uid::JPEG2000_LOSSLESS));
    }

    #[test]
    fn test_registry_supports_implicit_vr_le() {
        let registry = CodecRegistry::default();
        // Implicit VR LE is supported via ExplicitVrLeCodec's supported_source_ts
        // AND via dedicated ImplicitVrLeCodec (when transcode feature enabled).
        assert!(registry.supports(ts_uid::IMPLICIT_VR_LE));
        let codec = registry.get(ts_uid::EXPLICIT_VR_LE);
        assert!(codec.is_some());
        assert_eq!(codec.unwrap().transfer_syntax_uid(), ts_uid::EXPLICIT_VR_LE);
    }

    #[test]
    fn test_registry_register_and_get() {
        let mut registry = CodecRegistry::new();
        registry.register(Box::new(explicit_vr_le::ExplicitVrLeCodec));
        assert!(registry.get(ts_uid::EXPLICIT_VR_LE).is_some());
        assert!(registry.get("1.2.840.10008.1.2.4.70").is_none());
    }

    /// The announceable list must be lexicographically sorted and stable even
    /// though the backing `HashMap` iterates in per-process random order
    /// (RandomState seed). This is what keeps the `/dicomweb/` capabilities
    /// byte-exact in transcode builds.
    #[test]
    fn announceable_transfer_syntaxes_sorted_and_stable() {
        // Empty registry -> empty announceable.
        let empty = CodecRegistry::new();
        assert!(empty.announceable_transfer_syntaxes().is_empty());

        // Default registry -> base codecs are uncompressed and filtered out
        // (so still empty unless transcode is compiled in).
        let base = CodecRegistry::default();
        let base_list = base.announceable_transfer_syntaxes();

        #[cfg(feature = "transcode")]
        {
            // Non-empty: every member announced, sorted lexicographically, and
            // repeats are byte-identical across two calls (stable, not
            // HashMap-order-dependent).
            assert!(!base_list.is_empty());
            let mut sorted = base_list.clone();
            sorted.sort_unstable();
            assert_eq!(base_list, sorted, "announceable must be pre-sorted");
            assert_eq!(base_list, base.announceable_transfer_syntaxes(), "unstable");
            let mut dedup = base_list.clone();
            dedup.dedup();
            assert_eq!(base_list.len(), dedup.len(), "no duplicate TS announced");
        }

        #[cfg(not(feature = "transcode"))]
        {
            // Default build: only uncompressed base codecs exist -> filtered
            // to an empty announceable list.
            assert!(base_list.is_empty());
        }
    }

    #[test]
    fn test_decoded_pixels_validate_ok() {
        let pixels = DecodedPixels {
            data: vec![0u8; 100 * 100],
            rows: 100,
            columns: 100,
            samples_per_pixel: 1,
            bits_allocated: 8,
            photometric_interpretation: "MONOCHROME2".into(),
        };
        assert!(pixels.validate().is_ok());
    }

    #[test]
    fn test_decoded_pixels_validate_size_mismatch() {
        let pixels = DecodedPixels {
            data: vec![0u8; 50], // Too small for 100x100
            rows: 100,
            columns: 100,
            samples_per_pixel: 1,
            bits_allocated: 8,
            photometric_interpretation: "MONOCHROME2".into(),
        };
        assert!(pixels.validate().is_err());
    }

    #[test]
    fn test_decoded_pixels_validate_invalid_samples() {
        let pixels = DecodedPixels {
            data: vec![0u8; 100],
            rows: 10,
            columns: 10,
            samples_per_pixel: 4, // Invalid
            bits_allocated: 8,
            photometric_interpretation: "RGB".into(),
        };
        assert!(pixels.validate().is_err());
    }

    #[test]
    fn test_unsupported_ts_name_jpeg_extended() {
        assert_eq!(
            unsupported_ts_name("1.2.840.10008.1.2.4.51"),
            Some("JPEG Extended (Process 2 & 4)")
        );
    }

    #[test]
    fn test_unsupported_ts_name_mpeg() {
        assert_eq!(
            unsupported_ts_name("1.2.840.10008.1.2.4.100"),
            Some("MPEG2 Main Profile / Main Level — video, not supported")
        );
    }

    #[test]
    fn test_unsupported_ts_name_unknown() {
        assert_eq!(unsupported_ts_name("1.2.840.10008.1.2.1"), None);
        assert_eq!(unsupported_ts_name("nonexistent"), None);
    }

    #[test]
    fn test_unsupported_ts_name_all_known() {
        // Verify all entries have non-empty names
        for (uid, name) in KNOWN_UNSUPPORTED_TS {
            assert!(!uid.is_empty(), "Empty UID in KNOWN_UNSUPPORTED_TS");
            assert!(!name.is_empty(), "Empty name for UID {uid}");
        }
    }

    // -- StubParser: test double for DicomParser trait ------------------

    use clarus_ports::dicom::{DicomError, DicomParser, DicomTag, InstanceMeta};
    use std::collections::HashMap;

    /// Minimal `DicomParser` implementation for codec unit tests.
    /// Returns canned metadata without parsing actual DICOM bytes.
    struct StubParser {
        metadata: InstanceMeta,
    }

    impl StubParser {
        fn new() -> Self {
            Self {
                metadata: InstanceMeta {
                    patient_name: "TEST^PATIENT".into(),
                    patient_id: "TEST001".into(),
                    study_instance_uid: "1.2.840.10008.1.1".into(),
                    series_instance_uid: "1.2.840.10008.1.2".into(),
                    sop_instance_uid: "1.2.840.10008.1.3".into(),
                    modality: "OT".into(),
                    ..Default::default()
                },
            }
        }
    }

    impl DicomParser for StubParser {
        fn extract_metadata(&self, _data: &[u8]) -> Result<InstanceMeta, DicomError> {
            Ok(self.metadata.clone())
        }

        fn extract_search_tags(
            &self,
            _data: &[u8],
        ) -> Result<HashMap<DicomTag, String>, DicomError> {
            let mut tags = HashMap::new();
            tags.insert((0x0010, 0x0010), "TEST^PATIENT".into());
            tags.insert((0x0010, 0x0020), "TEST001".into());
            Ok(tags)
        }
    }

    #[test]
    fn test_stub_parser_extract_metadata() {
        let stub = StubParser::new();
        let meta = stub.extract_metadata(b"fake dicom").unwrap();
        assert_eq!(meta.patient_name, "TEST^PATIENT");
        assert_eq!(meta.patient_id, "TEST001");
        assert_eq!(meta.sop_instance_uid, "1.2.840.10008.1.3");
    }
}
