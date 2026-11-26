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
/// PS3.5 6.2 VR width classes and the Explicit VR header rules.
mod vr_widths;
/// The same statement for the frame indexer's on-wire header walk. Its own
/// list omitted `SV` and `UV`, which is the class of bug this single source of
/// truth exists to prevent - a VR added after the list was written takes the
/// short branch and the walk desynchronises by two bytes.
pub(crate) use vr_widths::is_long_form_vr;
/// Re-exported so the DICOM test fixtures decide long vs short form from the
/// SAME statement the reader uses. Three local lists used to duplicate it in
/// `dicom_rs::parser`, and each omitted SV, UV and OV - so a fixture built with
/// any of them got a short-form header and the reader decoded the reserved
/// bytes as a zero length. Only the fixture builders call it today, hence the
/// test gate.
#[cfg(test)]
pub(crate) use vr_widths::is_long_form_vr_str;

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
    /// JPEG-LS Lossless. Requires `--features transcode`.
    pub const JPEGLS_LOSSLESS: &str = "1.2.840.10008.1.2.4.80";
    /// JPEG-LS Near-Lossless. Requires `--features transcode`.
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
        // PS3.5 8.1.1: Bits Allocated shall either be 1, or a multiple of 8.
        // The check below used to admit any value from 8 upward, so a
        // BitsAllocated of 12 passed and `>> 3` then sized the image as if the
        // container were 8 bits - a half-length buffer validated as complete.
        // 12-bit CT stores its samples in a 16-bit container, so the attribute
        // itself must still be 16. 1 is kept as its own legal case here so the
        // packed-form refusal below keeps its own message.
        if self.bits_allocated == 0
            || (self.bits_allocated < 8 && self.bits_allocated != 1)
            || (self.bits_allocated > 1 && !self.bits_allocated.is_multiple_of(8))
        {
            return Err(CodecError::InvalidMetadata(format!(
                "BitsAllocated must be 1 or a multiple of 8, got {}",
                self.bits_allocated
            )));
        }
        if self.bits_allocated == 1 {
            return Err(CodecError::InvalidMetadata(
                "Bit-packed (BitsAllocated=1) images are not supported by this encoder".into(),
            ));
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
    // PS3.5 8.1: the Value Field must be an even number of bytes, and the pad
    // "shall be appended to the end of the Value Field" - so the declared
    // length counts it. Declaring the odd sample count and omitting the byte
    // left a reader that trusts the length one byte short of the element end,
    // and a strict reader rejecting the file.
    let pad = usize::from(!raw_pixels.len().is_multiple_of(2));
    // The Value Length field is 32 bits, so a native value at or above 4 GiB
    // cannot be described by this header at all - PS3.5 8.1 notes the limit
    // itself. The cast used to wrap, so a 4 GiB value declared a length of 0
    // and every reader took the samples for the next element's header.
    //
    // This builder has no error channel, so the bound is asserted rather than
    // returned: reaching it means a caller handed over a native value the
    // 32-bit field cannot express, which is a programming error and not
    // something to paper over with a truncated header.
    let declared = u32::try_from(raw_pixels.len().saturating_add(pad)).expect(
        "native PixelData above the 32-bit Value Length limit cannot be written; \
         encapsulated transfer syntaxes exist for values this large",
    );
    let mut elem = Vec::with_capacity(12 + raw_pixels.len() + pad);
    elem.extend_from_slice(&0x7FE0u16.to_le_bytes());
    elem.extend_from_slice(&0x0010u16.to_le_bytes());
    elem.extend_from_slice(b"OW");
    elem.extend_from_slice(&[0x00, 0x00]); // reserved
    elem.extend_from_slice(&declared.to_le_bytes());
    elem.extend_from_slice(raw_pixels);
    if pad == 1 {
        elem.push(0x00);
    }
    elem
}

