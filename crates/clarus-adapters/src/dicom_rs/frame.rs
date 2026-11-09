// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Multi-frame DICOM Pixel Data extraction.
//!
//! Extracts a single frame from Pixel Data (7FE0,0010) for both
//! Native (uncompressed) and Encapsulated (compressed) formats.
//! Used by WADO-RS `/frames/{N}` endpoint (PS3.18 Section 10.4.2).
//!
//! # DICOM References
//! - PS3.5 Section 8.2: Native and Encapsulated Pixel Data encoding
//! - PS3.5 Table A.4-1: Encapsulated Pixel Data structure
//! - PS3.3 C.7.6.3: Image Pixel Module attributes

#![forbid(unsafe_code)]

use std::fs::File;
use std::io::{self, Read, Seek};
use std::path::Path;

// -- DICOM tag constants (image pixel module) -------------------------

/// Tag: Rows (0028,0010) - number of rows in the image.
pub const TAG_ROWS: (u16, u16) = (0x0028, 0x0010);
/// Tag: Columns (0028,0011) - number of columns in the image.
pub const TAG_COLUMNS: (u16, u16) = (0x0028, 0x0011);
/// Tag: SamplesPerPixel (0028,0002) - 1 for monochrome, 3 for RGB/YBR.
pub const TAG_SAMPLES_PER_PIXEL: (u16, u16) = (0x0028, 0x0002);
/// Tag: BitsAllocated (0028,0100) - bits per sample (8, 16).
pub const TAG_BITS_ALLOCATED: (u16, u16) = (0x0028, 0x0100);
/// Tag: NumberOfFrames (0028,0008) - total number of frames.
pub const TAG_NUMBER_OF_FRAMES: (u16, u16) = (0x0028, 0x0008);
/// Tag: PixelData (7FE0,0010) - the pixel data element.
pub const TAG_PIXEL_DATA: (u16, u16) = (0x7FE0, 0x0010);

// -- Error type --------------------------------------------------------

/// Errors that can occur during frame extraction.
///
/// Manual `Display`/`Error` (thiserror removed, local Display).
#[derive(Debug)]
pub enum FrameExtractionError {
    FrameOutOfRange { requested: u32, total: u32 },
    InvalidEncapsulation(String),
    TruncatedData,
    MissingMetadata(String),
    InvalidMetadata(String),
}

impl std::fmt::Display for FrameExtractionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameExtractionError::FrameOutOfRange { requested, total } => {
                write!(f, "Frame {requested} out of range (total: {total})")
            }
            FrameExtractionError::InvalidEncapsulation(s) => {
                write!(f, "Invalid encapsulated Pixel Data: {s}")
            }
            FrameExtractionError::TruncatedData => {
                write!(f, "Pixel Data too short for frame offset calculation")
            }
            FrameExtractionError::MissingMetadata(s) => {
                write!(f, "Missing DICOM metadata tag: {s}")
            }
            FrameExtractionError::InvalidMetadata(s) => {
                write!(f, "Invalid metadata value: {s}")
            }
        }
    }
}

impl std::error::Error for FrameExtractionError {}

/// A hard cap on the number of frames a single instance may declare/hold.
///
/// F5 (0310): the amplification surface is DISK + reload RAM, not the walk
/// itself. A 13 GB upload of 8-byte zero-length items yields ~1.6e9 chunks x
/// 56 B = ~91 GB of manifest table. `max_frames_per_instance` (config) bounds
/// that; this constant is the absolute ceiling the config may not exceed
/// (2^24 = 16_777_216), keeping one table <= ~900 MiB in the worst case.
pub const MAX_FRAMES_CEILING: u32 = 16_777_216;

ferrite_core::capacity_eq!(MAX_FRAMES_CEILING, 16_777_216);

/// Error from a capped frame-chunk computation.
///
/// The `TooManyFrames` variant is the F5 signal: the instance declared or
/// actually held more frames than the configured cap, so the caller MUST
/// REJECT it at STOW rather than store it without a table (which would reopen
/// the whole-blob serve path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameTableError {
    /// Declared `NumberOfFrames` or the WALKED item count exceeds `cap`.
    /// `declared` is the NumberOfFrames value, `walked` is the item count
    /// actually seen (both are reported so the caller can log honestly).
    TooManyFrames {
        declared: u32,
        walked: u32,
        cap: u32,
    },
}

impl std::fmt::Display for FrameTableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameTableError::TooManyFrames {
                declared,
                walked,
                cap,
            } => write!(
                f,
                "frame count exceeds max_frames_per_instance ({cap}): declared={declared}, walked={walked}"
            ),
        }
    }
}

impl std::error::Error for FrameTableError {}

// -- Public API --------------------------------------------------------

/// Extract a single frame from Pixel Data (7FE0,0010).
///
/// Returns the raw pixel bytes for frame `frame_number` (1-indexed).
/// Supports both Native (uncompressed) and Encapsulated (compressed) formats.
///
/// # Arguments
/// * `pixel_data` - Raw bytes of the PixelData element value (after tag+length+VR).
/// * `frame_number` - 1-indexed frame number.
/// * `frame_size` - Size of one frame in bytes:
///   `Rows x Columns x SamplesPerPixel x ceil(BitsAllocated/8)`, rounded to even.
/// * `num_frames` - Total number of frames from NumberOfFrames (0028,0008).
///
/// # Panics
/// Never panics - all error paths return `FrameExtractionError`.
pub fn extract_frame(
    pixel_data: &[u8],
    frame_number: u32,
    frame_size: usize,
    num_frames: u32,
) -> Result<Vec<u8>, FrameExtractionError> {
    // -- Validate frame_number --------------------------------------
    if frame_number < 1 || frame_number > num_frames {
        return Err(FrameExtractionError::FrameOutOfRange {
            requested: frame_number,
            total: num_frames,
        });
    }

    if pixel_data.is_empty() {
        return Err(FrameExtractionError::TruncatedData);
    }

    // -- Determine encoding: Native vs Encapsulated ----------------
    // Encapsulated Pixel Data starts with an Item tag (FFFE,E000).
    // In little-endian: bytes [FE, FF, 00, E0] -> group 0xFFFE, element 0xE000.
    let is_encapsulated = pixel_data.len() >= 4
        && pixel_data[0] == 0xFE
        && pixel_data[1] == 0xFF
        && pixel_data[2] == 0x00
        && pixel_data[3] == 0xE0;

    if is_encapsulated {
        extract_frame_encapsulated(pixel_data, frame_number, num_frames)
    } else {
        extract_frame_native(pixel_data, frame_number, frame_size, num_frames)
    }
}

// -- Native (uncompressed) extraction ---------------------------------

/// Extract frame from native (uncompressed) Pixel Data.
///
/// Frames are stored sequentially. Each frame is `frame_size` bytes.
/// Per DICOM PS3.5 Section 8.1.1, frame offsets are always even-byte aligned.
fn extract_frame_native(
    pixel_data: &[u8],
    frame_number: u32,
    frame_size: usize,
    _num_frames: u32,
) -> Result<Vec<u8>, FrameExtractionError> {
    // Frame size must be even (DICOM PS3.5 Section 8.1.1)
    let padded_frame_size = if frame_size.is_multiple_of(2) {
        frame_size
    } else {
        frame_size + 1
    };

    let offset = (frame_number - 1) as usize * padded_frame_size;
    let end = offset + frame_size; // read actual frame_size bytes, not padded

    if end > pixel_data.len() {
        return Err(FrameExtractionError::TruncatedData);
    }

    Ok(pixel_data[offset..end].to_vec())
}

// -- Encapsulated (compressed) extraction -----------------------------

/// Extract frame from encapsulated (compressed) Pixel Data.
///
/// Structure (PS3.5 Table A.4-1):
/// ```text
/// Basic Offset Table Item (FFFE,E000) - optional
/// Item #1 (FFFE,E000) - frame 1
/// Item #2 (FFFE,E000) - frame 2
/// ...
/// Sequence Delimiter Item (FFFE,E0DD)
/// ```
///
/// Each Item: tag (4 bytes) + length (4 bytes u32) + data (length bytes).
fn extract_frame_encapsulated(
    pixel_data: &[u8],
    frame_number: u32,
    num_frames: u32,
) -> Result<Vec<u8>, FrameExtractionError> {
    let data_len = pixel_data.len();
    let mut pos: usize = 0;

    // -- Step 1: Skip Basic Offset Table Item ----------------------
    if pos + 8 > data_len {
        return Err(FrameExtractionError::TruncatedData);
    }

    // Verify it's an Item tag (FFFE,E000)
    let tag_group = u16::from_le_bytes([pixel_data[pos], pixel_data[pos + 1]]);
    let tag_elem = u16::from_le_bytes([pixel_data[pos + 2], pixel_data[pos + 3]]);

    if tag_group != 0xFFFE || tag_elem != 0xE000 {
        return Err(FrameExtractionError::InvalidEncapsulation(
            "Expected Item tag (FFFE,E000) at start of encapsulated Pixel Data".into(),
        ));
    }

    pos += 4; // skip tag
    let bot_length = u32::from_le_bytes([
        pixel_data[pos],
        pixel_data[pos + 1],
        pixel_data[pos + 2],
        pixel_data[pos + 3],
    ]) as usize;
    pos += 4; // skip length

    // Skip Basic Offset Table data
    if pos + bot_length > data_len {
        return Err(FrameExtractionError::TruncatedData);
    }
    pos += bot_length;

    // -- Step 2: Iterate through frame Items -----------------------
    let mut current_frame: u32 = 0;

    while pos + 8 <= data_len {
        let item_group = u16::from_le_bytes([pixel_data[pos], pixel_data[pos + 1]]);
        let item_elem = u16::from_le_bytes([pixel_data[pos + 2], pixel_data[pos + 3]]);

        if item_group != 0xFFFE {
            return Err(FrameExtractionError::InvalidEncapsulation(format!(
                "Expected Item or Delimiter tag at offset {pos}, got ({item_group:04X},{item_elem:04X})"
            )));
        }

        pos += 4; // skip tag

        // Check for Sequence Delimiter (FFFE,E0DD) - end of items
        if item_elem == 0xE0DD {
            // No more frames - we've exhausted the sequence
            break;
        }

        if item_elem != 0xE000 {
            return Err(FrameExtractionError::InvalidEncapsulation(format!(
                "Unexpected element {item_elem:04X} in encapsulated sequence at offset {pos}"
            )));
        }

        // Read item length
        if pos + 4 > data_len {
            return Err(FrameExtractionError::TruncatedData);
        }
        let item_length = u32::from_le_bytes([
            pixel_data[pos],
            pixel_data[pos + 1],
            pixel_data[pos + 2],
            pixel_data[pos + 3],
        ]) as usize;
        pos += 4;

        // Read item data
        if pos + item_length > data_len {
            return Err(FrameExtractionError::TruncatedData);
        }

        current_frame += 1;

        if current_frame == frame_number {
            // This is the requested frame
            return Ok(pixel_data[pos..pos + item_length].to_vec());
        }

        pos += item_length;
    }

    // If we got here, the frame wasn't found
    Err(FrameExtractionError::FrameOutOfRange {
        requested: frame_number,
        total: num_frames.max(current_frame),
    })
}

// -- Metadata helpers -------------------------------------------------

/// Byte range of one frame inside the stored DICOM file.
///
/// `offset` is absolute from the START OF THE FILE (not from the start of
/// PixelData), because the serve path seek()s a file handle - the same handle
/// the CAS blob is opened with - and reads exactly `[offset, offset + len)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameChunk {
    /// 1-based frame number within the instance.
    pub number: u32,
    /// File-absolute byte offset of this frame's stored bytes.
    pub offset: u64,
    /// Length in bytes of this frame's stored bytes.
    pub len: u64,
}

/// Compute the per-frame byte ranges of Pixel Data (7FE0,0010).
///
/// Returns one [`FrameChunk`] per frame with FILE-ABSOLUTE offsets, so a
/// caller can serve a single frame with a bounded `seek` + `read` instead of
/// buffering the whole blob. This is the writer half of the manifest
/// frame-chunk table; the reader half is the byte-range serve path.
///
/// Handles both encodings:
///
/// - **Native (uncompressed)**: frames are contiguous, each `frame_size`
///   bytes, padded to an even length (PS3.5 8.1.1). The first frame starts at
///   the PixelData value offset.
/// - **Encapsulated (compressed)**: the PixelData value is a Basic Offset
///   Table item followed by one item per frame. Each frame's bytes are the
///   item's DATA, so the chunk points past the item's 8-byte header
///   (PS3.5 Table A.4-1).
///
/// Pixel data is NOT decoded and NOT copied: only the item headers are walked
/// to record offsets. A frame whose item is zero-length (a sparse tile)
/// yields a zero-length chunk, which the read path serves as an empty frame -
/// the same semantics `extract_frame` already gives.
///
/// `Ok(vec![])` means "no chunk table is useful here" (single-frame native
/// instance, unparsable PixelData, or missing geometry); the caller then
/// keeps the whole-blob path, so this can never make a servable frame
/// unservable. `Err` is never returned for a well-formed file.
pub fn compute_frame_chunks(dicom_bytes: &[u8]) -> Vec<FrameChunk> {
    // Uncapped wrapper: the ceiling is the only bound, so a "too many frames"
    // result cannot occur and the caller keeps the historical empty-table
    // contract. The STOW path uses the capped variant.
    compute_frame_chunks_capped(dicom_bytes, MAX_FRAMES_CEILING).unwrap_or_default()
}