/// Build an encapsulated (compressed) PixelData element.
///
/// PS3.5 A.4: "the Data Elements ... shall be encoded in Explicit VR Little
/// Endian" for the encapsulated transfer syntaxes, so there is no implicit
/// form to write and no flag to select one. An implicit header here is a
/// sequence of bytes no conformant reader expects: the two reserved zero bytes
/// read as the top half of a 32-bit length.
pub fn build_encapsulated_pixeldata(fragment: &[u8]) -> Vec<u8> {
    // PS3.5 A.4: every Item's Value is padded to an even length, and that pad
    // counts in the Item's declared length. It is EVEN, not four: the value of
    // a Fragment is a piece of a compressed codestream, so any byte pushed into
    // it becomes part of the codestream a decoder reassembles. Aligning to 4
    // inserted up to three zero bytes into a stream split on an odd boundary -
    // a JPEG-LS codestream cut after six bytes gained two.
    let pad = usize::from(!fragment.len().is_multiple_of(2));
    let padded_len = fragment
        .len()
        .checked_add(pad)
        .expect("len + 1 cannot overflow");

    let total = 4 + 2 + 2 + 4   // Tag + VR + reserved + undefined len
        + 4 + 4                  // BOT: empty Item (FFFE,E000 + 0x00000000)
        + 4 + 4 + padded_len     // Fragment Item: tag + len + data
        + 4 + 4; // Sequence Delimiter
    let mut out = Vec::with_capacity(total);

    // Tag (7FE0,0010)
    out.extend_from_slice(&0x7FE0u16.to_le_bytes());
    out.extend_from_slice(&0x0010u16.to_le_bytes());
    // VR "OB"
    out.extend_from_slice(b"OB");
    // Reserved (0x0000)
    out.extend_from_slice(&[0x00, 0x00]);
    // Undefined length (0xFFFF_FFFF)
    out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    // BOT - empty Item (length = 0). Required by DICOM PS3.5.
    out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]); // (FFFE,E000)
    out.extend_from_slice(&0u32.to_le_bytes());
    // Fragment Item
    out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]); // (FFFE,E000)
    out.extend_from_slice(
        &u32::try_from(padded_len)
            .expect("fragment fits 32 bits")
            .to_le_bytes(),
    );
    out.extend_from_slice(fragment);
    if pad == 1 {
        out.push(0x00);
    }
    // Sequence Delimiter
    out.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]); // (FFFE,E0DD)
    out.extend_from_slice(&0u32.to_le_bytes());

    out
}