/// [`compute_frame_chunks`] with an explicit frame cap (F5).
///
/// Returns `Err(TooManyFrames)` when the DECLARED `NumberOfFrames` exceeds
/// `cap`, or when the WALKED item count exceeds `cap` (a file can declare few
/// frames but store many items). On success the table is identical to what
/// `compute_frame_chunks` returns for a file within the cap.
///
/// The declared check runs BEFORE any walk, so a crafted
/// `NumberOfFrames = 0xFFFFFFFF` is rejected in O(1) - no table is built at
/// all. The walked check bounds a file that under-declares but stores many
/// items; it caps the `Vec` at `cap + 1` rows (the extra row is what trips the
/// signal), never more.
pub fn compute_frame_chunks_capped(
    dicom_bytes: &[u8],
    cap: u32,
) -> Result<Vec<FrameChunk>, FrameTableError> {
    let rows = match find_tag_u32(dicom_bytes, TAG_ROWS) {
        Some(v) => v,
        None => return Ok(Vec::new()),
    };
    let columns = match find_tag_u32(dicom_bytes, TAG_COLUMNS) {
        Some(v) => v,
        None => return Ok(Vec::new()),
    };
    let samples = find_tag_u32(dicom_bytes, TAG_SAMPLES_PER_PIXEL).unwrap_or(1);
    let bits = find_tag_u32(dicom_bytes, TAG_BITS_ALLOCATED).unwrap_or(8);
    let num_frames = find_tag_u32(dicom_bytes, TAG_NUMBER_OF_FRAMES).unwrap_or(1);

    // F5: reject a huge DECLARATION before walking anything.
    if num_frames > cap {
        return Err(FrameTableError::TooManyFrames {
            declared: num_frames,
            walked: 0,
            cap,
        });
    }

    // Multi-frame is the only case where a chunk table buys anything: a
    // single-frame instance is served whole anyway.
    if num_frames <= 1 {
        return Ok(Vec::new());
    }

    let (pd_offset, pd_len) = match find_tag_range(dicom_bytes, TAG_PIXEL_DATA) {
        Some(v) => v,
        None => return Ok(Vec::new()),
    };
    // `pd_len` is the DEFINED value length, or "to EOF" for undefined-length
    // encapsulated PixelData (PS3.5 Table A.4-1). Only a genuinely empty
    // defined value is a dead end here.
    if pd_len == 0 {
        return Ok(Vec::new());
    }
    let pd_end = pd_offset.saturating_add(pd_len).min(dicom_bytes.len());
    let pixel_data = &dicom_bytes[pd_offset..pd_end];

    if pixel_data_is_encapsulated_bytes(pixel_data) {
        chunks_encapsulated(pixel_data, pd_offset, num_frames, cap)
    } else {
        Ok(chunks_native(
            pd_offset,
            pd_end,
            compute_frame_size(rows as u16, columns as u16, samples as u16, bits as u16)
                .unwrap_or(0),
            num_frames,
        ))
    }
}

/// True when a PixelData element VALUE begins with an Item tag (FFFE,E000),
/// i.e. it is encapsulated (PS3.5 Table A.4-1).
fn pixel_data_is_encapsulated_bytes(pixel_data: &[u8]) -> bool {
    pixel_data.len() >= 4
        && pixel_data[0] == 0xFE
        && pixel_data[1] == 0xFF
        && pixel_data[2] == 0x00
        && pixel_data[3] == 0xE0
}

/// Native chunks: contiguous frames of `frame_size` bytes, even-padded.
fn chunks_native(
    pd_offset: usize,
    pd_end: usize,
    frame_size: usize,
    num_frames: u32,
) -> Vec<FrameChunk> {
    if frame_size == 0 {
        return Vec::new();
    }
    // PS3.5 8.1.1: each frame starts on an even byte boundary.
    let stride = if frame_size.is_multiple_of(2) {
        frame_size
    } else {
        frame_size + 1
    };
    let available = pd_end.saturating_sub(pd_offset);
    // F1 (0310): grow O(ACTUAL frames), NOT O(declared num_frames).
    // `with_capacity(num_frames)` on an untrusted NumberOfFrames of
    // 0xFFFFFFFF would request ~240 GB before a single byte is examined.
    let mut out = Vec::new();
    for index in 0..num_frames {
        let start = index as usize * stride;
        if start >= available {
            // Declared frames exceed the stored bytes: stop at what is
            // really there rather than emitting chunks that read past EOF.
            break;
        }
        let len = frame_size.min(available - start);
        out.push(FrameChunk {
            number: index + 1,
            offset: (pd_offset + start) as u64,
            len: len as u64,
        });
    }
    out
}

/// Encapsulated chunks: skip the Basic Offset Table, then record each item's
/// DATA range (past the 8-byte item header).
///
/// `cap` bounds the WALKED item count (F5): if the stream holds more than `cap`
/// items, the walk stops and returns `Err(TooManyFrames)` instead of growing
/// the table. A stream that is not item-shaped returns `Ok(vec![])` (the
/// caller keeps the whole-blob path; a partial table would mis-serve).
fn chunks_encapsulated(
    pixel_data: &[u8],
    pd_offset: usize,
    num_frames: u32,
    cap: u32,
) -> Result<Vec<FrameChunk>, FrameTableError> {
    let data_len = pixel_data.len();
    let mut pos: usize = 0;

    // Basic Offset Table item: tag + length + (ignored) offset table bytes.
    if pos + 8 > data_len {
        return Ok(Vec::new());
    }
    if read_u16_at(pixel_data, pos) != 0xFFFE || read_u16_at(pixel_data, pos + 2) != 0xE000 {
        return Ok(Vec::new());
    }
    let bot_len = read_u32_at(pixel_data, pos + 4) as usize;
    pos += 8;
    if pos + bot_len > data_len {
        return Ok(Vec::new());
    }
    // The Basic Offset Table is the ONLY authoritative frame->fragment map
    // (PS3.5 A.4): one entry per frame, in bytes from the first fragment's
    // item tag. An empty table says nothing, so the one-item-per-frame
    // convention below is the only reading left.
    let bot_start = pos;
    let frame_starts: Vec<usize> = (0..bot_len / 4)
        .map(|i| read_u32_at(pixel_data, bot_start + i * 4) as usize)
        .collect();
    pos += bot_len;

    let mut out = Vec::new();
    let mut frame_no: u32 = 0;
    while pos + 8 <= data_len {
        let group = read_u16_at(pixel_data, pos);
        let element = read_u16_at(pixel_data, pos + 2);
        if group != 0xFFFE {
            // Not an item: the encapsulation is not the shape we understand.
            // Returning what we have is honest - the caller falls back to the
            // whole-blob path only if the table is EMPTY, so a partial table
            // would mis-serve. Discard instead.
            return Ok(Vec::new());
        }
        if element == 0xE0DD {
            break; // sequence delimiter
        }
        if element != 0xE000 {
            return Ok(Vec::new());
        }
        // F5: trip the cap as soon as the WALKED count would exceed it. Checked
        // before the push so the table never grows past `cap` rows.
        if frame_no >= cap {
            return Err(FrameTableError::TooManyFrames {
                declared: 0,
                walked: frame_no.saturating_add(1),
                cap,
            });
        }
        let item_len = read_u32_at(pixel_data, pos + 4) as usize;
        pos += 8;
        if pos + item_len > data_len {
            return Ok(Vec::new());
        }
        frame_no += 1;
        out.push(FrameChunk {
            number: frame_no,
            offset: (pd_offset + pos) as u64,
            len: item_len as u64,
        });
        pos += item_len;
    }
    // PS3.5 A.4: a frame need not be one fragment. With a non-empty Basic
    // Offset Table the boundaries are given, and a frame that spans several
    // fragments is NOT expressible as one contiguous byte range - the item
    // headers sit between them - so the table cannot describe it. Refusing is
    // the honest answer: the caller falls back to the whole-blob path, which
    // extracts by the same table but never claims a frame is a byte range. An
    // empty table says nothing and leaves the one-item-per-frame convention,
    // which is what keeps an over-declared NumberOfFrames bounded by the items
    // actually stored.
    if !frame_starts.is_empty() {
        if frame_starts.len() > cap as usize || frame_starts.len() != num_frames as usize {
            return Ok(Vec::new());
        }
        // One entry per frame, agreeing with the walk: item i IS frame i.
        if frame_starts.len() != out.len() {
            return Ok(Vec::new());
        }
        for (chunk, &start) in out.iter().zip(frame_starts.iter()) {
            // BOT offsets count from the first fragment's ITEM TAG; a chunk
            // offset points at the item's VALUE, which is one 8-byte item
            // header later (PS3.5 A.4).
            let expect = chunk.offset as usize - pd_offset - (bot_start + bot_len) - 8;
            if start != expect {
                return Ok(Vec::new());
            }
        }
        return Ok(out);
    }
    Ok(out)
}

/// Little-endian u16 at `data[at..at+2]`, 0 when out of range.
fn read_u16_at(data: &[u8], at: usize) -> u16 {
    match data.get(at..at + 2) {
        Some(b) => u16::from_le_bytes([b[0], b[1]]),
        None => 0,
    }
}

/// Little-endian u32 at `data[at..at+4]`, 0 when out of range.
fn read_u32_at(data: &[u8], at: usize) -> u32 {
    match data.get(at..at + 4) {
        Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        None => 0,
    }
}

// -------------------------------------------------------------------------
// Streaming file variant (STOW read-back removal)
// -------------------------------------------------------------------------

/// Compute the per-frame byte ranges of Pixel Data **without reading the
/// instance into memory**.
///
/// This is the STOW-side twin of [`compute_frame_chunks`]: same semantics, same
/// output, but the header is walked with `Read + Seek` over a `File` instead of
/// over a `&[u8]`, so building the manifest frame table for a multi-GB WSI costs
/// O(1) memory instead of a whole-instance read-back. The 13.09 GB RMS base
/// level needs an ~8.6 MiB table; the slice variant read all ~13 GB to derive
/// it, this one never reads a pixel byte.
///
/// # No header cap (deliberate)
/// The VL Whole Slide IOD makes the Per-frame Functional Groups Sequence
/// (5200,9230) Type 1, one item per frame, BEFORE PixelData (PS3.3 C.7.6.16), so
/// the header is O(number of frames) - a conformant 161191-frame WSI carries a
/// ~10-25 MiB per-frame sequence, a 40x scan ~4x that. A fixed header cap tuned
/// to one file would not generalize, so the walk is incremental and unbounded in
/// the same way a stream is.
///
/// # Allocation discipline (ap101)
/// Every length field read from the file (`bot_len`, `item_len`, declared value
/// lengths, declared NumberOfFrames) is consumed by `seek` ONLY, never as an
/// allocation size. Geometry values are read through a fixed 16-byte stack
/// buffer. The only heap allocation is the returned table, grown O(ACTUAL frames
/// walked) with `Vec::new()` + `push` - never `with_capacity(declared)`, which a
/// malicious `NumberOfFrames = 0xFFFFFFFF` would turn into a ~240 GB request.
///
/// # Failure contract
/// NEVER returns `Err` and NEVER returns a partial table: any read/seek error,
/// truncation, unparsable geometry, or single-frame/unreadable instance yields
/// an EMPTY `Vec`. An empty table means "keep the whole-blob serve path", so a
/// failure here can never make a servable frame unservable - it only costs the
/// caller the tile fast path.
///
/// Undefined-length encapsulated PixelData (PS3.5 Table A.4-1, the RMS field
/// file) IS tabled: the item walk stops at the FFFE,E0DD delimiter, so no
/// declared length is needed.
pub fn compute_frame_chunks_from_file(path: &Path) -> Vec<FrameChunk> {
    compute_frame_chunks_from_file_capped(path, MAX_FRAMES_CEILING).unwrap_or_default()
}

/// [`compute_frame_chunks_from_file`] with an explicit frame cap (F5).
///
/// Same walking discipline as the slice variant: a DECLARED `NumberOfFrames`
/// over `cap` is rejected before any item walk, and a WALKED item count over
/// `cap` trips the signal. Any read/seek/truncation problem still yields
/// `Ok(vec![])` (never `Err`) - only the F5 cap produces `Err`.
pub fn compute_frame_chunks_from_file_capped(
    path: &Path,
    cap: u32,
) -> Result<Vec<FrameChunk>, FrameTableError> {
    let Ok(mut file) = File::open(path) else {
        return Ok(Vec::new());
    };
    match scan_frame_chunks(&mut file, cap) {
        Ok(chunks) => Ok(chunks),
        Err(ScanOutcome::NoTable) => Ok(Vec::new()),
        Err(ScanOutcome::TooMany(e)) => Err(e),
    }
}

/// 0310 bulkdata (seek-based single-tag read): find ONE tag's value in a file
/// WITHOUT ever reading the whole file.
///
/// Streams the element walk (Read+Seek, the SAME VR/length decode as
/// [`element_extent`]) and, when it reaches `target`, seeks to the value start
/// and reads exactly the value bytes. This is the bounded replacement for the
/// former whole-blob fallback in `h_wado_bulkdata`: a `/bulkdata/{g}/{e}`
/// request for a bogus tag on a 13 GB WSI must cost O(value), not O(instance).
///
/// `max_value` is a HARD cap enforced BEFORE any allocation:
/// - a defined-length value whose declared length exceeds `max_value` returns
///   `None` with ZERO bytes allocated (a crafted huge length cannot allocate);
/// - an undefined-length value is walked to its FFFE,E0DD delimiter under a
///   running byte budget capped at `max_value`; exceeding the budget is `None`.
///
/// Any read/seek error or malformed element is `None` - never a partial value,
/// never an abort (matches the "keep the safe path" discipline of the frame
/// walk).
///
/// Returns `None` for a tag that is absent as well (the caller serves an empty
/// body, the historical not-found behaviour).
pub fn find_tag_value_in_file(
    path: &Path,
    target: (u16, u16),
    max_value: usize,
) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    let mut pos = stream_scan_start(&mut file)?;
    // Any tag may be requested, so walk the element stream generically,
    // stepping over each value (sequences and undefined items are skipped
    // without buffering their bodies).
    while pos + 8 <= file_len {
        let h = read_element_header(&mut file, pos)?;
        if (h.group, h.element) == target {
            if h.undefined {
                // Walk to the E0DD delimiter under a hard byte budget. Refuse
                // (None) the moment the budget would be exceeded - no
                // unbounded allocation.
                let end = stream_undefined_end(&mut file, h.value_start)?;
                let len = end.checked_sub(h.value_start)?;
                if len > max_value as u64 {
                    return None;
                }
                let mut buf = vec![0u8; len as usize];
                stream_read_exact_at(&mut file, h.value_start, &mut buf)?;
                return Some(buf);
            }
            // Defined length: refuse BEFORE the allocation when over cap.
            if h.length > max_value as u64 {
                return None;
            }
            let mut buf = vec![0u8; h.length as usize];
            stream_read_exact_at(&mut file, h.value_start, &mut buf)?;
            return Some(buf);
        }
        // Not the target: step past it.
        if h.undefined {
            pos = stream_undefined_end(&mut file, h.value_start)?;
        } else {
            pos = h
                .value_start
                .checked_add(h.length)?
                .checked_add(h.length % 2)?;
        }
    }
    None
}

/// Internal outcome of the streaming walk.
///
/// `NoTable` is the historical "keep the whole-blob path" result (any
/// malformed/truncated/unreadable shape, or a genuinely single-frame file).
/// `TooMany` is the F5 cap signal and is the ONLY case the caller must reject.
#[derive(Debug)]
enum ScanOutcome {
    NoTable,
    TooMany(FrameTableError),
}

/// The streaming walk behind [`compute_frame_chunks_from_file`].
///
/// `Err(NoTable)` on any malformed/truncated/unreadable shape (the caller maps
/// it to an empty table). `Ok(vec![])` is the single-frame / no-useful-table
/// case. `Err(TooMany)` is the F5 cap signal.
fn scan_frame_chunks(file: &mut File, cap: u32) -> Result<Vec<FrameChunk>, ScanOutcome> {
    // -- 1. Skip preamble + meta group, land on the dataset start ------
    let dataset_start = stream_scan_start(file).ok_or(ScanOutcome::NoTable)?;

    // -- 2. Streaming element walk to PixelData, collecting geometry --
    let mut rows: Option<u32> = None;
    let mut columns: Option<u32> = None;
    let mut samples: Option<u32> = None;
    let mut bits: Option<u32> = None;
    let mut num_frames: Option<u32> = None;

    let mut pos = dataset_start;
    // Geometry values are read into a FIXED stack buffer regardless of the
    // declared value length: a u32-compatible integer never needs more than 16
    // bytes, and sizing a buffer by the file's own length is the alloc-DoS we
    // must not reintroduce.
    let mut geom = [0u8; 16];

    let (pd_offset, pd_len, pd_undefined) = loop {
        // A missing header means we ran out of the file without reaching
        // PixelData: no table (the caller keeps the whole-blob path), never a
        // partial one.
        let Some(header) = read_element_header(file, pos) else {
            return Ok(Vec::new());
        };
        let tag = (header.group, header.element);
        if tag == TAG_PIXEL_DATA {
            break (header.value_start, header.length, header.undefined);
        }

        // Geometry tag: read at most 16 bytes of the value, decode with the
        // SAME `parse_dicom_int` the slice path uses.
        let geom_slot = match tag {
            TAG_ROWS => Some(&mut rows),
            TAG_COLUMNS => Some(&mut columns),
            TAG_SAMPLES_PER_PIXEL => Some(&mut samples),
            TAG_BITS_ALLOCATED => Some(&mut bits),
            TAG_NUMBER_OF_FRAMES => Some(&mut num_frames),
            _ => None,
        };
        if let Some(slot) = geom_slot {
            if header.undefined {
                // An undefined-length integer makes no sense; treat the file
                // as unparsable rather than guessing.
                return Ok(Vec::new());
            }
            let n = header.length.min(geom.len() as u64) as usize;
            if n > 0 {
                stream_read_exact_at(file, header.value_start, &mut geom[..n])
                    .ok_or(ScanOutcome::NoTable)?;
                *slot = parse_dicom_int(&geom[..n]);
            }
        }

        // F5: reject a huge DECLARATION as soon as NumberOfFrames is known,
        // before walking the item stream at all - a crafted
        // NumberOfFrames = 0xFFFFFFFF must not reach the table builder.
        if tag == TAG_NUMBER_OF_FRAMES
            && let Some(nf) = num_frames.filter(|nf| *nf > cap)
        {
            return Err(ScanOutcome::TooMany(FrameTableError::TooManyFrames {
                declared: nf,
                walked: 0,
                cap,
            }));
        }

        // Advance past this element: undefined-length -> walk to its delimiter;
        // defined length -> seek past the value, plus the PS3.5 7.1 pad byte
        // when the VALUE length is odd.
        pos = if header.undefined {
            stream_undefined_end(file, header.value_start).ok_or(ScanOutcome::NoTable)?
        } else {
            header
                .value_start
                .checked_add(header.length)
                .and_then(|v| v.checked_add(header.length % 2))
                .ok_or(ScanOutcome::NoTable)?
        };
    };

    // -- 3. Early empty (mirrors the slice variant's guards) -----------
    let rows = rows.ok_or(ScanOutcome::NoTable)?;
    let columns = columns.ok_or(ScanOutcome::NoTable)?;
    let samples = samples.unwrap_or(1);
    let bits = bits.unwrap_or(8);
    let num_frames = num_frames.unwrap_or(1);
    // Undefined-length PixelData is NOT an early-empty trigger: it is the
    // standard encoding for encapsulated pixel data (PS3.5 Table A.4-1) and
    // what the field RMS file uses. The encapsulated item walk terminates at
    // the FFFE,E0DD delimiter, so `pd_len` is never consulted for it.
    if num_frames <= 1 || (!pd_undefined && pd_len == 0) {
        return Ok(Vec::new());
    }

    let file_len = file.metadata().ok().ok_or(ScanOutcome::NoTable)?.len();
    // Defined length: bound by pd_offset + pd_len. Undefined length: the
    // encapsulated stream runs to the sequence delimiter, so the safe upper
    // bound is EOF (the item walk stops at E0DD long before it).
    let pd_end = if pd_undefined {
        file_len
    } else {
        pd_offset
            .checked_add(pd_len)
            .ok_or(ScanOutcome::NoTable)?
            .min(file_len)
    };
    if pd_offset + 4 > pd_end {
        return Ok(Vec::new());
    }

    // -- 4. Determine encapsulation from the first 4 bytes of PixelData --
    let mut first4 = [0u8; 4];
    if stream_read_exact_at(file, pd_offset, &mut first4).is_none() {
        return Ok(Vec::new());
    }
    let encapsulated = first4 == [0xFE, 0xFF, 0x00, 0xE0];

    if encapsulated {
        chunks_encapsulated_stream(file, pd_offset, pd_end, num_frames, cap)
    } else {
        Ok(chunks_native_stream(
            pd_offset,
            pd_end,
            compute_frame_size(rows as u16, columns as u16, samples as u16, bits as u16)
                .unwrap_or(0) as u64,
            num_frames,
        ))
    }
}

/// A decoded element header at a file position.
struct StreamHeader {
    group: u16,
    element: u16,
    /// Value length as declared (NOT clamped; `u64::MAX` never appears here
    /// because undefined length is reported via `undefined`).
    length: u64,
    /// File-absolute offset of the value.
    value_start: u64,
    /// `true` for the 0xFFFFFFFF undefined length.
    undefined: bool,
}

/// Read one element header at `pos`, deciding the shape by the SAME rules as
/// `element_extent`: extended VR -> 12-byte header, VR pair -> 8-byte header,
/// else implicit -> 8-byte header. Returns `None` at/after EOF.
fn read_element_header(file: &mut File, pos: u64) -> Option<StreamHeader> {
    let mut h = [0u8; 12];
    // Try the 12-byte extended form first; if the file is shorter, fall back to
    // 8 bytes (a short VR never occupies bytes 8..12).
    let got12 = stream_read_exact_at(file, pos, &mut h).is_some();
    if !got12 {
        // Retry with 8 bytes at the same position (the read above may have
        // failed because the file ends before byte 12).
        let mut h8 = [0u8; 8];
        stream_read_exact_at(file, pos, &mut h8)?;
        let group = u16::from_le_bytes([h8[0], h8[1]]);
        let element = u16::from_le_bytes([h8[2], h8[3]]);
        let vr = [h8[4], h8[5]];
        if is_extended_vr_bytes(vr) {
            return None; // an extended element truncated inside its header
        }
        let length = if is_vr_pair(vr) {
            u16::from_le_bytes([h8[6], h8[7]]) as u64
        } else {
            u32::from_le_bytes([h8[4], h8[5], h8[6], h8[7]]) as u64
        };
        return Some(StreamHeader {
            group,
            element,
            length,
            value_start: pos + 8,
            undefined: length == 0xFFFF_FFFF,
        });
    }

    let group = u16::from_le_bytes([h[0], h[1]]);
    let element = u16::from_le_bytes([h[2], h[3]]);
    let vr = [h[4], h[5]];
    if is_extended_vr_bytes(vr) {
        let raw = u32::from_le_bytes([h[8], h[9], h[10], h[11]]);
        Some(StreamHeader {
            group,
            element,
            length: if raw == 0xFFFF_FFFF { 0 } else { raw as u64 },
            value_start: pos + 12,
            undefined: raw == 0xFFFF_FFFF,
        })
    } else if is_vr_pair(vr) {
        let raw = u16::from_le_bytes([h[6], h[7]]);
        Some(StreamHeader {
            group,
            element,
            length: raw as u64,
            value_start: pos + 8,
            undefined: false,
        })
    } else {
        let raw = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        Some(StreamHeader {
            group,
            element,
            length: if raw == 0xFFFF_FFFF { 0 } else { raw as u64 },
            value_start: pos + 8,
            undefined: raw == 0xFFFF_FFFF,
        })
    }
}

/// Dataset start: skip 128-byte preamble + "DICM" (when present) and the whole
/// meta group (group 0002), seeking past each value. Read+Seek port of
/// `pixel_data_scan_start`.
fn stream_scan_start(file: &mut File) -> Option<u64> {
    let mut pos: u64 = 0;
    let mut magic = [0u8; 4];
    if stream_read_exact_at(file, 128, &mut magic).is_some() && &magic == b"DICM" {
        pos = 132;
    }
    let file_len = file.metadata().ok()?.len();
    while pos + 8 <= file_len {
        let mut h = [0u8; 8];
        stream_read_exact_at(file, pos, &mut h)?;
        let group = u16::from_le_bytes([h[0], h[1]]);
        if group != 0x0002 {
            return Some(pos);
        }
        match read_element_header(file, pos)? {
            StreamHeader {
                undefined: true, ..
            } => return Some(pos),
            StreamHeader {
                length,
                value_start,
                ..
            } => {
                pos = value_start.checked_add(length)?.checked_add(length % 2)?;
            }
        }
    }
    Some(pos)
}

/// First byte AFTER an undefined-length element, walking nested items and
/// counting nesting. Read+Seek port of `undefined_length_end`.
///
/// Iterative, so depth costs no stack - but it still needs a bound: an
/// undefined item header is 8 bytes, so a crafted 4 MiB payload can claim half
/// a million nesting levels and keep this walk busy for the whole request.
/// Real datasets nest a handful of levels (a sequence inside a sequence inside
/// an item); 64 is far past anything PS3.3 produces and still lets the walk
/// terminate in bounded work.
fn stream_undefined_end(file: &mut File, pos: u64) -> Option<u64> {
    // Iterative, not recursive: the doc comment above this walk promises that
    // nesting costs no stack, and a crafted upload can nest undefined-length
    // items until the stack runs out. One counter tracks the depth and the
    // loop never calls itself.
    const MAX_NESTING: u32 = 64;
    let file_len = file.metadata().ok()?.len();
    let mut nesting: u32 = 0;
    let mut p = pos;
    while p + 8 <= file_len {
        let mut h = [0u8; 8];
        stream_read_exact_at(file, p, &mut h)?;
        let group = u16::from_le_bytes([h[0], h[1]]);
        let element = u16::from_le_bytes([h[2], h[3]]);
        if group == 0xFFFE && element == 0xE000 {
            // An undefined item nests one level deeper; a defined item is
            // skipped by its length (never buffered).
            let len = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
            p += 8;
            if len == 0xFFFF_FFFF {
                nesting += 1;
                if nesting > MAX_NESTING {
                    return None;
                }
            } else {
                p = p.checked_add(len as u64)?;
            }
            continue;
        }
        if group == 0xFFFE && (element == 0xE00D || element == 0xE0DD) {
            p += 8;
            if nesting == 0 {
                return Some(p);
            }
            nesting -= 1;
            continue;
        }
        // A nested element: skip by its extent, or enter it when undefined.
        // Entering is just another turn of this loop with `nesting` one
        // deeper, which is what the recursive version expressed by calling
        // itself.
        match read_element_header(file, p)? {
            StreamHeader {
                undefined: true,
                value_start,
                ..
            } => {
                nesting += 1;
                if nesting > MAX_NESTING {
                    return None;
                }
                p = value_start;
            }
            StreamHeader {
                length,
                value_start,
                ..
            } => {
                p = value_start.checked_add(length)?.checked_add(length % 2)?;
            }
        }
    }
    None
}