/// PS3.5 A.4: one Basic Offset Table entry per frame, in ONE pass.
///
/// An entry is the byte offset of the frame's first Item, counted from the
/// first byte of the first Item that FOLLOWS the table. The previous loop
/// summed every preceding fragment for every frame - quadratic, 4.9e7 visits
/// for 10,000 one-fragment frames - and its `saturating_add` wrote
/// 0xFFFF_FFFF as a real offset once the total passed the 32-bit range, which
/// is a table entry a reader accepts and then seeks to nonsense. The running
/// total is kept once, and the conversion is checked.
fn basic_offset_table_entries(
    item_sizes: &[usize],
    bot_stride: usize,
    bot_entry_count: usize,
) -> Result<Vec<u32>, CodecError> {
    let mut offsets: Vec<u32> = Vec::with_capacity(bot_entry_count);
    if bot_entry_count == 0 {
        return Ok(offsets);
    }
    let mut running: u64 = 0;
    let mut next_frame_item = 0usize;
    for (idx, len) in item_sizes.iter().enumerate() {
        if idx == next_frame_item {
            let offset = u32::try_from(running).map_err(|_| {
                CodecError::EncodeFailed(format!(
                    "Basic Offset Table entry {idx} is {running} bytes, beyond the 32-bit range"
                ))
            })?;
            offsets.push(offset);
            next_frame_item += bot_stride;
        }
        // 8 header bytes + the Item's own length.
        running += 8 + *len as u64;
    }
    Ok(offsets)
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
    split_stream: bool,
) -> Vec<Vec<u8>> {
    if frames != 1 || fragments.len() <= 1 {
        return fragments;
    }
    // PS3.5 A.4: "Whether more than one Fragment per Frame is permitted or not
    // is defined per Transfer Syntax." So the fragment count says nothing on
    // its own, and neither does the component count: a single interleaved RGB
    // codestream may be split across three fragments just as three separate
    // component codestreams may each occupy one. Only the caller's codec knows
    // which shape its syntax produces, so it passes `split_stream` - the
    // decision was made by comparing the fragment count against the component
    // count, which is right exactly when the two coincide and silently wrong
    // otherwise.
    if !split_stream {
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
) -> Result<Vec<u8>, CodecError> {
    // PS3.5 A.4: an Item's Value is padded to an EVEN length and that pad counts
    // in its declared length. It is not four: a Fragment holds a piece of a
    // compressed codestream, so the pad joins the codestream a decoder joins
    // back together. Aligning to 4 wrote up to three zero bytes into the middle
    // of a stream split on an odd boundary. Only the final fragment may carry
    // anything the encoder did not emit, which is what an even-length pad is.
    let padded: Vec<Vec<u8>> = fragments
        .iter()
        .map(|f| {
            let mut p = Vec::with_capacity(f.len() + 1);
            p.extend_from_slice(f);
            if !p.len().is_multiple_of(2) {
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

    let item_sizes: Vec<usize> = padded.iter().map(|f| f.len()).collect();
    let offsets = basic_offset_table_entries(&item_sizes, bot_stride, bot_entry_count)?;

    let data_len: usize = padded.iter().map(|f| f.len()).sum();
    let total = 4 + 2 + 2 + 4 // Tag + VR + reserved + undefined len
        + 8 + bot_entry_count * 4 // BOT item
        + padded.len() * 8 + data_len // Fragment items
        + 8; // Sequence Delimiter
    let mut out = Vec::with_capacity(total);

    // Tag (7FE0,0010)
    out.extend_from_slice(&0x7FE0u16.to_le_bytes());
    out.extend_from_slice(&0x0010u16.to_le_bytes());
    // PS3.5 A.4 fixes the encapsulated encoding to Explicit VR Little Endian,
    // so the header is always VR(2) + reserved(2) + undefined length.
    out.extend_from_slice(b"OB");
    out.extend_from_slice(&[0x00, 0x00]);
    out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());

    // BOT item
    out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]); // (FFFE,E000)
    if bot_entry_count > 0 {
        out.extend_from_slice(&((bot_entry_count * 4) as u32).to_le_bytes());
        for off in &offsets {
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

    Ok(out)
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

/// How a dataset walk ended with respect to one attribute.
///
/// The three cases are kept apart because they are not the same statement.
/// PS3.3 C.7.6.3.1.1 gives 0 (unsigned) for an ABSENT Pixel Representation, so
/// absence has a defined default. A PRESENT attribute whose walk failed has no
/// such answer: the file declares a signedness this reader could not determine,
/// and substituting "unsigned" there inverts every negative sample.
pub(crate) enum FieldOutcome<T> {
    Present(T),
    Absent,
    Unreadable,
}

/// Read PixelRepresentation (0028,0103) with the two cases kept apart.
///
/// The transcode encode path uses this: JPEG2000 passes
/// `signed = (pixel_representation == 1)` to j2k, so an unreadable value is
/// refused rather than encoded as unsigned, while an absent one takes the
/// PS3.3 C.7.6.3.1.1 default.
pub(crate) fn pixel_representation_checked(data: &[u8]) -> Result<u16, CodecError> {
    match first_us_outcome(data, TAG_PIXEL_REPRESENTATION) {
        FieldOutcome::Present(value) => Ok(value),
        FieldOutcome::Absent => Ok(0),
        FieldOutcome::Unreadable => Err(CodecError::InvalidMetadata(
            "PixelRepresentation (0028,0103) is present but unreadable".into(),
        )),
    }
}

/// Read the first `US` value of `target` from a DICOM dataset.
///
/// PS3.5 6.2 / 7.1: the element grammar is Implicit VR or Explicit VR, and the
/// byte order follows the transfer syntax; only one combination was read here
/// before, so an Implicit VR or Big Endian source yielded no value and the
/// caller silently fell back to 0 (unsigned), inverting every signed image.
fn first_us_of(data: &[u8], target: (u16, u16)) -> Option<u16> {
    match first_us_outcome(data, target) {
        FieldOutcome::Present(v) => Some(v),
        // An unreadable value reads as absent for the lenient callers. The
        // strict one (`pixel_representation_checked`) asks the outcome
        // directly, because for signedness the two are not the same statement.
        FieldOutcome::Absent | FieldOutcome::Unreadable => None,
    }
}

/// [`first_us_of`] without collapsing "absent" into "unreadable".
fn first_us_outcome(data: &[u8], target: (u16, u16)) -> FieldOutcome<u16> {
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
            let ext = vr_widths::is_long_form_vr(vr.as_bytes());
            if ext {
                if pos + 12 > data.len() {
                    // A header cut off inside the long form: the walk can no
                    // longer say whether `target` is present, which is exactly
                    // the "present but unreadable" outcome.
                    return FieldOutcome::Unreadable;
                }
                // The short-form lengths follow the dataset's byte order as
                // well (PS3.5 7.1) - only the VR letters are order-free.
                (read_u32_grammar(data, pos + 8, be) as usize, pos + 12)
            } else {
                (read_u16_grammar(data, pos + 6, be) as usize, pos + 8)
            }
        };
        if (g, e) == target {
            // PS3.3 C.7.6.3.1.1: Pixel Representation is a single US, so a
            // complete value is exactly two bytes. `len >= 2` accepted a
            // declared four with two present and read the first word as if the
            // value were whole, and any value at all was returned - including
            // 2, which the standard does not define. A short, over-long or
            // off-enumeration value is unreadable, not a signedness.
            if len != 2 {
                return FieldOutcome::Unreadable;
            }
            let Some(value) = value_start
                .checked_add(2)
                .filter(|end| *end <= data.len())
                .map(|_| u16_at(&data[value_start..value_start + 2]))
            else {
                return FieldOutcome::Unreadable;
            };
            // The 0/1 domain belongs to Pixel Representation alone: PS3.3
            // C.7.6.3.1.1 defines exactly those two values for signedness. It
            // must NOT be applied here, because this walker also reads Bits
            // Stored, where any width is legal and 12 would be refused.
            if target == TAG_PIXEL_REPRESENTATION && value > 1 {
                return FieldOutcome::Unreadable;
            }
            return FieldOutcome::Present(value);
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
                return FieldOutcome::Unreadable;
            }
            pos = at;
            continue;
        }
        match value_start.checked_add(len) {
            // PS3.5 6.2: the declared Value Length is already even; adding the
            // pad again (`len % 2`) skipped a byte past every odd-length
            // element. The pad is inside the number, not after it.
            Some(next) if next <= data.len() => pos = next,
            _ => return FieldOutcome::Unreadable,
        }
    }
    // The loop ends when a whole 8-byte header no longer fits. A value that
    // overran the buffer already returned `Unreadable` above, so reaching here
    // means every element the walk stepped was whole. A tail shorter than a
    // header is normal - the last element's value is not necessarily followed by
    // another header - so it is not evidence of truncation and `Absent` stands.
    FieldOutcome::Absent
}

/// Step `pos` past an undefined-length element's item stream.
///
/// `pos` is expected to point at the first item tag. Returns false when the
/// stream is truncated or the delimiter is missing, which the caller treats as
/// "no more attributes" - the same conservative answer as reaching EOF.
fn skip_undefined_sequence(data: &[u8], pos: &mut usize, implicit: bool, be: bool) -> bool {
    // Two independent nesting counts. `item_depth` counts undefined-length
    // Items, which close at (FFFE,E00D); `sq_depth` counts undefined-length
    // Sequences, which close at (FFFE,E0DD). One counter for both made a nested
    // sequence's E0DD decrement the item level as well, so the outer sequence
    // appeared to end at the inner delimiter and the walk resumed inside the
    // item stream - where the next four bytes were read as a tag.
    let mut item_depth: u32 = 0;
    let mut sq_depth: u32 = 1; // this call is stepping over one sequence
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
                    item_depth += 1;
                } else if *pos + item_len > data.len() {
                    return false;
                } else {
                    *pos += item_len;
                }
            }
            (0xFFFE, 0xE00D) => {
                *pos += 4;
                item_depth = item_depth.saturating_sub(1);
            }
            (0xFFFE, 0xE0DD) => {
                *pos += 4;
                sq_depth -= 1;
                if sq_depth == 0 {
                    return true;
                }
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
                        // An undefined-length element here is a sequence; its
                        // own E0DD closes it, one level deeper than ours.
                        sq_depth += 1;
                    } else if *pos + l > data.len() {
                        return false;
                    } else {
                        *pos += l;
                    }
                } else {
                    if *pos + 2 > data.len() {
                        return false;
                    }
                    let vr = std::str::from_utf8(&data[*pos..*pos + 2]).unwrap_or("");
                    *pos += 2;
                    let ext = vr_widths::is_long_form_vr(vr.as_bytes());
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
                        sq_depth += 1;
                    } else if *pos + l > data.len() {
                        return false;
                    } else {
                        // PS3.5 6.2: the declared length already includes the
                        // pad, so adding `l % 2` again skipped one byte past
                        // every odd-length element.
                        *pos += l;
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
        let extended = vr_widths::is_long_form_vr(vr.as_bytes());
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
        // PS3.5 6.2: the declared Value Length is already even - the pad byte
        // counts INSIDE it (`standard/ps3.5/vr-width-classes-6.2-1.json`,
        // padIsInsideTheDeclaredLength). Adding `len % 2` again skipped one byte
        // past a conforming element. The old code only worked because the test
        // fixtures declared an ODD length and appended the pad outside it, which
        // is the non-conformance this walker is supposed to refuse.
        pos = value_start.saturating_add(len);
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
        // PS3.10 7.1: the meta group is Explicit VR LE whatever the dataset
        // syntax is, so only the WIDTH CLASS varies here - the byte order is
        // fixed. The class still comes from the shared table.
        let (len, vstart) = if vr_widths::is_long_form_vr(vr.as_bytes()) {
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
        let kept = coalesce_single_frame_fragments(vec![r.clone(), g.clone(), b.clone()], 1, false);
        assert_eq!(kept.len(), 3, "one codestream per component");

        // One component split across three fragments: coalesce into one.
        let joined = coalesce_single_frame_fragments(
            vec![vec![1u8, 2], vec![3u8, 4], vec![5u8, 6]],
            1,
            true,
        );
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0], vec![1u8, 2, 3, 4, 5, 6]);

        // More than one frame: the fragment-to-frame mapping is the only thing
        // identifying a frame, so nothing is joined.
        let multi = coalesce_single_frame_fragments(vec![r, g, b], 3, true);
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
        // PS3.5 6.2: the Value Field is padded to an even length and that even
        // length is what the Value Length declares
        // (`standard/ps3.5/vr-width-classes-6.2-1.json`, padIsInsideTheDeclaredLength).
        // The transfer syntax UID is 19 characters, so the declared length is 20
        // and the NUL is the twentieth byte - declaring 19 and appending the pad
        // separately wrote a File Meta element that declared less than it
        // occupied. The parser compensated, so the sequence tests below were
        // exercising a malformed meta element rather than the conforming one.
        let declared = ts.len() + usize::from(!ts.len().is_multiple_of(2));
        d.extend_from_slice(&0x0002u16.to_le_bytes());
        d.extend_from_slice(&0x0010u16.to_le_bytes());
        d.extend_from_slice(b"UI");
        d.extend_from_slice(&(declared as u16).to_le_bytes());
        d.extend_from_slice(ts);
        if declared != ts.len() {
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
            pixel_representation_checked(&d).expect("readable"),
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
            pixel_representation_checked(&d).expect("readable"),
            1,
            "the nested delimiter is consumed, not mistaken for the outer end"
        );
    }

    /// A truncated sequence stops the walk instead of guessing.
    ///
    /// The walk cannot reach the rest of the dataset, so it cannot say whether
    /// PixelRepresentation is there. That is "unreadable", not "absent": the
    /// lenient readers still answer 0 (the PS3.3 C.7.6.3.1.1 default for an
    /// absent attribute), while the strict one refuses rather than encoding a
    /// possibly-signed image as unsigned.
    #[test]
    fn a_truncated_undefined_sequence_is_unreadable_not_absent() {
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

        let err =
            pixel_representation_checked(&d).expect_err("a walk that stopped early cannot answer");
        assert!(
            err.to_string().contains("present but unreadable"),
            "got: {err}"
        );
        // No value is invented for the lenient caller either.
        assert_eq!(first_us_of(&d, TAG_PIXEL_REPRESENTATION), None);
    }

    /// PS3.3 C.7.6.3.1.1 defines exactly two Pixel Representation values.
    ///
    /// A complete, well-formed US of 2 describes no encoding, so it is
    /// unreadable rather than a signedness. The same rule must NOT leak onto
    /// other US attributes - Bits Stored legitimately holds values above 1 -
    /// which is why the domain check is scoped to the one tag.
    #[test]
    fn an_undefined_pixel_representation_value_is_unreadable() {
        let mut d = evrle_header();
        push_us(&mut d, TAG_PIXEL_REPRESENTATION, 2);
        let err = pixel_representation_checked(&d).expect_err("2 is not a defined value");
        assert!(
            err.to_string().contains("present but unreadable"),
            "got: {err}"
        );
        assert_eq!(first_us_of(&d, TAG_PIXEL_REPRESENTATION), None);

        // A declared four-byte value is not a single US either.
        let mut short = evrle_header();
        short.extend_from_slice(&TAG_PIXEL_REPRESENTATION.0.to_le_bytes());
        short.extend_from_slice(&TAG_PIXEL_REPRESENTATION.1.to_le_bytes());
        short.extend_from_slice(b"US");
        short.extend_from_slice(&4u16.to_le_bytes());
        short.extend_from_slice(&[0u8, 0, 0, 0]);
        assert!(
            pixel_representation_checked(&short).is_err(),
            "a four-byte Pixel Representation is not a US"
        );

        // Bits Stored is not subject to the 0/1 domain.
        let mut bs = evrle_header();
        push_us(&mut bs, TAG_BITS_STORED, 12);
        assert_eq!(bits_stored_of(&bs), Some(12), "12 is a legal Bits Stored");
    }

    /// An element whose value runs past EOF is a TRUNCATED dataset, not an
    /// absent attribute.
    ///
    /// The walk advanced past the end and the loop exited, which answered
    /// `Absent` - and the strict caller turns that into `Ok(0)`, unsigned. A
    /// file that declares a signedness it cannot deliver must not be read as if
    /// it had none.
    #[test]
    fn a_value_that_overruns_the_buffer_is_unreadable_not_absent() {
        let mut d = evrle_header();
        // Bits Stored declares 64 bytes with only 2 present, so stepping the
        // walk by its declared length lands past EOF. Pixel Representation
        // follows in the bytes that ARE there, but a walk that cannot step the
        // preceding element never reaches it legitimately.
        d.extend_from_slice(&TAG_BITS_STORED.0.to_le_bytes());
        d.extend_from_slice(&TAG_BITS_STORED.1.to_le_bytes());
        d.extend_from_slice(b"US");
        d.extend_from_slice(&64u16.to_le_bytes());
        d.extend_from_slice(&8u16.to_le_bytes());

        d.extend_from_slice(&TAG_PIXEL_REPRESENTATION.0.to_le_bytes());
        d.extend_from_slice(&TAG_PIXEL_REPRESENTATION.1.to_le_bytes());
        d.extend_from_slice(b"US");
        d.extend_from_slice(&2u16.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());

        let err = pixel_representation_checked(&d)
            .expect_err("a value that runs past EOF means the dataset is truncated");
        assert!(
            err.to_string().contains("present but unreadable"),
            "got: {err}"
        );
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

    // ---- A2b: codec core regressions ------------------------------------

    #[test]
    fn native_pixeldata_declares_the_padding_it_writes() {
        // PS3.5 8.1: the Value Field is an even number of bytes and the pad is
        // appended to it, so the declared length counts the pad.
        let odd = build_native_ow_pixeldata(&[1, 2, 3]);
        let declared = u32::from_le_bytes([odd[8], odd[9], odd[10], odd[11]]) as usize;
        assert_eq!(declared, 4, "three samples are declared as four bytes");
        assert_eq!(odd.len(), 12 + 4, "and the pad byte is present");
        assert_eq!(odd[15], 0x00, "at the end of the value");

        let even = build_native_ow_pixeldata(&[1, 2, 3, 4]);
        let declared = u32::from_le_bytes([even[8], even[9], even[10], even[11]]) as usize;
        assert_eq!(declared, 4, "an even value is unchanged");
        assert_eq!(even.len(), 12 + 4, "and gains no pad");
    }

    /// PS3.5 A.4: a Fragment's Value is a piece of the compressed codestream,
    /// padded to an EVEN length. It is not four-aligned: the extra bytes would
    /// join the codestream a decoder reassembles.
    ///
    /// The assertion is on the codestream bytes themselves, not on the header,
    /// because a wrong pad shows up as a changed byte inside the value.
    #[test]
    fn an_encapsulated_fragment_pads_only_to_even_and_keeps_its_codestream() {
        // Six bytes is already even: no pad, and the value is byte-identical.
        let six = [0xFFu8, 0xD8, 0xFF, 0xDB, 0x00, 0x43];
        let built = build_encapsulated_pixeldata(&six);
        // Layout: tag(4) OB(2) reserved(2) len(4) BOT item(8) -> fragment Item.
        let frag_item = 4 + 2 + 2 + 4 + 8;
        let frag_len = u32::from_le_bytes([
            built[frag_item + 4],
            built[frag_item + 5],
            built[frag_item + 6],
            built[frag_item + 7],
        ]) as usize;
        assert_eq!(
            frag_len, 6,
            "an even codestream declares exactly its length"
        );
        assert_eq!(
            &built[frag_item + 8..frag_item + 8 + 6],
            &six,
            "the codestream is copied byte for byte"
        );

        // Seven bytes is odd: exactly ONE pad byte, and the first seven are
        // still the codestream. Four-alignment would have added one more and
        // the declared length would read 8.
        let seven = [0xFFu8, 0xD8, 0xFF, 0xDB, 0x00, 0x43, 0x7F];
        let built = build_encapsulated_pixeldata(&seven);
        let frag_len = u32::from_le_bytes([
            built[frag_item + 4],
            built[frag_item + 5],
            built[frag_item + 6],
            built[frag_item + 7],
        ]) as usize;
        assert_eq!(
            frag_len, 8,
            "seven bytes declare eight: even, not four-aligned"
        );
        assert_eq!(
            &built[frag_item + 8..frag_item + 8 + 7],
            &seven,
            "the codestream bytes are untouched"
        );
        assert_eq!(built[frag_item + 8 + 7], 0x00, "one pad byte at the end");

        // The header is Explicit VR LE with OB: PS3.5 A.4 fixes the encoding.
        assert_eq!(&built[4..6], b"OB", "VR is OB");
        assert_eq!(&built[6..8], &[0, 0], "reserved bytes");
        assert_eq!(
            &built[8..12],
            &0xFFFF_FFFFu32.to_le_bytes(),
            "undefined length"
        );
    }

    #[test]
    fn a_sub_byte_sample_width_is_rejected_before_the_length_check() {
        // PS3.5 8.1.1: Bits Allocated is 1 or a multiple of 8. A width of 4
        // made `expected_size` compute zero, so a 1x1 image with no bytes
        // passed the length check as "the right size".
        let pixels = DecodedPixels {
            data: Vec::new(),
            rows: 1,
            columns: 1,
            samples_per_pixel: 1,
            bits_allocated: 4,
            photometric_interpretation: "MONOCHROME2".to_string(),
        };
        let err = pixels
            .validate_frames(1)
            .expect_err("a width of 4 has no byte representation");
        assert!(err.to_string().contains("multiple of 8"), "got: {err}");

        // 1 bit is legal for a SEG, but this encoder has no packed form.
        let packed = DecodedPixels {
            bits_allocated: 1,
            ..pixels
        };
        let err = packed
            .validate_frames(1)
            .expect_err("bit-packed input is refused, not mis-sized");
        assert!(err.to_string().contains("BitsAllocated=1"), "got: {err}");
    }

    /// The Basic Offset Table is one pass and every entry is a real offset.
    ///
    /// PS3.5 A.4: an entry is the byte offset of the frame's first Item from
    /// the first byte of the first Item after the table. The previous loop
    /// summed every preceding fragment for every frame, and `saturating_add`
    /// turned an offset past the 32-bit range into 0xFFFF_FFFF - an entry
    /// that points nowhere but looks like a number.
    #[test]
    fn basic_offset_table_entries_are_exact_and_one_pass() {
        // Three frames, two fragments each, lengths chosen so the offsets are
        // not multiples of anything convenient.
        let fragments: Vec<Vec<u8>> = vec![
            vec![0u8; 4],
            vec![0u8; 8],
            vec![0u8; 12],
            vec![0u8; 16],
            vec![0u8; 20],
            vec![0u8; 24],
        ];
        let out = build_encapsulated_pixeldata_multi(&fragments, 2).expect("built");
        // Walk: tag(4) + VR(2) + reserved(2) + undefined len(4) = 12, then the
        // BOT item header at offset 12.
        let bot_len_at = 12 + 4;
        let bot_len =
            u32::from_le_bytes(out[bot_len_at..bot_len_at + 4].try_into().expect("4 bytes"))
                as usize;
        assert_eq!(bot_len, 3 * 4, "one entry per frame");
        let mut expected = 0u32;
        for f in 0..3 {
            let at = bot_len_at + 4 + f * 4;
            let got = u32::from_le_bytes(out[at..at + 4].try_into().expect("4 bytes"));
            assert_eq!(got, expected, "entry {f}");
            // Each frame's first Item is `8 + len` bytes after the previous one.
            for frag in fragments.iter().skip(f * 2).take(2) {
                expected += 8 + frag.len() as u32;
            }
        }
    }

    /// An offset past the 32-bit range is refused, not saturated.
    ///
    /// PS3.5 A.4: an entry is a 32-bit offset. The previous arithmetic used
    /// `saturating_add`, so a frame starting beyond that range got the entry
    /// 0xFFFF_FFFF - a number a reader accepts and then seeks to nonsense. The
    /// helper takes Item SIZES, so the overflow is driven without allocating
    /// the four gigabytes it describes.
    #[test]
    fn a_basic_offset_table_offset_beyond_the_32_bit_range_is_refused() {
        // Two frames, one Item each; the second Item's start is 8 + the first
        // Item's length. A length near u32::MAX puts it out of range.
        let over = [u32::MAX as usize, 4];
        let err = basic_offset_table_entries(&over, 1, 2)
            .expect_err("an offset past the range cannot be written");
        assert!(
            err.to_string().contains("beyond the 32-bit range"),
            "got: {err}"
        );

        // The largest representable offset is still accepted.
        let exact = [(u32::MAX - 8) as usize, 4];
        let offsets = basic_offset_table_entries(&exact, 1, 2).expect("the boundary is legal");
        assert_eq!(offsets, vec![0, u32::MAX], "no saturation at the edge");
    }

    #[test]
    fn coalescing_no_longer_infers_from_the_component_count() {
        // PS3.5 A.4: the transfer syntax decides whether a frame may span
        // fragments, so three fragments of a single-frame image are either one
        // split codestream or three whole ones - a question only the codec can
        // answer. Passing the decision in makes both shapes expressible.
        let three = vec![vec![1u8, 2], vec![3u8, 4], vec![5u8, 6]];
        let joined = coalesce_single_frame_fragments(three.clone(), 1, true);
        assert_eq!(joined.len(), 1, "one split codestream is joined");
        assert_eq!(joined[0], vec![1, 2, 3, 4, 5, 6]);

        let kept = coalesce_single_frame_fragments(three, 1, false);
        assert_eq!(kept.len(), 3, "three whole codestreams are kept apart");
    }
}