/// First byte AFTER the last top-level element of the file, i.e. where a
/// conformant dataset ends.
///
/// Used to distinguish framing junk a client appended (`\r\n` before the
/// closing multipart delimiter) from real trailing bytes of the last element.
/// Without that distinction a tail stripped by shape alone could cut into
/// PixelData: a native 16-bit sample of 0x0A0D is the two bytes `0D 0A`, and
/// that is the LAST two bytes of the file for one image in ~65536.
///
/// `None` when the walk cannot reach a definite answer (malformed, truncated,
/// unreadable): the caller must then leave the bytes alone.
pub fn dataset_end_in_file(path: &Path) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    let mut pos = stream_scan_start(&mut file)?;
    // The meta group may precede the dataset; to this walk it is just more
    // top-level elements, and the result is the same.
    let mut end = pos;
    while pos + 8 <= file_len {
        let h = read_element_header(&mut file, pos)?;
        if h.group == 0xFFFE {
            // A stray delimiter at the top level means the structure is not
            // what this walk assumes.
            return None;
        }
        pos = if h.undefined {
            stream_undefined_end(&mut file, h.value_start)?
        } else {
            let value_end = h.value_start.checked_add(h.length)?;
            // Elements are even-length padded (PS3.5 7.1.2).
            value_end.checked_add(h.length % 2)?
        };
        if pos > file_len {
            return None;
        }
        end = pos;
    }
    Some(end)
}

/// Native chunks with file-absolute offsets: contiguous `frame_size`-byte
/// frames, even-padded. Arithmetic identical to `chunks_native`.
///
/// The table grows O(actual frames that fit the file), NOT O(declared
/// `num_frames`): a malicious declaration cannot size the allocation.
fn chunks_native_stream(
    pd_offset: u64,
    pd_end: u64,
    frame_size: u64,
    num_frames: u32,
) -> Vec<FrameChunk> {
    if frame_size == 0 {
        return Vec::new();
    }
    let stride = if frame_size.is_multiple_of(2) {
        frame_size
    } else {
        frame_size + 1
    };
    let available = pd_end.saturating_sub(pd_offset);
    let mut out = Vec::new();
    let mut index: u64 = 0;
    loop {
        let start = index.saturating_mul(stride);
        if start >= available {
            break;
        }
        // Bound the count by the declaration too: a file that physically holds
        // more bytes than `num_frames * stride` must not emit extra chunks.
        if index >= num_frames as u64 {
            break;
        }
        let len = frame_size.min(available - start);
        out.push(FrameChunk {
            number: (index + 1) as u32,
            offset: pd_offset + start,
            len,
        });
        index += 1;
    }
    out
}

/// Encapsulated chunks with file-absolute offsets: skip the Basic Offset Table,
/// then record each item's DATA range. Item DATA is never read - only seeked
/// past - so the walk is O(1) memory. Offsets equal `(pd_offset + pos)` in
/// `chunks_encapsulated`.
///
/// `cap` bounds the WALKED item count (F5): more than `cap` items trips
/// `TooMany`. Any malformed/truncated shape yields `Ok(vec![])`.
fn chunks_encapsulated_stream(
    file: &mut File,
    pd_offset: u64,
    pd_end: u64,
    num_frames: u32,
    cap: u32,
) -> Result<Vec<FrameChunk>, ScanOutcome> {
    let mut pos = pd_offset;
    // Basic Offset Table item header.
    let mut bot = [0u8; 8];
    if stream_read_exact_at(file, pos, &mut bot).is_none() {
        return Ok(Vec::new());
    }
    if bot[0] != 0xFE || bot[1] != 0xFF || bot[2] != 0x00 || bot[3] != 0xE0 {
        return Ok(Vec::new());
    }
    let bot_len = u32::from_le_bytes([bot[4], bot[5], bot[6], bot[7]]) as u64;
    let fragments_start = pos.saturating_add(8);
    pos = fragments_start;
    // `bot_len` is consumed by arithmetic only, never as an allocation size.
    let Some(after_bot) = pos.checked_add(bot_len) else {
        return Ok(Vec::new());
    };
    if after_bot > pd_end {
        return Ok(Vec::new());
    }
    let bot_offset = pos;
    pos = after_bot;

    let mut out = Vec::new();
    let mut frame_no: u32 = 0;
    while pos + 8 <= pd_end {
        let mut item = [0u8; 8];
        if stream_read_exact_at(file, pos, &mut item).is_none() {
            return Ok(Vec::new());
        }
        let group = u16::from_le_bytes([item[0], item[1]]);
        let element = u16::from_le_bytes([item[2], item[3]]);
        if group != 0xFFFE {
            // Not an item: the encapsulation is not the shape we understand.
            // A partial table would mis-serve, so discard.
            return Ok(Vec::new());
        }
        if element == 0xE0DD {
            break; // sequence delimiter
        }
        if element != 0xE000 {
            return Ok(Vec::new());
        }
        // F5: trip the cap before the push, so the table never grows past it.
        if frame_no >= cap {
            return Err(ScanOutcome::TooMany(FrameTableError::TooManyFrames {
                declared: 0,
                walked: frame_no.saturating_add(1),
                cap,
            }));
        }
        let item_len = u32::from_le_bytes([item[4], item[5], item[6], item[7]]) as u64;
        let data_start = pos + 8;
        let Some(data_end) = data_start.checked_add(item_len) else {
            return Ok(Vec::new());
        };
        if data_end > pd_end {
            return Ok(Vec::new());
        }
        frame_no += 1;
        out.push(FrameChunk {
            number: frame_no,
            offset: data_start,
            len: item_len,
        });
        // Seek past the item DATA without reading it.
        pos = data_end;
    }
    // PS3.5 A.4: a non-empty Basic Offset Table must agree with the walk. A
    // disagreement means a frame spans several fragments, which one contiguous
    // range cannot express, so there is no table to serve - the same reading
    // the slice variant takes. An empty table says nothing and leaves the
    // one-item-per-frame convention.
    if bot_len > 0 {
        if !bot_len.is_multiple_of(4)
            || bot_len / 4 != num_frames as u64
            || bot_len / 4 > cap as u64
        {
            return Ok(Vec::new());
        }
        if bot_len / 4 != out.len() as u64 {
            return Ok(Vec::new());
        }
        let mut table = vec![0u8; bot_len as usize];
        if stream_read_exact_at(file, bot_offset, &mut table).is_none() {
            return Ok(Vec::new());
        }
        for (chunk, entry) in out.iter().zip(table.chunks_exact(4)) {
            let start = u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]) as u64;
            let expect = chunk.offset - fragments_start;
            if start != expect {
                return Ok(Vec::new());
            }
        }
    }
    Ok(out)
}

/// `read_exact` at an absolute file offset; `None` on a short read or a seek
/// past EOF. The single I/O primitive the streaming walk uses.
fn stream_read_exact_at(file: &mut File, at: u64, buf: &mut [u8]) -> Option<()> {
    file.seek(io::SeekFrom::Start(at)).ok()?;
    file.read_exact(buf).ok()
}

// -------------------------------------------------------------------------
// Slice helpers (unchanged)
// -------------------------------------------------------------------------

/// File-absolute `(offset, length)` of a tag's VALUE, or `None`.
///
/// A minimal, self-contained scan (tag + length + value, stopping at PixelData
/// only when it is not the tag we are looking for). Deliberately independent
/// of the metadata parser: this runs once per STOW instance to build the
/// frame table, and must not pay a full metadata walk.
fn find_tag_range(data: &[u8], target: (u16, u16)) -> Option<(usize, usize)> {
    let mut pos = pixel_data_scan_start(data);
    while pos + 8 <= data.len() {
        let tag = (read_u16_at(data, pos), read_u16_at(data, pos + 2));
        if tag == target {
            // An undefined-length TARGET (PixelData with 0xFFFFFFFF, the
            // standard encapsulation encoding: PS3.5 Table A.4-1) has no
            // declared length, but its value is legitimately "the rest of the
            // element stream". Report the value range to EOF; an encapsulated
            // consumer (`chunks_encapsulated`) walks the item stream and stops
            // at the FFFE,E0DD delimiter, so the to-EOF bound is exact. This
            // mirrors the file variant, which likewise uses EOF as the upper
            // bound for undefined-length PixelData.
            if let Some((length, value_start, _end)) = element_extent(data, pos) {
                return Some((value_start, length));
            }
            let vr = [data[pos + 4], data[pos + 5]];
            let value_start = if is_extended_vr_bytes(vr) {
                pos + 12
            } else {
                pos + 8
            };
            if value_start > data.len() {
                return None;
            }
            return Some((value_start, data.len() - value_start));
        }
        // Stop at PixelData: without this, an encapsulated PixelData body
        // would be scanned as if it were a dataset (item headers are not
        // elements) and could produce a bogus hit.
        if tag == TAG_PIXEL_DATA {
            return None;
        }
        // An undefined-length element (a private or nested sequence) is
        // skipped past its delimitation; treating it as an error would end
        // the scan before PixelData on every real vendor file.
        match element_extent(data, pos) {
            Some((length, _value_start, value_end)) => {
                // Pad on the VALUE length, not on the element length (see
                // `pixel_data_scan_start`).
                pos = value_end + (length % 2);
            }
            None => {
                let vr = [data[pos + 4], data[pos + 5]];
                let value_start = if is_extended_vr_bytes(vr) {
                    pos + 12
                } else {
                    pos + 8
                };
                pos = undefined_length_end(data, value_start)?;
            }
        }
    }
    None
}
/// Read a tag's value as u32 via [`find_tag_range`] plus the ASCII/binary
/// integer decoding that DICOM numbers need (US, IS, UL, SS).
fn find_tag_u32(data: &[u8], target: (u16, u16)) -> Option<u32> {
    let (offset, len) = find_tag_range(data, target)?;
    let end = offset.saturating_add(len).min(data.len());
    parse_dicom_int(data.get(offset..end)?)
}

/// Overwrite a top-level IS `NumberOfFrames` (0028,0008) in place with `value`.
///
/// The rewritten text is right-padded with a space to the ORIGINAL length
/// (PS3.5 6.2: an IS value is padded with a space, never NUL), so the element
/// keeps its size and every offset after it stays valid - the splice below
/// cuts the buffer at the PixelData header it found before the rewrite.
/// A value that does not fit the original field is left alone rather than
/// resizing the element.
fn rewrite_number_of_frames(buf: &mut [u8], value: u32) {
    let text = value.to_string();
    let mut pos = 132usize;
    while pos + 8 <= buf.len() {
        let tag = (read_u16_at(buf, pos), read_u16_at(buf, pos + 2));
        if tag == TAG_NUMBER_OF_FRAMES {
            let vr = [buf[pos + 4], buf[pos + 5]];
            let (hdr_len, len) = if is_extended_vr_bytes(vr) {
                let l = read_u32_at(buf, pos + 8) as usize;
                (12usize, l)
            } else {
                let l = read_u16_at(buf, pos + 6) as usize;
                (8usize, l)
            };
            if text.len() > len || pos + hdr_len + len > buf.len() {
                return;
            }
            buf[pos + hdr_len..pos + hdr_len + len].fill(b' ');
            buf[pos + hdr_len..pos + hdr_len + text.len()].copy_from_slice(text.as_bytes());
            return;
        }
        let vr = [buf[pos + 4], buf[pos + 5]];
        let (hdr_len, len) = if is_extended_vr_bytes(vr) {
            (12usize, read_u32_at(buf, pos + 8) as usize)
        } else {
            (8usize, read_u16_at(buf, pos + 6) as usize)
        };
        if len == 0xFFFF_FFFF {
            return; // an undefined-length sequence: stop rather than desync
        }
        match pos.checked_add(hdr_len).and_then(|p| p.checked_add(len)) {
            Some(next) if next > pos => pos = next,
            _ => return,
        }
    }
}

/// 0310 F3 (D3): rebuild a small, PARSEABLE instance buffer from a bounded
/// header plus ONE frame's stored bytes.
///
/// The frame-thumbnail route must decode one frame, but the decoder wants a
/// full instance slice (it looks up geometry, VOI and PixelData by tag). For a
/// multi-GB WSI that slice is the whole instance. This function returns
/// `head[..pd_header_end] + <PixelData header with the chunk length> + frame`,
/// so the caller parses only the bounded header plus the single frame.
///
/// `head` must contain every element before PixelData AND the PixelData
/// tag+VR+length header (i.e. it may be cut inside the PixelData VALUE). The
/// returned buffer declares the PixelData value as exactly `frame`, so a
/// single-frame / encapsulated extraction sees a well-formed element.
///
/// Returns `None` when the head does not contain a usable PixelData header.
pub fn splice_frame_into_head(head: &[u8], frame: &[u8]) -> Option<Vec<u8>> {
    // Locate the PixelData element HEADER in the head (value may be absent).
    let start = pixel_data_scan_start(head);
    let mut pos = start;
    while pos + 8 <= head.len() {
        let tag = (read_u16_at(head, pos), read_u16_at(head, pos + 2));
        if tag == TAG_PIXEL_DATA {
            // Header width depends on the encoding, and getting it wrong is
            // silent corruption: an explicit long VR (OB/OW/UN/SQ/...) carries
            // 2 reserved bytes plus a 4-byte length (12 total), every other
            // explicit VR carries a 2-byte length (8 total), and an Implicit VR
            // element has no VR bytes at all and a 4-byte length (8 total).
            // Reading the implicit case as 12 swallowed the first four pixel
            // bytes into the header and left a second length field behind.
            let vr = [head[pos + 4], head[pos + 5]];
            let hdr_len = if is_extended_vr_bytes(vr) {
                12usize
            } else {
                8usize
            };
            if pos + hdr_len > head.len() {
                return None;
            }
            // The result holds ONE frame, so NumberOfFrames must say one.
            // Leaving the original count (a 400-frame slide, say) makes the
            // spliced instance violate PS3.5 A.4 - the frame table walk then
            // finds fewer items than declared, and a consumer that trusts the
            // count reads past the end.
            let mut out = head[..pos + hdr_len].to_vec();
            rewrite_number_of_frames(&mut out, 1);
            let pos = pos;
            // Preserve the original length-mode: undefined => encapsulated
            // PixelData (write a fresh BOT + one item + delimiter); a defined
            // length => native frames (raw value).
            let declared = read_u32_at(head, pos + hdr_len - 4);
            let encapsulated = declared == 0xFFFF_FFFF;

            if encapsulated {
                // New value = BOT item (empty) + one frame item + E0DD.
                let mut out2 = Vec::with_capacity(pos + hdr_len + frame.len() + 32);
                out2.extend_from_slice(&out);
                // Rewrite the PixelData value length to undefined (unchanged,
                // but be explicit about the 4-byte field offset).
                out2.truncate(pos + hdr_len - 4);
                out2.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
                // Basic Offset Table item: (FFFE,E000) length 0.
                out2.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
                out2.extend_from_slice(&0u32.to_le_bytes());
                // The single frame item.
                out2.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
                out2.extend_from_slice(&(frame.len() as u32).to_le_bytes());
                out2.extend_from_slice(frame);
                if !frame.len().is_multiple_of(2) {
                    out2.push(0x00);
                }
                // Sequence delimiter (FFFE,E0DD).
                out2.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
                out2.extend_from_slice(&0u32.to_le_bytes());
                return Some(out2);
            }

            // Native: raw value is exactly the frame bytes.
            let mut out2 = Vec::with_capacity(pos + hdr_len + frame.len() + 2);
            out2.extend_from_slice(&out);
            out2.truncate(pos + hdr_len - 4);
            out2.extend_from_slice(&(frame.len() as u32).to_le_bytes());
            out2.extend_from_slice(frame);
            if !frame.len().is_multiple_of(2) {
                out2.push(0x00); // PS3.5 7.1 value pad
            }
            return Some(out2);
        }
        if tag == TAG_PIXEL_DATA {
            return None;
        }
        // Advance by the element extent (skip nested undefined-length).
        match element_extent(head, pos) {
            Some((len, _vs, ve)) => pos = ve + (len % 2),
            None => {
                let vr = [head[pos + 4], head[pos + 5]];
                let vs = if is_extended_vr_bytes(vr) {
                    pos + 12
                } else {
                    pos + 8
                };
                pos = undefined_length_end(head, vs)?;
            }
        }
    }
    None
}

/// First byte position of the dataset's element stream.
///
/// Skips a Part 10 preamble + "DICM" (128 + 4) and, when present, the whole
/// meta group (group 0002). Falls back to 0 for a headerless dataset.
fn pixel_data_scan_start(data: &[u8]) -> usize {
    let mut pos = 0;
    if data.len() >= 132 && &data[128..132] == b"DICM" {
        pos = 132;
    }
    // Meta group elements all have group 0002; the dataset starts at the
    // first tag whose group is not 0002.
    while pos + 8 <= data.len() {
        let group = read_u16_at(data, pos);
        if group != 0x0002 {
            return pos;
        }
        match element_extent(data, pos) {
            // Advance past the element, padding to an even boundary on the
            // VALUE length (PS3.5 7.1: an odd value is followed by one pad
            // byte). Using the total element length here would misalign the
            // cursor and the scan would never find PixelData.
            Some((len, _vs, ve)) => pos = ve + (len % 2),
            // The meta group is defined-length, so an undefined length here
            // means the file is not shaped as expected: stop and let the
            // caller treat the dataset as starting at the current position.
            None => return pos,
        }
    }
    pos
}

/// `(value_len, value_start, value_end)` for the element header at `pos`.
///
/// Implicit VR (no VR bytes) and explicit VR short/long are distinguished the
/// same way the main parser does. Undefined lengths are rejected: a frame
/// table only makes sense over measured values.
fn element_extent(data: &[u8], pos: usize) -> Option<(usize, usize, usize)> {
    if pos + 8 > data.len() {
        return None;
    }
    let vr = [data[pos + 4], data[pos + 5]];
    let (length, value_start) = if is_extended_vr_bytes(vr) {
        if pos + 12 > data.len() {
            return None;
        }
        (read_u32_at(data, pos + 8) as usize, pos + 12)
    } else if is_vr_pair(vr) {
        (read_u16_at(data, pos + 6) as usize, pos + 8)
    } else {
        // Implicit VR little endian: 4-byte length, no VR.
        (read_u32_at(data, pos + 4) as usize, pos + 8)
    };
    if length == 0xFFFF_FFFF {
        return None; // undefined length
    }
    let value_end = value_start.checked_add(length)?;
    if value_end > data.len() {
        return None;
    }
    Some((length, value_start, value_end))
}

/// First byte AFTER an undefined-length element that starts at `pos`.
///
/// An undefined-length sequence (or item) ends at the Sequence/Item
/// Delimitation Item that closes it, not at a declared length. Such sequences
/// are routine in real files (Siemens private groups, nested functional
/// groups), so a scan that treated them as an error would give up before
/// reaching PixelData and silently produce no frame table - which is exactly
/// what happened on a real XA file whose (0029,1200) private sequence is
/// undefined-length.
///
/// Walks item headers, counting nesting, and returns the position just past
/// the delimitation that closes the OUTERMOST element.
fn undefined_length_end(data: &[u8], pos: usize) -> Option<usize> {
    let mut nesting: u32 = 0;
    let mut p = pos;
    while p + 8 <= data.len() {
        let group = read_u16_at(data, p);
        let element = read_u16_at(data, p + 2);
        if group == 0xFFFE && element == 0xE000 {
            // Item: an undefined item nests one level deeper.
            let len = read_u32_at(data, p + 4);
            p += 8;
            if len == 0xFFFF_FFFF {
                nesting += 1;
            } else {
                p = p.saturating_add(len as usize);
            }
            continue;
        }
        if group == 0xFFFE && (element == 0xE00D || element == 0xE0DD) {
            // Item / Sequence Delimitation Item.
            p += 8;
            if nesting == 0 {
                return Some(p);
            }
            nesting -= 1;
            continue;
        }
        // A nested element: skip it by its own extent, or descend again when
        // it is itself undefined-length.
        match element_extent(data, p) {
            Some((len, _vs, ve)) => p = ve + (len % 2),
            None => {
                let vr = [data[p + 4], data[p + 5]];
                let value_start = if is_extended_vr_bytes(vr) {
                    p + 12
                } else {
                    p + 8
                };
                p = undefined_length_end(data, value_start)?;
            }
        }
    }
    None
}

/// True for the explicit-VR VRs that carry a 4-byte length (PS3.5 7.1.2).
fn is_extended_vr_bytes(vr: [u8; 2]) -> bool {
    matches!(
        &vr,
        b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
    )
}

/// True when two bytes look like a DICOM VR (two uppercase letters).
///
/// Used only to tell explicit from implicit VR in this standalone scan. A
/// false negative (an implicit element whose length bytes happen to look like
/// letters) would misread one element and abort the scan, in which case no
/// chunk table is produced and the whole-blob path is kept - the safe
/// direction.
fn is_vr_pair(vr: [u8; 2]) -> bool {
    vr[0].is_ascii_uppercase() && vr[1].is_ascii_uppercase()
}

// -- Metadata helpers -------------------------------------------------

/// Compute the size of one uncompressed frame in bytes.
///
/// Formula (PS3.5 Section 8.1.1):
/// `frame_size = Rows x Columns x SamplesPerPixel x ceil(BitsAllocated / 8)`
///
/// Returns `None` if any parameter is zero (invalid).
pub fn compute_frame_size(
    rows: u16,
    columns: u16,
    samples_per_pixel: u16,
    bits_allocated: u16,
) -> Option<usize> {
    if rows == 0 || columns == 0 || bits_allocated == 0 {
        return None;
    }
    if bits_allocated == 1 {
        // PS3.5 Section 8.1.1: one bit per pixel, each row padded to the
        // next byte boundary. The frame is rows packed bit-rows, NOT
        // rows x columns bytes (the old ceil(bits/8)=1 reading inflated
        // a 1-bit frame 8x - the 20.09 SEG /frames bug).
        let row_bytes = (columns as usize).div_ceil(8);
        return Some(rows as usize * samples_per_pixel as usize * row_bytes);
    }
    let bytes_per_sample = (bits_allocated as usize).div_ceil(8);
    if bytes_per_sample == 0 {
        return None;
    }
    Some(rows as usize * columns as usize * samples_per_pixel as usize * bytes_per_sample)
}

/// Parse a DICOM integer tag value from raw bytes.
///
/// Handles both binary integer VRs (US, SS, UL, SL) and
/// ASCII numeric string VRs (IS, DS for integer values).
/// Tries ASCII parsing first (most metadata tags are ASCII IS),
/// falls back to binary for 2- and 4-byte values.
pub fn parse_dicom_int(data: &[u8]) -> Option<u32> {
    if data.is_empty() {
        return None;
    }

    // Try ASCII string first - handles IS (Integer String) and DS.
    // The UTF-8 check must be NON-FATAL: a binary US value whose bytes are
    // not valid UTF-8 (e.g. Rows=430 -> [0xAE,0x01]) previously aborted the
    // whole function before reaching the binary fallbacks (02.09 field
    // incident: /frames 500 on an RLE US because parse_dicom_int((0028,0010))
    // returned None).
    if let Ok(s) = std::str::from_utf8(data) {
        let trimmed = s.trim_end_matches('\0').trim();
        if let Ok(v) = trimmed.parse::<u32>() {
            return Some(v);
        }
    }

    // Binary unsigned short (2 bytes) - US
    if data.len() == 2 {
        return Some(u16::from_le_bytes([data[0], data[1]]) as u32);
    }

    // Binary unsigned long (4 bytes) - UL
    if data.len() == 4 {
        let val = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        return Some(val);
    }

    None
}

// -- Tests ------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Build a minimal encapsulated Pixel Data for testing.
    // Structure: [Item tag(4)] [BOT length=0(4)] [Item#1(4+len1+data1)] [Item#2(4+len2+data2)] [SeqDelim(4+0)]
    // DICOM tags are always little-endian: (FFFE,E000) = [FE,FF,00,E0]
    fn make_encapsulated(frames: &[&[u8]]) -> Vec<u8> {
        let mut buf = Vec::new();
        // Basic Offset Table Item (empty)
        buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]); // tag (FFFE,E000) LE
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // length = 0

        for frame_data in frames {
            // Item tag
            buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]); // (FFFE,E000) LE
            let len = frame_data.len() as u32;
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(frame_data);
        }

        // Sequence Delimiter
        buf.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]); // (FFFE,E0DD) LE
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // length = 0

        buf
    }

    #[test]
    fn test_extract_frame_native_first() {
        // 3 frames of 4 bytes each
        let pixel_data = b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c";
        let result = extract_frame(pixel_data, 1, 4, 3).unwrap();
        assert_eq!(result, b"\x01\x02\x03\x04");
    }

    #[test]
    fn test_extract_frame_native_second() {
        let pixel_data = b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c";
        let result = extract_frame(pixel_data, 2, 4, 3).unwrap();
        assert_eq!(result, b"\x05\x06\x07\x08");
    }

    #[test]
    fn test_extract_frame_native_third() {
        let pixel_data = b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c";
        let result = extract_frame(pixel_data, 3, 4, 3).unwrap();
        assert_eq!(result, b"\x09\x0a\x0b\x0c");
    }

    #[test]
    fn test_extract_frame_native_odd_frame_size() {
        // 3 frames of 3 bytes each -> padded to 4 bytes
        let pixel_data = b"\x01\x02\x03\x00\x04\x05\x06\x00\x07\x08\x09\x00";
        let result = extract_frame(pixel_data, 1, 3, 3).unwrap();
        assert_eq!(result, b"\x01\x02\x03"); // returns actual frame_size, not padded
    }

    #[test]
    fn test_extract_frame_encapsulated_first() {
        let enc = make_encapsulated(&[b"\xAA\xBB\xCC", b"\xDD\xEE\xFF", b"\x11\x22\x33"]);
        let result = extract_frame(&enc, 1, 0, 3).unwrap();
        assert_eq!(result, b"\xAA\xBB\xCC");
    }

    #[test]
    fn test_extract_frame_encapsulated_third() {
        let enc = make_encapsulated(&[b"\xAA\xBB\xCC", b"\xDD\xEE\xFF", b"\x11\x22\x33"]);
        let result = extract_frame(&enc, 3, 0, 3).unwrap();
        assert_eq!(result, b"\x11\x22\x33");
    }

    #[test]
    fn test_extract_frame_out_of_range() {
        let pixel_data = b"\x01\x02\x03\x04";
        let err = extract_frame(pixel_data, 5, 4, 1).unwrap_err();
        assert!(matches!(err, FrameExtractionError::FrameOutOfRange { .. }));
    }

    #[test]
    fn test_extract_frame_empty_data() {
        let err = extract_frame(b"", 1, 4, 1).unwrap_err();
        assert!(matches!(err, FrameExtractionError::TruncatedData));
    }

    #[test]
    fn test_compute_frame_size_8bit_monochrome() {
        // 512x512, 1 sample, 8 bits -> 262144 bytes
        let size = compute_frame_size(512, 512, 1, 8).unwrap();
        assert_eq!(size, 262_144);
    }

    #[test]
    fn test_compute_frame_size_16bit_monochrome() {
        // 256x256, 1 sample, 16 bits -> 131072 bytes
        let size = compute_frame_size(256, 256, 1, 16).unwrap();
        assert_eq!(size, 131_072);
    }

    #[test]
    fn test_compute_frame_size_rgb() {
        // 512x512, 3 samples, 8 bits -> 786432 bytes
        let size = compute_frame_size(512, 512, 3, 8).unwrap();
        assert_eq!(size, 786_432);
    }

    #[test]
    fn test_compute_frame_size_1bit_packed() {
        // PS3.5 8.1.1: one bit per pixel, rows padded to byte boundaries.
        // 776x776 -> 776 rows x 97 bytes = 75272 (the 20.09 SEG shape).
        let size = compute_frame_size(776, 776, 1, 1).unwrap();
        assert_eq!(size, 75_272);
        // 100x100 -> 100 rows x ceil(100/8)=13 bytes = 1300.
        let size = compute_frame_size(100, 100, 1, 1).unwrap();
        assert_eq!(size, 1_300);
    }

    #[test]
    fn test_extract_frame_1bit_packed_second() {
        // Two 1-bit frames, each a row-padded bit mask; the second frame
        // must be sliced at the bit-packed size, not at rows x cols bytes.
        let rows = 2u16;
        let cols = 10u16; // row_bytes = 2
        let frame_size = compute_frame_size(rows, cols, 1, 1).unwrap(); // 4 bytes
        assert_eq!(frame_size, 4);
        let mut pixel_data = Vec::new();
        // frame 1: 2 rows x 2 bytes each
        pixel_data.extend_from_slice(&[0b1010_1010, 0b1111_0000, 0b1010_1010, 0b1111_0000]);
        // frame 2
        pixel_data.extend_from_slice(&[0b0101_0101, 0b0000_1111, 0b0101_0101, 0b0000_1111]);
        let second = extract_frame(&pixel_data, 2, frame_size, 2).unwrap();
        assert_eq!(
            second,
            vec![0b0101_0101, 0b0000_1111, 0b0101_0101, 0b0000_1111]
        );
    }

    #[test]
    fn test_parse_dicom_int_us() {
        // US (unsigned short) - 2 bytes LE
        assert_eq!(parse_dicom_int(&[0x00, 0x02]), Some(512));
    }

    #[test]
    fn test_parse_dicom_int_ul() {
        // UL (unsigned long) - 4 bytes LE
        assert_eq!(parse_dicom_int(&[0x40, 0x00, 0x00, 0x00]), Some(64));
    }

    #[test]
    fn test_parse_dicom_int_is() {
        // IS (Integer String) - ASCII
        assert_eq!(parse_dicom_int(b"120 "), Some(120));
        assert_eq!(parse_dicom_int(b"1\0"), Some(1));
    }

    #[test]
    fn test_parse_dicom_int_us_with_non_utf8_bytes() {
        // 02.09 field incident: Rows=430 -> [0xAE,0x01]; 0xAE is not valid
        // UTF-8 and used to abort before the binary fallback.
        assert_eq!(parse_dicom_int(&[0xAE, 0x01]), Some(430));
        assert_eq!(parse_dicom_int(&[0xC8, 0x00]), Some(200));
        assert_eq!(parse_dicom_int(&[0xFF, 0x1F]), Some(8191));
    }

    #[test]
    fn test_frame_chunks_native_ow_pixel_data_after_meta_group() {
        // Regression: a real XA/angio file stores native frames under an OW
        // (extended-VR, 4-byte length) PixelData that follows a populated
        // meta group and many odd-length elements. The earlier padding
        // arithmetic advanced by the ELEMENT length instead of the VALUE
        // length, so the scan misaligned and produced an EMPTY table for a
        // perfectly multi-frame instance.
        let mut file = vec![0u8; 128];
        file.extend_from_slice(b"DICM");
        // Meta group with one odd-length value to exercise the pad byte.
        push_elem(&mut file, 0x0002, 0x0001, b"OB", &[0x00, 0x01, 0x00]);
        push_elem(&mut file, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
        // Dataset, with an odd-length SH before the pixel data.
        push_elem(&mut file, 0x0008, 0x0060, b"CS", b"XA");
        push_elem(&mut file, 0x0008, 0x103e, b"LO", b"Coro NDx"); // 9 bytes -> odd
        push_elem(&mut file, 0x0028, 0x0010, b"US", &2u16.to_le_bytes());
        push_elem(&mut file, 0x0028, 0x0011, b"US", &2u16.to_le_bytes());
        push_elem(&mut file, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
        push_elem(&mut file, 0x0028, 0x0008, b"IS", b"3 ");
        let frames: Vec<u8> = (0u8..12).collect();
        push_elem(&mut file, 0x7FE0, 0x0010, b"OW", &frames);

        let chunks = compute_frame_chunks(&file);
        assert_eq!(chunks.len(), 3, "OW-tagged native multi-frame must chunk");
        for (i, chunk) in chunks.iter().enumerate() {
            let start = chunk.offset as usize;
            let end = start + chunk.len as usize;
            assert_eq!(&file[start..end], &frames[i * 4..i * 4 + 4]);
        }
    }

    #[test]
    fn test_frame_chunks_skips_undefined_length_sequence() {
        // Regression from a real vendor XA file: a private group element
        // declared with an UNDEFINED length sits between the geometry tags and
        // PixelData. The scan used to abort there, yielding no frame table for
        // an 83-frame instance. It must skip the sequence instead.
        let mut file = vec![0u8; 128];
        file.extend_from_slice(b"DICM");
        push_elem(&mut file, 0x0008, 0x0060, b"CS", b"XA");
        push_elem(&mut file, 0x0028, 0x0010, b"US", &2u16.to_le_bytes());
        push_elem(&mut file, 0x0028, 0x0011, b"US", &2u16.to_le_bytes());
        push_elem(&mut file, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
        push_elem(&mut file, 0x0028, 0x0008, b"IS", b"2 ");
        // (0029,1200) SQ with UNDEFINED length containing one item.
        file.extend_from_slice(&0x0029u16.to_le_bytes());
        file.extend_from_slice(&0x1200u16.to_le_bytes());
        file.extend_from_slice(b"SQ");
        file.extend_from_slice(&[0, 0]);
        file.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        //   Item, defined length, one inner element.
        file.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        let mut inner = Vec::new();
        push_elem(&mut inner, 0x0029, 0x1001, b"LO", b"PRIVATE");
        file.extend_from_slice(&(inner.len() as u32).to_le_bytes());
        file.extend_from_slice(&inner);
        //   Sequence Delimitation Item ends the undefined-length sequence.
        file.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);

        let frames: Vec<u8> = (0u8..8).collect();
        push_elem(&mut file, 0x7FE0, 0x0010, b"OB", &frames);

        let chunks = compute_frame_chunks(&file);
        assert_eq!(
            chunks.len(),
            2,
            "the scan must skip the undefined-length sequence and reach PixelData"
        );
        for (i, chunk) in chunks.iter().enumerate() {
            let start = chunk.offset as usize;
            let end = start + chunk.len as usize;
            assert_eq!(&file[start..end], &frames[i * 4..i * 4 + 4]);
        }
    }

    #[test]
    fn test_frame_chunks_above_u16_max_frame_number() {
        // A whole-slide instance exceeds 65535 tiles. The frame numbers must
        // keep counting past that, because a wider record field is useless if
        // the producer stops at 65535. Built cheap: 65537 items of 8 bytes
        // each (~1 MB), which is a real multi-frame shape, not a stub.
        const COUNT: u32 = 65_537;
        let mut pixel = Vec::with_capacity(8 + COUNT as usize * 16 + 8);
        // Basic Offset Table (empty).
        pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
        for i in 0..COUNT {
            pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
            pixel.extend_from_slice(&8u32.to_le_bytes());
            pixel.extend_from_slice(&(i as u64).to_le_bytes());
        }
        pixel.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);

        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0008), b"IS", COUNT.to_string().as_bytes()),
            ((0x7FE0, 0x0010), b"OB", &pixel),
        ]);

        let chunks = compute_frame_chunks(&file);
        assert_eq!(chunks.len(), COUNT as usize, "every frame must be tabled");
        assert_eq!(chunks.last().unwrap().number, COUNT);
        assert!(
            chunks.last().unwrap().number > u32::from(u16::MAX),
            "the last frame number must exceed the u16 ceiling"
        );
        // The last chunk still addresses its own bytes.
        let last = chunks.last().unwrap();
        let start = last.offset as usize;
        let end = start + last.len as usize;
        assert_eq!(
            &file[start..end],
            &(COUNT as u64 - 1).to_le_bytes(),
            "the >u16::MAX chunk points at its own payload"
        );
    }

    #[test]
    fn test_parse_dicom_int_ascii_beats_binary() {
        // Two ASCII bytes "10" are IS, not US 0x3031.
        assert_eq!(parse_dicom_int(b"10"), Some(10));
    }

    // -- compute_frame_chunks -----------------------------------------

    /// One element of a synthetic test file: `(tag, VR, value bytes)`.
    type TestElement<'a> = ((u16, u16), &'a [u8], &'a [u8]);

    /// Build a minimal explicit-VR-little-endian DICOM file:
    /// preamble + DICM + the given (tag, VR, value) elements in order.
    fn make_file(elements: &[TestElement<'_>]) -> Vec<u8> {
        let mut buf = vec![0u8; 128];
        buf.extend_from_slice(b"DICM");
        for ((g, e), vr, value) in elements {
            buf.extend_from_slice(&g.to_le_bytes());
            buf.extend_from_slice(&e.to_le_bytes());
            buf.extend_from_slice(vr);
            if vr == b"OB" || vr == b"OW" || vr == b"UN" || vr == b"SQ" {
                buf.extend_from_slice(&[0, 0]);
                buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
            } else {
                buf.extend_from_slice(&(value.len() as u16).to_le_bytes());
            }
            buf.extend_from_slice(value);
            // PS3.5 7.1: an odd-length value is followed by one pad byte. The
            // parser aligns on this, so the fixture must write it or the next
            // element header is read one byte early.
            if !value.len().is_multiple_of(2) {
                buf.push(0x00);
            }
        }
        buf
    }

    fn us(v: u16) -> Vec<u8> {
        v.to_le_bytes().to_vec()
    }

    /// Append one explicit-VR-LE element, padding an odd value to even.
    fn push_elem(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
        buf.extend_from_slice(&group.to_le_bytes());
        buf.extend_from_slice(&element.to_le_bytes());
        buf.extend_from_slice(vr);
        let extended = matches!(vr, b"OB" | b"OW" | b"UN" | b"SQ" | b"OF" | b"UT");
        if extended {
            buf.extend_from_slice(&[0, 0]);
            buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
        } else {
            buf.extend_from_slice(&(value.len() as u16).to_le_bytes());
        }
        buf.extend_from_slice(value);
        if !value.len().is_multiple_of(2) {
            buf.push(0x00);
        }
    }

    #[test]
    fn test_frame_chunks_encapsulated_offsets_match_bytes() {
        // The whole point of the table: a byte-range read must return exactly
        // the frame that a full-blob extraction returns.
        let f1 = b"AAAA";
        let f2 = b"BBBBBB";
        let f3 = b"C";
        let pixel = make_encapsulated(&[f1, f2, f3]);
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)), // Rows
            ((0x0028, 0x0011), b"US", &us(2)), // Columns
            ((0x0028, 0x0008), b"IS", b"3 "),  // NumberOfFrames
            ((0x7FE0, 0x0010), b"OB", &pixel), // PixelData
        ]);

        let chunks = compute_frame_chunks(&file);
        assert_eq!(chunks.len(), 3, "one chunk per frame");
        assert_eq!(chunks[0].number, 1);
        assert_eq!(chunks[1].number, 2);
        assert_eq!(chunks[2].number, 3);

        // The decisive check: slicing the FILE by each chunk reproduces the
        // frame byte-for-byte.
        for (chunk, expected) in chunks.iter().zip([f1.as_slice(), f2, f3]) {
            let start = chunk.offset as usize;
            let end = start + chunk.len as usize;
            assert_eq!(&file[start..end], expected, "chunk {}", chunk.number);
        }
    }

    /// PS3.5 A.4: a frame need not be one fragment. A table chunk is a
    /// CONTIGUOUS byte range, so a frame spanning two fragments cannot be
    /// described by one - the second fragment's item header sits between them.
    /// Renumbering the fragments as frames served the tail of an image as if it
    /// were an image; refusing sends the caller to the extraction path, which
    /// assembles the frame properly.
    #[test]
    fn test_frame_chunks_refuses_a_frame_that_spans_fragments() {
        // Frame 1 spans fragments A1+A2; frame 2 is the single fragment B.
        let a1 = [0x11u8; 4];
        let a2 = [0x22u8; 6];
        let b = [0x33u8; 3];

        let mut pixel = Vec::new();
        // Basic Offset Table: two entries, the second frame starting after
        // both A items.
        let frame2_log = 8 + a1.len() + 8 + a2.len();
        pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        pixel.extend_from_slice(&8u32.to_le_bytes());
        pixel.extend_from_slice(&0u32.to_le_bytes());
        pixel.extend_from_slice(&(frame2_log as u32).to_le_bytes());
        for frag in [&a1[..], &a2[..], &b[..]] {
            pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
            pixel.extend_from_slice(&(frag.len() as u32).to_le_bytes());
            pixel.extend_from_slice(frag);
        }
        pixel.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);

        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0008), b"IS", b"2 "), // NumberOfFrames
            ((0x7FE0, 0x0010), b"OB", &pixel),
        ]);
        assert!(
            compute_frame_chunks(&file).is_empty(),
            "a multi-fragment frame has no contiguous byte range"
        );

        // One fragment per frame, with the table agreeing, still works.
        let f1 = [0x44u8; 4];
        let f2 = [0x55u8; 4];
        let mut pixel = Vec::new();
        let second_log = 8 + f1.len();
        pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        pixel.extend_from_slice(&8u32.to_le_bytes());
        pixel.extend_from_slice(&0u32.to_le_bytes());
        pixel.extend_from_slice(&(second_log as u32).to_le_bytes());
        for frag in [&f1[..], &f2[..]] {
            pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
            pixel.extend_from_slice(&(frag.len() as u32).to_le_bytes());
            pixel.extend_from_slice(frag);
        }
        pixel.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0008), b"IS", b"2 "),
            ((0x7FE0, 0x0010), b"OB", &pixel),
        ]);
        assert_eq!(compute_frame_chunks(&file).len(), 2);
    }

    #[test]
    fn test_frame_chunks_encapsulated_handles_sparse_tile() {
        let pixel = make_encapsulated(&[b"AAAA", b"", b"CC"]);
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0008), b"IS", b"3 "),
            ((0x7FE0, 0x0010), b"OB", &pixel),
        ]);
        let chunks = compute_frame_chunks(&file);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[1].len, 0, "sparse tile is empty, not skipped");
    }

    #[test]
    fn test_frame_chunks_native_offsets_match_bytes() {
        // 2 rows x 2 columns, 8 bits, 3 frames -> 4 bytes each, contiguous.
        let frames: Vec<u8> = (0u8..12).collect();
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)), // Rows
            ((0x0028, 0x0011), b"US", &us(2)), // Columns
            ((0x0028, 0x0100), b"US", &us(8)), // BitsAllocated
            ((0x0028, 0x0008), b"IS", b"3 "),  // NumberOfFrames
            ((0x7FE0, 0x0010), b"OB", &frames),
        ]);
        let chunks = compute_frame_chunks(&file);
        assert_eq!(chunks.len(), 3);
        for (i, chunk) in chunks.iter().enumerate() {
            let start = chunk.offset as usize;
            let end = start + chunk.len as usize;
            assert_eq!(
                &file[start..end],
                &frames[i * 4..i * 4 + 4],
                "native chunk {}",
                chunk.number
            );
        }
    }

    #[test]
    fn test_frame_chunks_single_frame_returns_empty_table() {
        // A single-frame instance is served whole; the table would be dead
        // weight in every manifest.
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0100), b"US", &us(8)),
            ((0x7FE0, 0x0010), b"OB", b"abcdefgh"),
        ]);
        assert!(compute_frame_chunks(&file).is_empty());
    }

    #[test]
    fn test_frame_chunks_missing_geometry_returns_empty_table() {
        // No frame table is better than a wrong one: the caller then keeps
        // the whole-blob path.
        let pixel = make_encapsulated(&[b"AAAA", b"BBBB"]);
        let file = make_file(&[((0x7FE0, 0x0010), b"OB", &pixel)]);
        assert!(compute_frame_chunks(&file).is_empty());
    }

    #[test]
    fn test_frame_chunks_truncated_encapsulation_is_rejected() {
        // A truncated item must not yield a chunk that reads past the blob.
        // The pixel data element is built with its length claiming one byte
        // more than is actually present, so the LAST frame's item runs past
        // the end of the file.
        let mut pixel = make_encapsulated(&[b"AAAA", b"BBBB"]);
        // Chop the final 4 bytes of frame 2's data (leaving the item header
        // intact but its payload short).
        let keep = pixel.len() - 12;
        pixel.truncate(keep);
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0008), b"IS", b"2 "),
            ((0x7FE0, 0x0010), b"OB", &pixel),
        ]);
        let chunks = compute_frame_chunks(&file);
        // Either the table is dropped entirely, or every chunk it DOES report
        // lies inside the file. It must never point past the end.
        for chunk in &chunks {
            assert!(
                (chunk.offset + chunk.len) as usize <= file.len(),
                "chunk {} reads past the blob",
                chunk.number
            );
        }
    }

    // -- compute_frame_chunks_from_file (streaming variant) -------------

    /// Write `bytes` to a fresh temp file and return the path plus a guard that
    /// removes it on drop. `frame.rs` carries no `tempfile` dev-dependency, so
    /// the name comes from pid + an atomic counter (the house unique-name
    /// pattern) - never a clock, which is not a uniqueness primitive.
    struct TempFile {
        path: std::path::PathBuf,
    }
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
    fn write_temp(bytes: &[u8]) -> (TempFile, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("clarus-frame-scan-{}-{n}.dcm", std::process::id()));
        std::fs::write(&path, bytes).expect("write fixture");
        (TempFile { path: path.clone() }, path)
    }

    /// Assert the file variant produces the SAME (number, offset, len) rows as
    /// the slice variant, for an arbitrary fixture.
    fn assert_file_matches_bytes(bytes: &[u8]) {
        let expected = compute_frame_chunks(bytes);
        let (_dir, path) = write_temp(bytes);
        let got = compute_frame_chunks_from_file(&path);
        assert_eq!(
            got.len(),
            expected.len(),
            "file variant frame count must match the slice variant"
        );
        for (g, e) in got.iter().zip(expected.iter()) {
            assert_eq!(g.number, e.number, "frame number");
            assert_eq!(g.offset, e.offset, "frame offset (file-absolute)");
            assert_eq!(g.len, e.len, "frame length");
        }
    }

    /// Append an explicit-VR-LE element with UNDEFINED length (0xFFFFFFFF) and
    /// the given value bytes. Used for the RMS-class encapsulated PixelData,
    /// whose value is an item stream terminated by FFFE,E0DD.
    fn push_elem_undefined(
        buf: &mut Vec<u8>,
        group: u16,
        element: u16,
        vr: &[u8; 2],
        value: &[u8],
    ) {
        buf.extend_from_slice(&group.to_le_bytes());
        buf.extend_from_slice(&element.to_le_bytes());
        buf.extend_from_slice(vr);
        // Extended VR only: OB/OW/... carry the 2 reserved bytes + 4-byte len.
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        buf.extend_from_slice(value);
    }

    #[test]
    fn test_file_variant_matches_bytes_encapsulated() {
        // Multi-frame encapsulated, with a zero-length (sparse) tile and a
        // sequence delimiter terminator.
        let pixel = make_encapsulated(&[b"AAAA", b"BBBBBB", b"", b"C"]);
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0008), b"IS", b"4 "),
            ((0x7FE0, 0x0010), b"OB", &pixel),
        ]);
        assert_file_matches_bytes(&file);
        // The sparse tile must survive as a zero-length chunk, not vanish.
        let (_dir, path) = write_temp(&file);
        let chunks = compute_frame_chunks_from_file(&path);
        assert_eq!(chunks[2].len, 0, "sparse tile is a zero-length chunk");
    }

    #[test]
    fn test_file_variant_matches_bytes_native() {
        // Native OW PixelData (extended VR, 4-byte length) after an odd-length
        // element - the same shape as the real XA regression fixture.
        let mut file = vec![0u8; 128];
        file.extend_from_slice(b"DICM");
        push_elem(&mut file, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
        push_elem(&mut file, 0x0008, 0x103e, b"LO", b"Coro NDx"); // 9 bytes -> odd
        push_elem(&mut file, 0x0028, 0x0010, b"US", &us(2));
        push_elem(&mut file, 0x0028, 0x0011, b"US", &us(2));
        push_elem(&mut file, 0x0028, 0x0100, b"US", &us(8));
        push_elem(&mut file, 0x0028, 0x0008, b"IS", b"3 ");
        let frames: Vec<u8> = (0u8..12).collect();
        push_elem(&mut file, 0x7FE0, 0x0010, b"OW", &frames);
        assert_file_matches_bytes(&file);
    }

    #[test]
    fn test_file_variant_skips_undefined_length_sequence() {
        // Same undefined-length private sequence as the slice regression: the
        // streaming walk must skip it and still reach PixelData.
        let mut file = vec![0u8; 128];
        file.extend_from_slice(b"DICM");
        push_elem(&mut file, 0x0008, 0x0060, b"CS", b"XA");
        push_elem(&mut file, 0x0028, 0x0010, b"US", &us(2));
        push_elem(&mut file, 0x0028, 0x0011, b"US", &us(2));
        push_elem(&mut file, 0x0028, 0x0008, b"IS", b"2 ");
        file.extend_from_slice(&0x0029u16.to_le_bytes());
        file.extend_from_slice(&0x1200u16.to_le_bytes());
        file.extend_from_slice(b"SQ");
        file.extend_from_slice(&[0, 0]);
        file.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        file.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        let mut inner = Vec::new();
        push_elem(&mut inner, 0x0029, 0x1001, b"LO", b"PRIVATE");
        file.extend_from_slice(&(inner.len() as u32).to_le_bytes());
        file.extend_from_slice(&inner);
        file.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);
        let frames: Vec<u8> = (0u8..8).collect();
        push_elem(&mut file, 0x7FE0, 0x0010, b"OB", &frames);
        assert_file_matches_bytes(&file);
    }

    #[test]
    fn test_file_variant_universality_large_per_frame_sequence() {
        // The universality proof: a conformant WSI carries a Per-frame
        // Functional Groups Sequence (5200,9230), UNDEFINED length, one item
        // per frame, BEFORE PixelData - so the header grows with the frame
        // count and is far larger than any fixed cap would allow. The file
        // variant must still produce the FULL table, proving the walk is
        // O(1) memory and header size is irrelevant.
        const COUNT: u32 = 400;
        let mut file = vec![0u8; 128];
        file.extend_from_slice(b"DICM");
        push_elem(&mut file, 0x0008, 0x0060, b"CS", b"SM");
        push_elem(&mut file, 0x0028, 0x0010, b"US", &us(2));
        push_elem(&mut file, 0x0028, 0x0011, b"US", &us(2));
        push_elem(
            &mut file,
            0x0028,
            0x0008,
            b"IS",
            COUNT.to_string().as_bytes(),
        );

        // (5200,9230) SQ, undefined length, COUNT items, each carrying a
        // non-trivial payload so the header is genuinely large.
        file.extend_from_slice(&0x5200u16.to_le_bytes());
        file.extend_from_slice(&0x9230u16.to_le_bytes());
        file.extend_from_slice(b"SQ");
        file.extend_from_slice(&[0, 0]);
        file.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        for f in 0..COUNT {
            // Item, undefined length, one element inside.
            file.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
            file.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            let mut item = Vec::new();
            // A ~64-byte payload per frame: a legitimately bulky per-frame
            // functional group, not a stub.
            item.extend_from_slice(&[0x00, 0x09]); // group 0009 (private)
            item.extend_from_slice(&[0x00, 0x10]);
            item.extend_from_slice(b"UN");
            item.extend_from_slice(&[0, 0]);
            item.extend_from_slice(&64u32.to_le_bytes());
            item.extend_from_slice(&(0..64u8).map(|b| b ^ (f as u8)).collect::<Vec<u8>>());
            file.extend_from_slice(&item);
            // Item Delimitation Item.
            file.extend_from_slice(&[0xFE, 0xFF, 0x0D, 0xE0, 0x00, 0x00, 0x00, 0x00]);
        }
        // Sequence Delimitation Item closes (5200,9230).
        file.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);

        let pixel = make_encapsulated(&[&[0xAA; 32][..]; COUNT as usize]);
        push_elem(&mut file, 0x7FE0, 0x0010, b"OB", &pixel);

        assert_file_matches_bytes(&file);
        let (_dir, path) = write_temp(&file);
        let chunks = compute_frame_chunks_from_file(&path);
        assert_eq!(
            chunks.len(),
            COUNT as usize,
            "a 400-frame per-frame-sequence header must still produce the full table"
        );
        assert_eq!(chunks.last().unwrap().number, COUNT);
    }

    #[test]
    fn test_file_variant_huge_declared_frames_does_not_allocate() {
        // F1: the table grows O(ACTUAL items walked) and is never sized by the
        // declared NumberOfFrames. A grossly over-declared instance is refused
        // in O(1) by the cap (the 0xFFFFFFFF case is pinned in the ap101 gate
        // `ap101_frame_chunks`); here a declaration that is merely larger than
        // the stored items must still yield the ACTUAL table, not a
        // declaration-sized allocation.
        let pixel = make_encapsulated(&[b"AAAA", b"BBBB", b"CCCC"]);
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            // Declares 500 frames; only 3 items are actually stored.
            ((0x0028, 0x0008), b"IS", b"500 "),
            ((0x7FE0, 0x0010), b"OB", &pixel),
        ]);
        let (_dir, path) = write_temp(&file);
        let chunks = compute_frame_chunks_from_file(&path);
        assert_eq!(
            chunks.len(),
            3,
            "the table is bounded by ACTUAL items, not the declared count"
        );
    }

    #[test]
    fn test_file_variant_huge_declared_frames_native_bounded_by_file() {
        // Same for the native branch: an over-declaration must clamp to what
        // the file actually holds (available / stride), never pre-allocate.
        let frames: Vec<u8> = (0u8..12).collect(); // 3 frames of 4 bytes
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0100), b"US", &us(8)),
            // Declares 500 frames; the file holds 3.
            ((0x0028, 0x0008), b"IS", b"500 "),
            ((0x7FE0, 0x0010), b"OB", &frames),
        ]);
        let (_dir, path) = write_temp(&file);
        let chunks = compute_frame_chunks_from_file(&path);
        assert_eq!(
            chunks.len(),
            3,
            "native clamps to the bytes actually present"
        );
        assert!(chunks.iter().all(|c| c.len == 4));
    }

    #[test]
    fn test_file_variant_failure_modes_are_empty_not_error() {
        // Unreadable path.
        assert!(
            compute_frame_chunks_from_file(std::path::Path::new("Z:/definitely/not/here.dcm"))
                .is_empty(),
            "a missing file yields an empty table, never a panic"
        );

        // Single-frame native (no useful table).
        let single = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0100), b"US", &us(8)),
            ((0x7FE0, 0x0010), b"OB", b"abcdefgh"),
        ]);
        let (_d1, p1) = write_temp(&single);
        assert!(compute_frame_chunks_from_file(&p1).is_empty());

        // Truncated encapsulated: no partial table, and never a chunk past EOF.
        let mut trunc = make_encapsulated(&[b"AAAA", b"BBBB"]);
        let keep = trunc.len() - 12;
        trunc.truncate(keep);
        let tf = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0008), b"IS", b"2 "),
            ((0x7FE0, 0x0010), b"OB", &trunc),
        ]);
        let (_d2, p2) = write_temp(&tf);
        let chunks = compute_frame_chunks_from_file(&p2);
        for c in &chunks {
            assert!(
                c.offset + c.len <= tf.len() as u64,
                "file variant must never point past EOF"
            );
        }

        // Garbage bytes (no DICM, no elements) -> empty.
        let (_d3, p3) = write_temp(b"\x00\x01\x02\x03 not a dicom file");
        assert!(compute_frame_chunks_from_file(&p3).is_empty());
    }

    // -- Undefined-length encapsulated PixelData (RMS class) -----------

    /// Build a multi-frame instance whose PixelData is UNDEFINED length
    /// (0xFFFFFFFF) - the standard encapsulation encoding (PS3.5 Table A.4-1)
    /// and exactly what the 13.09 GB RMS field file uses. The value is an
    /// empty BOT item, one item per tile, then the sequence delimiter.
    fn make_undefined_length_pixeldata_file(tiles: &[&[u8]]) -> Vec<u8> {
        let mut pixel = Vec::new();
        pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
        for tile in tiles {
            pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
            pixel.extend_from_slice(&(tile.len() as u32).to_le_bytes());
            pixel.extend_from_slice(tile);
        }
        pixel.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);

        let mut file = vec![0u8; 128];
        file.extend_from_slice(b"DICM");
        push_elem(&mut file, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.4.90");
        push_elem(&mut file, 0x0008, 0x0060, b"CS", b"SM");
        push_elem(&mut file, 0x0028, 0x0010, b"US", &us(2));
        push_elem(&mut file, 0x0028, 0x0011, b"US", &us(2));
        push_elem(&mut file, 0x0028, 0x0100, b"US", &us(8));
        push_elem(
            &mut file,
            0x0028,
            0x0008,
            b"IS",
            tiles.len().to_string().as_bytes(),
        );
        // The decisive difference from every other fixture: UNDEFINED length.
        push_elem_undefined(&mut file, 0x7FE0, 0x0010, b"OB", &pixel);
        file
    }

    #[test]
    fn test_both_variants_table_undefined_length_pixeldata() {
        // The RMS-class file: undefined-length encapsulated PixelData. Both
        // variants must produce the SAME full table. Before this fix both
        // returned EMPTY (element_extent rejects 0xFFFFFFFF), which forced the
        // /frames/{N} route onto the whole-blob read-back.
        let tiles: Vec<&[u8]> = vec![&[0xAA; 300], &[], &[0xCC; 512], &[0xDD; 64]];
        let file = make_undefined_length_pixeldata_file(&tiles);

        let slice = compute_frame_chunks(&file);
        assert_eq!(
            slice.len(),
            tiles.len(),
            "the SLICE variant must now table undefined-length PixelData"
        );
        let (_dir, path) = write_temp(&file);
        let streamed = compute_frame_chunks_from_file(&path);
        assert_eq!(
            streamed.len(),
            tiles.len(),
            "the FILE variant must table undefined-length PixelData"
        );

        // Parity: identical rows in both.
        for (s, f) in slice.iter().zip(streamed.iter()) {
            assert_eq!(s.number, f.number, "frame number");
            assert_eq!(s.offset, f.offset, "frame offset");
            assert_eq!(s.len, f.len, "frame length");
        }
        assert_eq!(slice[1].len, 0, "sparse tile survives as zero-length");

        // The decisive check: slicing the FILE by each recorded offset/len
        // reproduces the source tile byte-for-byte.
        for (chunk, tile) in streamed.iter().zip(tiles.iter()) {
            let start = chunk.offset as usize;
            let end = start + chunk.len as usize;
            assert_eq!(&file[start..end], *tile, "chunk {}", chunk.number);
        }
    }

    /// A crafted upload can nest undefined-length sequences until the stack
    /// runs out. The streaming walk is iterative, so depth costs a counter and
    /// never a stack frame: a recursion over these 5000 levels aborts the
    /// process, an iteration past the bound simply answers "no definite end".
    #[test]
    fn test_a_deeply_nested_undefined_sequence_does_not_overflow_the_stack() {
        // Deep enough that one stack frame per level cannot fit in the default
        // 2 MiB test thread stack (5000 x ~400 B already exceeds it), yet
        // small enough to build in memory.
        const DEPTH: usize = 5000;
        let mut file = vec![0u8; 128];
        file.extend_from_slice(b"DICM");
        for _ in 0..DEPTH {
            push_elem_undefined(&mut file, 0x0008, 0x1140, b"SQ", &[]);
            file.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
            file.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        }
        // One defined leaf, then DEPTH item and sequence delimiters.
        push_elem(&mut file, 0x0028, 0x0010, b"US", &us(2));
        for _ in 0..DEPTH {
            file.extend_from_slice(&[0xFE, 0xFF, 0x0D, 0xE0, 0, 0, 0, 0]);
            file.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0]);
        }

        let (_dir, path) = write_temp(&file);
        let mut f = File::open(&path).expect("open");
        // The depth cap rejects this input; what matters is that the call
        // RETURNS rather than aborting on stack exhaustion.
        let answer = stream_undefined_end(&mut f, 132);
        assert!(
            answer.is_none(),
            "the depth cap refuses, it does not recurse"
        );
    }

    #[test]
    fn test_undefined_length_pixeldata_parity_with_defined_length_shape() {
        let tiles: Vec<&[u8]> = vec![&[0x11; 128], &[0x22; 256], &[0x33; 32]];

        let undefined_file = make_undefined_length_pixeldata_file(&tiles);
        let pixel = make_encapsulated(&tiles);
        let defined_file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0008), b"IS", b"3 "),
            ((0x7FE0, 0x0010), b"OB", &pixel),
        ]);

        let undef = compute_frame_chunks(&undefined_file);
        let def = compute_frame_chunks(&defined_file);
        assert_eq!(undef.len(), def.len(), "same frame count");
        // Offsets differ by the header-size delta, but the LENGTHS and the
        // served BYTES must match frame-for-frame.
        for (u, d) in undef.iter().zip(def.iter()) {
            assert_eq!(u.number, d.number, "frame number");
            assert_eq!(u.len, d.len, "frame length");
            let ub = &undefined_file[u.offset as usize..(u.offset + u.len) as usize];
            let db = &defined_file[d.offset as usize..(d.offset + d.len) as usize];
            assert_eq!(ub, db, "served bytes for frame {}", u.number);
        }
    }

    #[test]
    fn test_undefined_length_pixeldata_malformed_is_empty_not_partial() {
        // Undefined-length encapsulated but the item stream is truncated (no
        // E0DD delimiter). The walk must return EMPTY, never a partial table
        // and never a chunk past EOF.
        let mut pixel = Vec::new();
        pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
        // One item that claims 999999 bytes but has only 4.
        pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        pixel.extend_from_slice(&999_999u32.to_le_bytes());
        pixel.extend_from_slice(b"junk");

        let mut file = vec![0u8; 128];
        file.extend_from_slice(b"DICM");
        push_elem(&mut file, 0x0028, 0x0010, b"US", &us(2));
        push_elem(&mut file, 0x0028, 0x0011, b"US", &us(2));
        push_elem(&mut file, 0x0028, 0x0008, b"IS", b"5 ");
        push_elem_undefined(&mut file, 0x7FE0, 0x0010, b"OB", &pixel);

        assert!(
            compute_frame_chunks(&file).is_empty(),
            "a truncated undefined-length stream must not produce a partial table"
        );
        let (_dir, path) = write_temp(&file);
        assert!(compute_frame_chunks_from_file(&path).is_empty());
    }

    #[test]
    fn test_splice_frame_into_head_rebuilds_single_frame_instance() {
        // The D3 helper: a bounded head (elements before PixelData + the
        // PixelData header) PLUS one frame's bytes must parse as a
        // single-frame instance whose frame table is exactly [frame].
        let tiles: Vec<&[u8]> = vec![&[0xAA; 300], &[0xBB; 512], &[0xCC; 64]];
        let full = make_undefined_length_pixeldata_file(&tiles);

        let table = compute_frame_chunks(&full);
        assert_eq!(table.len(), tiles.len());

        // Head = everything up to and including the 12-byte PixelData header.
        let pd_tag_pos = full
            .windows(4)
            .position(|w| w == [0xE0, 0x7F, 0x10, 0x00])
            .expect("PixelData tag present");
        let head = &full[..pd_tag_pos + 12];

        // Splice the SECOND frame's raw item bytes (its stored bytes).
        let f2 = table[1];
        let f2_bytes = &full[f2.offset as usize..(f2.offset + f2.len) as usize];
        let spliced = splice_frame_into_head(head, f2_bytes).expect("splice ok");
        // The spliced instance holds one frame, so it is no longer multi-frame:
        // NumberOfFrames says 1 and the chunk table is empty by design (a
        // single-frame instance is served whole, not by byte range).
        assert_eq!(
            find_tag_u32(&spliced, TAG_NUMBER_OF_FRAMES),
            Some(1),
            "the spliced instance must declare the single frame it holds"
        );
        assert!(compute_frame_chunks(&spliced).is_empty());
        // Exactly one fragment item, carrying exactly the spliced frame, and
        // nothing after it but the sequence delimiter.
        let (pd_off, _) = find_tag_range(&spliced, TAG_PIXEL_DATA).expect("PixelData");
        let value = &spliced[pd_off..];
        let item = &value[8..]; // skip the empty Basic Offset Table item
        assert_eq!(&item[4..8], &(f2_bytes.len() as u32).to_le_bytes());
        assert_eq!(&item[8..8 + f2_bytes.len()], f2_bytes);
        let tail = 8 + f2_bytes.len();
        assert_eq!(&item[tail..tail + 8], &[0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0]);
    }

    #[test]
    fn test_find_tag_value_in_file_parity_with_extract_tag_value() {
        // A value PAST the head must be found by the seek-based read and equal
        // the whole-bytes `extract_tag_value` result.
        let lut = vec![0x37u8; 3000]; // a "big" bulk value after the head-sized prefix
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0008), b"IS", b"2 "),
            ((0x0028, 0x1101), b"OB", &lut),
        ]);
        let (_dir, path) = write_temp(&file);

        let got =
            find_tag_value_in_file(&path, (0x0028, 0x1101), 1 << 20).expect("the tag is present");
        assert_eq!(got, lut, "seek-based read equals the whole-bytes value");
    }

    #[test]
    fn test_find_tag_value_in_file_absent_is_none() {
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
        ]);
        let (_dir, path) = write_temp(&file);
        assert!(
            find_tag_value_in_file(&path, (0x0028, 0x1101), 1 << 20).is_none(),
            "an absent tag is None"
        );
    }

    #[test]
    fn test_find_tag_value_in_file_over_cap_is_none() {
        // A declared value larger than the cap must be refused (None) - and
        // the point of the guard is that it refuses BEFORE any allocation.
        let big = vec![0u8; 4096];
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x1101), b"OB", &big),
        ]);
        let (_dir, path) = write_temp(&file);
        assert!(
            find_tag_value_in_file(&path, (0x0028, 0x1101), 1024).is_none(),
            "a value over max_value must be refused"
        );
        // Exactly the cap is allowed (inclusive bound).
        assert_eq!(
            find_tag_value_in_file(&path, (0x0028, 0x1101), 4096)
                .expect("len == cap is inside the bound")
                .len(),
            4096
        );
    }

    #[test]
    fn test_find_tag_value_in_file_undefined_length_is_bounded() {
        // An undefined-length bulk value must be walked to its E0DD delimiter
        // and served; over cap it must be None.
        let mut pixel = Vec::new();
        pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
        pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        pixel.extend_from_slice(&64u32.to_le_bytes());
        pixel.extend_from_slice(&[0x9Eu8; 64]);
        pixel.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);

        let mut file = vec![0u8; 128];
        file.extend_from_slice(b"DICM");
        push_elem(&mut file, 0x0028, 0x0010, b"US", &us(2));
        push_elem(&mut file, 0x0028, 0x0011, b"US", &us(2));
        push_elem_undefined(&mut file, 0x0028, 0x1101, b"OB", &pixel);
        let (_dir, path) = write_temp(&file);

        // Bounded walk returns the item stream (BOT + one item + delimiter):
        // it must be non-empty and NOT exceed the cap.
        let v = find_tag_value_in_file(&path, (0x0028, 0x1101), 1 << 20)
            .expect("undefined-length value is served");
        assert!(!v.is_empty(), "the walked value is non-empty");
        assert!(v.len() <= 1 << 20);
        // A cap below the walked length refuses.
        assert!(
            find_tag_value_in_file(&path, (0x0028, 0x1101), 16).is_none(),
            "an undefined-length value over cap must be refused"
        );
    }
}
