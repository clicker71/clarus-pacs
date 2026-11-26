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
/// Tag: PhotometricInterpretation (0028,0004) - decides whether the stored
/// sample count matches `SamplesPerPixel` (PS3.3 C.7.6.3).
pub const TAG_PHOTOMETRIC_INTERPRETATION: (u16, u16) = (0x0028, 0x0004);
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
    /// The instance could not be READ. A stored blob that cannot be opened is
    /// not an instance without a frame table, and reporting it as one would
    /// hand the caller a manifest row built from no bytes at all.
    Io(String),
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
            FrameTableError::Io(msg) => write!(f, "frame table unavailable: {msg}"),
        }
    }
}

impl std::error::Error for FrameTableError {}

// -- Public API --------------------------------------------------------

/// How the Pixel Data element was encoded on the wire.
///
/// PS3.5 A.4: encapsulated (compressed) Pixel Data has VR `OB`/`OW` with
/// UNDEFINED length and its value is a sequence of Items beginning with the
/// Basic Offset Table. Native Pixel Data is a defined-length value of raw
/// samples. This is a property of the ELEMENT HEADER, so it is known before a
/// single pixel byte is looked at - which is the point: a native image whose
/// first samples happen to be `FE FF 00 E0` is not an encapsulated one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelDataEncoding {
    /// Defined-length value of raw samples.
    Native,
    /// Undefined-length value of length-prefixed Items (PS3.5 A.4).
    Encapsulated,
}

/// Extract a single frame from Pixel Data (7FE0,0010) whose encoding is KNOWN.
///
/// Prefer this over [`extract_frame`] wherever the caller has the element
/// header (the transfer syntax / whether PixelData was undefined-length): the
/// encoding then comes from the file structure instead of from two bytes of
/// image content.
///
/// # Arguments
/// * `pixel_data` - Raw bytes of the PixelData element value (after tag+length+VR).
/// * `frame_number` - 1-indexed frame number.
/// * `frame_size` - Size of one frame in bytes:
///   `Rows x Columns x SamplesPerPixel x ceil(BitsAllocated/8)`.
/// * `num_frames` - Total number of frames from NumberOfFrames (0028,0008).
/// * `encoding` - The encoding the caller read from the element header.
///
/// # Panics
/// Never panics - all error paths return `FrameExtractionError`.
pub fn extract_frame_with_encoding(
    pixel_data: &[u8],
    frame_number: u32,
    frame_size: usize,
    num_frames: u32,
    encoding: PixelDataEncoding,
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

    match encoding {
        PixelDataEncoding::Encapsulated => {
            extract_frame_encapsulated(pixel_data, frame_number, num_frames)
        }
        PixelDataEncoding::Native => {
            extract_frame_native(pixel_data, frame_number, frame_size, num_frames)
        }
    }
}

/// [`extract_frame_with_encoding`] for a native value whose frames are not
/// whole bytes.
///
/// PS3.5 8.1 concatenates the frames of a native value with no per-frame
/// padding, so a frame boundary falls mid-byte whenever the frame's bit length
/// is not a multiple of 8 and `frame_size` (a byte count) cannot name it. A
/// caller holding `Rows`/`Columns`/`SamplesPerPixel`/`BitsAllocated` knows the
/// exact bit width and should use this; the byte-sized entry points stay as
/// they are for the byte-aligned case they were written for.
pub fn extract_native_frame_bits(
    pixel_data: &[u8],
    frame_number: u32,
    frame_bits: usize,
    num_frames: u32,
) -> Result<Vec<u8>, FrameExtractionError> {
    if frame_number < 1 || frame_number > num_frames {
        return Err(FrameExtractionError::FrameOutOfRange {
            requested: frame_number,
            total: num_frames,
        });
    }
    if pixel_data.is_empty() {
        return Err(FrameExtractionError::TruncatedData);
    }
    if frame_bits == 0 {
        return Err(FrameExtractionError::InvalidMetadata(
            "frame bit length is zero".into(),
        ));
    }
    if frame_bits.is_multiple_of(8) {
        return extract_frame_native(pixel_data, frame_number, frame_bits / 8, num_frames);
    }
    extract_frame_native_bits(pixel_data, frame_number, frame_bits)
}

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
/// # Encoding detection
/// This entry point has no header, so it has to classify the value by its own
/// bytes: an encapsulated value begins with the Basic Offset Table's Item tag
/// (FFFE,E000). A caller that DOES hold the header should call
/// [`extract_frame_with_encoding`] instead - see that function for why the
/// byte sniff is only a fallback.
///
/// # Panics
/// Never panics - all error paths return `FrameExtractionError`.
pub fn extract_frame(
    pixel_data: &[u8],
    frame_number: u32,
    frame_size: usize,
    num_frames: u32,
) -> Result<Vec<u8>, FrameExtractionError> {
    let encoding = if begins_with_item_header(pixel_data) {
        PixelDataEncoding::Encapsulated
    } else {
        PixelDataEncoding::Native
    };
    extract_frame_with_encoding(pixel_data, frame_number, frame_size, num_frames, encoding)
}

/// Whether `pixel_data` starts with an Item header (PS3.5 7.5).
///
/// The tag is `(FFFE,E000)` and the bytes follow the DATASET byte order, but
/// the two orders of that tag are `FE FF 00 E0` and `FF FE E0 00` - neither is
/// the other reversed in a way a native sample run can reach by accident less
/// easily than by design, so both are accepted here.
fn begins_with_item_header(pixel_data: &[u8]) -> bool {
    if pixel_data.len() < 4 {
        return false;
    }
    let le = pixel_data[..4] == [0xFE, 0xFF, 0x00, 0xE0];
    let be = pixel_data[..4] == [0xFF, 0xFE, 0xE0, 0x00];
    le || be
}

// -- Native (uncompressed) extraction ---------------------------------

/// Extract frame from native (uncompressed) Pixel Data.
///
/// PS3.5 8.2: native Pixel Data is one contiguous value - frame N+1 starts
/// immediately after frame N, at `frame_number * frame_size`. The only padding
/// in the encoding is the single byte that makes the WHOLE Value Field even
/// (PS3.5 7.1.1), so it lives after the last frame and never between two.
///
/// The previous version rounded every frame up to an even length and used that
/// as the stride, so two 3-byte frames were read as if they occupied 8 bytes:
/// frame 2 started at byte 4 instead of byte 3, returned `TruncatedData`, and
/// both indexers published a 2-byte chunk.
fn extract_frame_native(
    pixel_data: &[u8],
    frame_number: u32,
    frame_size: usize,
    _num_frames: u32,
) -> Result<Vec<u8>, FrameExtractionError> {
    // Checked arithmetic: an attacker-controlled NumberOfFrames or a huge
    // geometry must not wrap the offset into a valid-looking range.
    let offset = (frame_number as usize - 1)
        .checked_mul(frame_size)
        .ok_or(FrameExtractionError::TruncatedData)?;
    let end = offset
        .checked_add(frame_size)
        .ok_or(FrameExtractionError::TruncatedData)?;

    if end > pixel_data.len() {
        return Err(FrameExtractionError::TruncatedData);
    }

    Ok(pixel_data[offset..end].to_vec())
}

/// Extract a native frame whose BIT length is not a multiple of 8.
///
/// PS3.5 8.1 packs the samples of a Value Field continuously and concatenates
/// the frames with no per-frame padding
/// (standard/ps3.5/bit-packed-pixel-data-8.1.json, bitPackedContinuity +
/// multiFrameConcatenation), so frame N of a sub-byte-width image begins at
/// BIT `(N-1) * frame_bits`, which falls inside a byte. Answering with a byte
/// range serves bits that belong to two different frames - the failure the
/// byte-aligned path cannot even see.
///
/// The answer is the frame's bits RE-PACKED from bit 0 of the returned buffer
/// (so its first sample is the top of byte 0) and zero-padded to a whole byte.
/// That keeps the sample values identical while making the frame individually
/// decodable, which is the only form a caller holding one frame can use.
fn extract_frame_native_bits(
    pixel_data: &[u8],
    frame_number: u32,
    frame_bits: usize,
) -> Result<Vec<u8>, FrameExtractionError> {
    let first_bit = (frame_number as usize - 1)
        .checked_mul(frame_bits)
        .ok_or(FrameExtractionError::TruncatedData)?;
    let end_bit = first_bit
        .checked_add(frame_bits)
        .ok_or(FrameExtractionError::TruncatedData)?;
    let out_len = frame_bits.div_ceil(8);
    if end_bit.div_ceil(8) > pixel_data.len() {
        return Err(FrameExtractionError::TruncatedData);
    }
    let mut out = vec![0u8; out_len];
    for bit in 0..frame_bits {
        let src = first_bit + bit;
        let byte = pixel_data[src / 8];
        // PS3.5 8.1 (the Overlay Data packing rule the standard applies to all
        // bit-packed data): "the first bit of an Overlay Plane is encoded in
        // the least significant bit, immediately followed by the next bit of
        // the Overlay Plane in the next most significant bit". Byte
        // `src / 8` therefore holds plane bits `8k .. 8k+7` from bit 0 upward,
        // so the source mask counts UP from 0x01. Reading it downwards served
        // every one-bit sample in reverse order within each byte, which
        // mirrored a 1-bit image in groups of eight.
        let set = byte & (1u8 << (src % 8)) != 0;
        // The repack writes from bit 0 of the output in the SAME order, so the
        // first sample stays the first sample.
        if set {
            out[bit / 8] |= 1u8 << (bit % 8);
        }
    }
    Ok(out)
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
///
/// PS3.5 A.4 makes the Basic Offset Table the authoritative frame->fragment
/// map: one offset per frame, counted in bytes from the first byte of the
/// first Item TAG. A frame may span SEVERAL fragments, and treating item N as
/// frame N then serves the first fragment of such a frame as the whole frame
/// (a truncated codestream) and shifts every later frame. When the table is
/// present, AGREES with the items (non-decreasing, each offset on a fragment
/// boundary, the count equal to the item count) and yields the declared
/// frames, the frame is assembled from its fragments. Otherwise - an empty
/// table, or one the items do not bear out - the items themselves are the only
/// reading left, and they are read one item per frame.
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

    // Basic Offset Table data: offsets count from the FIRST ITEM TAG, which
    // is the byte `pos` is about to become.
    if pos + bot_length > data_len {
        return Err(FrameExtractionError::TruncatedData);
    }
    if !bot_length.is_multiple_of(4) {
        return Err(FrameExtractionError::InvalidEncapsulation(
            "Basic Offset Table length is not a multiple of 4".into(),
        ));
    }
    let fragments_start = pos;
    pos += bot_length;

    // -- Step 2: Iterate through frame Items -----------------------
    // Record every fragment's DATA range; the table (when usable) decides
    // how many of them make up the requested frame.
    let mut fragments: Vec<(usize, usize)> = Vec::new();

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
        // The item's data starts after its 8-byte header (tag + length); the
        // element item_elem was read from starts 4 bytes before `pos`.
        let item_tag_at = pos - 4;
        pos += 4;

        // Read item data
        if pos + item_length > data_len {
            return Err(FrameExtractionError::TruncatedData);
        }

        // The fragment's ITEM TAG offset, which is what the Basic Offset Table
        // counts from (PS3.5 A.4); its data begins 8 bytes later.
        fragments.push((item_tag_at, item_length));
        pos += item_length;
    }

    // Frames from the table, when it is present and the items bear it out.
    match frames_from_bot(
        pixel_data,
        fragments_start,
        bot_length,
        &fragments,
        num_frames,
    ) {
        BotVerdict::Ranges(frames) => {
            let index = (frame_number as usize).wrapping_sub(1);
            match frames.get(index) {
                Some(range) => Ok(pixel_data[range.clone()].to_vec()),
                None => Err(FrameExtractionError::FrameOutOfRange {
                    requested: frame_number,
                    total: frames.len() as u32,
                }),
            }
        }
        // PS3.5 A.4 lets a frame span fragments, and extraction returns an
        // OWNED buffer, so the item headers between the pieces can simply be
        // dropped: the caller gets the whole codestream. Rejecting here would
        // have made every multi-fragment frame unreadable even though the
        // table that describes it is complete.
        BotVerdict::Fragments(frames) => {
            let index = (frame_number as usize).wrapping_sub(1);
            match frames.get(index) {
                Some(ranges) => Ok(join_fragments(pixel_data, ranges)),
                None => Err(FrameExtractionError::FrameOutOfRange {
                    requested: frame_number,
                    total: frames.len() as u32,
                }),
            }
        }
        // With no table the items themselves are the only reading left, and
        // they are read one item per frame - but only as far as the instance
        // DECLARES frames. A stream holding more items than declared does not
        // say which item starts which frame, so an item past the declaration
        // is not a frame and is not served as one.
        BotVerdict::Absent => {
            let index = (frame_number as usize).wrapping_sub(1);
            match fragments.get(index) {
                Some(&(tag_at, len)) if index < num_frames as usize => {
                    Ok(pixel_data[tag_at + 8..tag_at + 8 + len].to_vec())
                }
                _ => Err(FrameExtractionError::FrameOutOfRange {
                    requested: frame_number,
                    total: num_frames.min(fragments.len() as u32),
                }),
            }
        }
    }
}

/// What the Basic Offset Table says about the items that follow it.
enum BotVerdict {
    /// One contiguous byte range per frame; the table is usable as-is.
    Ranges(Vec<std::ops::Range<usize>>),
    /// One frame per entry, in ITEM units: a frame may own several fragments,
    /// so no single byte range exists, but the payloads can be concatenated.
    /// Extraction can answer this; a byte-range indexer cannot.
    Fragments(Vec<Vec<std::ops::Range<usize>>>),
    /// No usable table, so one item per frame is the only reading left.
    Absent,
}

/// The fragments each frame owns, in table order, plus whether any frame owns
/// more than one fragment.
///
/// PS3.5 A.4: offset `i` is the byte distance from the first byte of the first
/// Item TAG to the first byte of the first Item TAG of frame `i`. A frame runs
/// from its own offset to the next one, and any number of fragments - each
/// introduced by its own 8-byte item header - lies between them. The table is
/// trusted only when its offsets are strictly increasing from ZERO, the entry
/// count matches the declared `NumberOfFrames`, and every offset lands exactly
/// on a fragment's item tag; otherwise the items do not bear the table out and
/// guessing which of the two is right is worse than reading the items.
///
/// The span is walked ONCE, in ordered lockstep with the fragment list: a
/// binary search per offset turned extracting one frame from a large tiled
/// image into O(n log n) work over the whole item stream.
fn frames_from_bot(
    pixel_data: &[u8],
    fragments_start: usize,
    bot_length: usize,
    fragments: &[(usize, usize)],
    num_frames: u32,
) -> BotVerdict {
    if bot_length == 0 {
        return BotVerdict::Absent; // empty: says nothing about frame boundaries
    }
    if !bot_length.is_multiple_of(4) {
        return BotVerdict::Absent; // not a table of 32-bit offsets
    }
    let count = bot_length / 4;
    // PS3.5 A.4: exactly one entry per frame, and the table indexes the frames
    // the instance DECLARES. A table naming more frames than that is not the
    // map this instance needs and is refused. A table naming FEWER still pins
    // the start of every frame it names, and A.4 makes the last named frame run
    // to the END of the item stream - so its trailing fragments belong to it,
    // which is what the ordered pass below does.
    if count > num_frames as usize {
        return BotVerdict::Absent;
    }
    let mut starts: Vec<usize> = Vec::with_capacity(count);
    for i in 0..count {
        let at = fragments_start + i * 4;
        starts.push(u32::from_le_bytes([
            pixel_data[at],
            pixel_data[at + 1],
            pixel_data[at + 2],
            pixel_data[at + 3],
        ]) as usize);
    }
    // The first frame necessarily begins at the first fragment: PS3.5 A.4
    // counts from that item's tag, so entry 0 is zero. A table whose first
    // entry is not zero describes an instance holding fragments before frame
    // one, and ordering alone would still let it pass and shift every frame.
    if starts.first() != Some(&0) {
        return BotVerdict::Absent;
    }
    if starts.windows(2).any(|w| w[0] >= w[1]) {
        // Not strictly increasing: not a frame map, so the items are the only
        // reading left.
        return BotVerdict::Absent;
    }
    let Some(&(origin, _)) = fragments.first() else {
        return BotVerdict::Absent; // no fragments: nothing to describe
    };
    let rel = |at: usize| at.wrapping_sub(origin);

    // One ordered pass over both lists. `starts` is already ascending, and so
    // is the fragment order, so a single cursor assigns every fragment to the
    // frame whose offset range contains it.
    let mut frames: Vec<Vec<std::ops::Range<usize>>> = vec![Vec::new(); count];
    let mut frame_index = 0usize;
    for &(tag_at, len) in fragments {
        let at = rel(tag_at);
        while frame_index + 1 < count && at >= starts[frame_index + 1] {
            frame_index += 1;
        }
        if at < starts[frame_index] {
            return BotVerdict::Absent; // a fragment before its frame's offset
        }
        let end = tag_at + 8 + len;
        if end > pixel_data.len() {
            return BotVerdict::Absent;
        }
        frames[frame_index].push(tag_at + 8..end);
    }
    if frames.iter().any(|f| f.is_empty()) {
        return BotVerdict::Absent; // a frame with no fragment of its own
    }
    if frames.iter().all(|f| f.len() == 1) {
        return BotVerdict::Ranges(frames.into_iter().flatten().collect());
    }
    BotVerdict::Fragments(frames)
}

/// Concatenate one frame's fragment payloads.
///
/// PS3.5 A.4: the fragments of a frame are its codestream in pieces; the item
/// headers between them are encapsulation, not payload. Joining them is what
/// makes a multi-fragment frame decodable at all - serving the first fragment
/// alone hands the decoder a truncated codestream and reports success.
fn join_fragments(pixel_data: &[u8], ranges: &[std::ops::Range<usize>]) -> Vec<u8> {
    let total: usize = ranges.iter().map(|r| r.end - r.start).sum();
    let mut out = Vec::with_capacity(total);
    for range in ranges {
        out.extend_from_slice(&pixel_data[range.clone()]);
    }
    out
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
    // F5: reject a huge DECLARATION before anything else - including the
    // geometry lookups. An instance that declares more frames than the cap
    // allows must be reported as such whether or not it also happens to be
    // missing Rows/Columns: the declaration alone is the fact the caller has to
    // act on, and returning `Ok(vec![])` first (because geometry was absent)
    // turned a rejectable instance into an accepted one with no table.
    let num_frames = find_tag_u32(dicom_bytes, TAG_NUMBER_OF_FRAMES).unwrap_or(1);
    if num_frames > cap {
        return Err(FrameTableError::TooManyFrames {
            declared: num_frames,
            walked: 0,
            cap,
        });
    }

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

    let (pd_offset, pd_len, pd_undefined) =
        match find_tag_range_with_mode(dicom_bytes, TAG_PIXEL_DATA) {
            Some(v) => v,
            None => {
                // Even a single-frame instance had its element found, so a missing
                // PixelData is a shape problem, not "nothing to table". The walk
                // below still decides whether there is anything to count.
                return Ok(Vec::new());
            }
        };
    // `pd_len` is the DEFINED value length, or "to EOF" for undefined-length
    // encapsulated PixelData (PS3.5 Table A.4-1). Only a genuinely empty
    // defined value is a dead end here.
    if pd_len == 0 {
        return Ok(Vec::new());
    }
    let pd_end = pd_offset.saturating_add(pd_len).min(dicom_bytes.len());
    let pixel_data = &dicom_bytes[pd_offset..pd_end];

    // Multi-frame is the only case where a chunk table buys anything: a
    // single-frame instance is served whole anyway. The item walk still runs
    // for an encapsulated value, because the walked-item cap is a fact about
    // the instance the caller must hear - a crafted single-frame declaration
    // over a stream of thousands of items must not pass silently. Only the
    // rows are dropped.
    // PS3.5 Table A.4-1: the encapsulated form has UNDEFINED length and the
    // native form a defined one, so the header decides this without looking at
    // a pixel byte.
    //
    // A DEFINED length still gets the item-tag probe, because encoders emit
    // encapsulated content defined as well (non-conformant under A.4, but
    // serviceable, and what this crate's fixtures build). The probe is not
    // trusted on its own, though: a native image's first samples can spell
    // FE FF 00 E0 (a 2-pixel 8-bit RGB frame reads R=254 G=255 B=0 E=224), and
    // walking sample data as items yields nothing usable. So a probed value is
    // only treated as encapsulated when the item walk actually produces a
    // table; otherwise it is native after all.
    let single = num_frames <= 1;
    let frame_size = frame_size_for(
        rows as u16,
        columns as u16,
        samples as u16,
        bits as u16,
        find_tag_value_bytes(dicom_bytes, TAG_PHOTOMETRIC_INTERPRETATION),
    )
    .unwrap_or(0);
    let native_table = |size: usize| {
        chunks_native(
            pd_offset,
            pd_end,
            size,
            num_frames,
            mid_byte_frame_bits(rows, columns, samples, bits),
        )
    };
    let table = if pd_undefined || pixel_data_starts_with_item(pixel_data) {
        let encapsulated =
            chunks_encapsulated(pixel_data, pd_offset, num_frames, cap, pd_undefined)?;
        if encapsulated.is_empty() && !pd_undefined && !is_item_stream(pixel_data) {
            // The header said defined AND the value is not shaped like an item
            // stream (no usable Basic Offset Table header), so the leading
            // bytes were pixel content that happened to spell an Item tag and
            // the value is native after all.
            //
            // A value that IS item-shaped but whose table this walk rejects
            // stays encapsulated and keeps its empty table: those bytes are an
            // encapsulation we refuse to trust, not an image, and re-reading
            // them as native samples would serve garbage.
            native_table(frame_size)
        } else {
            encapsulated
        }
    } else {
        native_table(frame_size)
    };
    Ok(if single { Vec::new() } else { table })
}

/// The bit width of one stored native frame, when that width makes the frames
/// NOT byte-aligned.
///
/// PS3.5 8.1 packs the samples of a Value Field continuously and concatenates
/// frames with no per-frame padding, so frame N begins at BIT (N-1)*frame_bits.
/// When that product is not a multiple of 8 the boundary falls INSIDE a byte,
/// and no byte range can name the frame: any range published for it contains
/// bits of its neighbour, and the reader has no way to notice. Returning `None`
/// therefore means "byte-aligned, a byte stride describes it exactly", which is
/// also the answer for any geometry the bit count cannot be formed from.
fn mid_byte_frame_bits(rows: u32, columns: u32, samples: u32, bits_allocated: u32) -> Option<u64> {
    let bits = (rows as u64)
        .checked_mul(columns as u64)?
        .checked_mul(samples as u64)?
        .checked_mul(bits_allocated as u64)?;
    if bits == 0 || bits.is_multiple_of(8) {
        return None;
    }
    Some(bits)
}

/// Whether a PixelData value begins with a Basic Offset Table item header.
///
/// PS3.5 Table A.4-1: an encapsulated value opens with the Basic Offset Table
/// Item - tag (FFFE,E000) then a 4-byte length - followed by the frame items.
/// This is the structural test used to decide whether a value that merely
/// STARTS with those four bytes is really an item stream: the length must also
/// fit inside the value, which pixel samples that happen to spell an Item tag
/// almost never do.
fn is_item_stream(pixel_data: &[u8]) -> bool {
    if !pixel_data_starts_with_item(pixel_data) || pixel_data.len() < 8 {
        return false;
    }
    let bot_len =
        u32::from_le_bytes([pixel_data[4], pixel_data[5], pixel_data[6], pixel_data[7]]) as usize;
    // The table body must fit, a table is a whole number of 32-bit offsets, and
    // at least one frame item has to follow the table and its own 8-byte header.
    bot_len
        .checked_add(8)
        .and_then(|after| after.checked_add(8))
        .is_some_and(|need| need <= pixel_data.len() && bot_len.is_multiple_of(4))
}

/// Whether a PixelData value begins with an Item tag (FFFE,E000).
///
/// A FALLBACK classifier, used only when the element header says the value has
/// a defined length: PS3.5 A.4 requires the encapsulated form to be
/// undefined-length, but encoders do emit it defined, and the item walk is the
/// only reading that makes such a value serviceable. The header therefore wins
/// when it says undefined; this never overrides it.
fn pixel_data_starts_with_item(pixel_data: &[u8]) -> bool {
    pixel_data.len() >= 4
        && pixel_data[0] == 0xFE
        && pixel_data[1] == 0xFF
        && pixel_data[2] == 0x00
        && pixel_data[3] == 0xE0
}

/// Native chunks: contiguous frames of `frame_size` bytes, even-padded.
///
/// `mid_byte_bits` is `Some` when a frame's bit length is not a multiple of 8.
/// Such frames have no byte-range boundary (PS3.5 8.1 packs them continuously),
/// so NO table is published: a byte range for one would carry bits of its
/// neighbour and the reader could not detect the mixture. An empty table sends
/// the caller to the whole-blob path, which loses the tile fast path but serves
/// the correct bytes - the safe direction, and the same one every other
/// unrepresentable shape takes here.
fn chunks_native(
    pd_offset: usize,
    pd_end: usize,
    frame_size: usize,
    num_frames: u32,
    mid_byte_bits: Option<u64>,
) -> Vec<FrameChunk> {
    if mid_byte_bits.is_some() {
        return Vec::new();
    }
    if frame_size == 0 {
        return Vec::new();
    }
    // PS3.5 8.1: multi-frame native values are concatenated with NO per-frame
    // padding - "the individual Frames are not padded. The individual Frames
    // shall be concatenated and padding bits (if necessary) applied to the
    // complete Value Field". The byte stride is therefore the frame size
    // itself. Rounding it up to even (an earlier reading of 8.1.1) put frame 2
    // one byte late for any odd-sized frame, and the table still looked
    // plausible: every offset was in range, just in the wrong frame. The
    // extraction path already used the unpadded stride, so the two disagreed.
    let stride = frame_size;
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
        // PS3.5 8.2: native frames are contiguous and unpadded between each
        // other, so a frame with fewer than `frame_size` bytes left in the
        // value is a truncated one. Publishing it would hand the serve path a
        // short read that decodes as a whole frame - the same bytes the
        // extraction path refuses - so the table is dropped instead, and the
        // caller keeps the whole-blob path which fails the same way for the
        // same frame.
        if len < frame_size {
            return Vec::new();
        }
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
    undefined: bool,
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
    // F5: the table is validated BEFORE it is copied. Copying the offsets
    // first and comparing the count with `cap` afterwards hands a crafted
    // 4-byte-per-entry table an allocation sized by the file rather than by
    // the caller's cap - the same amplification the declared-frame check
    // exists to prevent.
    if !bot_len.is_multiple_of(4) || bot_len / 4 > cap as usize {
        return Err(FrameTableError::TooManyFrames {
            declared: num_frames,
            walked: (bot_len / 4) as u32,
            cap,
        });
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
    let mut terminated = false;
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
            // PS3.5 Table A.4-1: the Sequence Delimitation Item closes the
            // value and is encoded with length ZERO. A delimiter carrying a
            // length is not one, so the item stream is not the shape we
            // understand - what follows may be payload the walk mistook for
            // items.
            if read_u32_at(pixel_data, pos + 4) != 0 {
                return Ok(Vec::new());
            }
            terminated = true;
            break;
        }
        if element != 0xE000 {
            return Ok(Vec::new());
        }
        // F5: trip the cap as soon as the WALKED count would exceed it. Checked
        // before the push so the table never grows past `cap` rows.
        if frame_no >= cap {
            return Err(FrameTableError::TooManyFrames {
                declared: num_frames,
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
    // PS3.5 Table A.4-1: an encapsulated value is a sequence of items closed by
    // a Sequence Delimitation Item. Reaching the end of the buffer after a
    // complete item means the delimiter is missing, so the instance is
    // truncated even though every fragment in hand is whole - and a table
    // built from it would describe a value that does not exist. Undefined
    // length is the only form that carries a delimiter, and `pd_len` runs to
    // EOF for it, so a defined-length value is left alone.
    if !terminated && undefined {
        return Ok(Vec::new());
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
        if frame_starts.first() != Some(&0) || frame_starts.len() != num_frames as usize {
            return Ok(Vec::new());
        }
        // One entry per frame, agreeing with the walk: item i IS frame i.
        if frame_starts.len() != out.len() {
            return Ok(Vec::new());
        }
        // PS3.5 A.4: offset `i` is the byte distance from the first byte of the
        // first Item TAG to the first byte of the first Item TAG of frame `i`.
        // Every entry must therefore land ON an Item tag - that is the only
        // thing that makes it a frame boundary rather than an arbitrary byte
        // count. Comparing against the walk's own arithmetic alone would accept
        // a table that agrees with a WRONG reading of the stream.
        let table_base = bot_start + bot_len;
        for &start in &frame_starts {
            let Some(tag_at) = table_base.checked_add(start) else {
                return Ok(Vec::new());
            };
            // PS3.5 A.4: offset `i` is the distance from the first Item TAG to
            // the first Item TAG of frame `i`, so the byte an entry addresses
            // IS an Item tag. The loop below already requires every entry to
            // equal the walk's own offset for that frame, and those offsets are
            // item-tag positions - so this check changes no verdict today. It
            // is kept as the direct statement of the rule: the equality holds
            // only because the walk happens to be synchronous with the table,
            // and a future change to either side should not be able to make a
            // table of non-tag offsets pass silently.
            if tag_at + 8 > data_len
                || read_u16_at(pixel_data, tag_at) != 0xFFFE
                || read_u16_at(pixel_data, tag_at + 2) != 0xE000
            {
                return Ok(Vec::new());
            }
        }
        for (chunk, &start) in out.iter().zip(frame_starts.iter()) {
            // BOT offsets count from the first fragment's ITEM TAG; a chunk
            // offset points at the item's VALUE, which is one 8-byte item
            // header later (PS3.5 A.4).
            let expect = chunk.offset as usize - pd_offset - table_base - 8;
            if start != expect {
                return Ok(Vec::new());
            }
        }
        return Ok(out);
    }
    // PS3.5 A.4: an EMPTY Basic Offset Table says nothing about frame
    // boundaries - it does not say "one item per frame". With no table, a frame
    // may still span several fragments, and the item count is then LARGER than
    // the frame count. Serving item i as frame i in that case hands out the
    // first fragment of a frame as the whole frame (a truncated codestream) and
    // shifts every later frame, all while the row count still looks plausible.
    //
    // Only MORE items than declared frames is refused. Fewer is the ordinary
    // over-declaration (a header claiming 500 frames over 3 stored items), where
    // the items are still whole frames and the table is merely short - that
    // stays as it was, because refusing it would drop the tile fast path for an
    // instance whose frames are perfectly servable.
    if out.len() > num_frames as usize {
        return Ok(Vec::new());
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

/// u16 at `at` in the given byte order, `None` when out of range.
///
/// Borrowed header walks must not index: the last element of a truncated file
/// leaves its length field half present, and an out-of-bounds index there is a
/// panic on input the walker is expected to refuse.
fn read_u16_enc(data: &[u8], at: usize, big_endian: bool) -> Option<u16> {
    let b = data.get(at..at + 2)?;
    Some(if big_endian {
        u16::from_be_bytes([b[0], b[1]])
    } else {
        u16::from_le_bytes([b[0], b[1]])
    })
}

/// u32 at `at` in the given byte order, `None` when out of range.
fn read_u32_enc(data: &[u8], at: usize, big_endian: bool) -> Option<u32> {
    let b = data.get(at..at + 4)?;
    Some(if big_endian {
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    } else {
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    })
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
    // A stored blob that cannot be OPENED is not an instance without a table.
    // Reporting `Ok(vec![])` here would let the caller write a manifest row
    // that claims the instance has no frames to serve, when in fact it never
    // read a byte of it.
    let mut file =
        File::open(path).map_err(|e| FrameTableError::Io(format!("{}: {e}", path.display())))?;
    match scan_frame_chunks(&mut file, cap) {
        Ok(chunks) => Ok(chunks),
        Err(ScanOutcome::NoTable) => Ok(Vec::new()),
        Err(ScanOutcome::TooMany(e)) => Err(e),
        Err(ScanOutcome::Io(e)) => Err(FrameTableError::Io(e)),
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
    find_tag_value_in_file_checked(path, target, max_value)
        .ok()
        .flatten()
}

/// [`find_tag_value_in_file`] with the I/O failure kept distinct from absence.
///
/// `Ok(None)` is "this instance has no such tag"; `Err` is "this blob could not
/// be read". The convenience wrapper above collapses both to `None`, which is
/// right for a route that serves an empty body either way, but a caller that
/// must tell an unavailable file from a valid negative result needs the
/// difference - a stored blob that will not open reported as an absent tag is
/// the kind of silent success this walk exists to avoid.
pub fn find_tag_value_in_file_checked(
    path: &Path,
    target: (u16, u16),
    max_value: usize,
) -> Result<Option<Vec<u8>>, std::io::Error> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let Some((mut pos, encoding)) = stream_scan_start_checked(&mut file)? else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "no readable dataset start",
        ));
    };
    // Any tag may be requested, so walk the element stream generically,
    // stepping over each value (sequences and undefined items are skipped
    // without buffering their bodies).
    while pos + 8 <= file_len {
        let Some(h) = read_element_header(&mut file, pos, encoding) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "truncated element header",
            ));
        };
        if (h.group, h.element) == target {
            if h.undefined {
                // Walk to the E0DD delimiter under a hard byte budget. The
                // budget bounds the WALK, not only the allocation: a value of
                // many tiny items would otherwise cost one header read per item
                // even when the caller asked for a few kilobytes - the walk
                // must not be the amplification the cap exists to prevent.
                let end = match stream_undefined_end_bounded(
                    &mut file,
                    h.value_start,
                    encoding,
                    max_value,
                ) {
                    Ok(end) => end,
                    // The caller's own budget, not a property of the file:
                    // this is a valid negative answer, not corruption.
                    Err(WalkStop::OverBudget) => return Ok(None),
                    Err(WalkStop::Malformed) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "undefined-length value has no delimiter",
                        ));
                    }
                };
                let len = end.checked_sub(h.value_start).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "value end precedes start")
                })?;
                if len > max_value as u64 {
                    return Ok(None);
                }
                let mut buf = vec![0u8; len as usize];
                stream_read_exact_at(&mut file, h.value_start, &mut buf).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short value read")
                })?;
                return Ok(Some(buf));
            }
            // Defined length: refuse BEFORE the allocation when over cap.
            if h.length > max_value as u64 {
                return Ok(None);
            }
            let mut buf = vec![0u8; h.length as usize];
            stream_read_exact_at(&mut file, h.value_start, &mut buf).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short value read")
            })?;
            return Ok(Some(buf));
        }
        // Not the target: step past it.
        pos = if h.undefined {
            let Some(end) = stream_undefined_end(&mut file, h.value_start, encoding) else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "undefined-length value has no delimiter",
                ));
            };
            end
        } else {
            let next = h
                .value_start
                .checked_add(h.length)
                .and_then(|v| v.checked_add(h.length % 2))
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "element runs past EOF")
                })?;
            // Overflow is not the only way to run past the end: a value whose
            // declared length is merely LARGER than the bytes that follow
            // computes fine and lands beyond EOF. The loop then exits on its
            // own bound and returns `Ok(None)`, which reads as "this instance
            // has no such tag" - a truncated blob reported as a valid negative.
            if next > file_len {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "element value runs past EOF",
                ));
            }
            next
        };
    }
    Ok(None)
}

/// Internal outcome of the streaming walk.
///
/// `NoTable` is the historical "keep the whole-blob path" result (any
/// malformed/truncated/unreadable shape, or a genuinely single-frame file).
/// `TooMany` is the F5 cap signal and is the ONLY case the caller must reject.
/// `Io` is a real failure to READ the blob, which is not the same answer as
/// "this instance needs no table" and is therefore kept distinct.
#[derive(Debug)]
enum ScanOutcome {
    NoTable,
    TooMany(FrameTableError),
    Io(String),
}

/// The streaming walk behind [`compute_frame_chunks_from_file`].
///
/// `Err(NoTable)` on any malformed/truncated/unreadable shape (the caller maps
/// it to an empty table). `Ok(vec![])` is the single-frame / no-useful-table
/// case. `Err(TooMany)` is the F5 cap signal.
fn scan_frame_chunks(file: &mut File, cap: u32) -> Result<Vec<FrameChunk>, ScanOutcome> {
    // The file was opened, so the size is a fact about a blob the caller CAN
    // read; needing it to know where the dataset ends makes its absence a real
    // read failure, not "this instance needs no table".
    let file_len = file
        .metadata()
        .map_err(|e| ScanOutcome::Io(format!("stat: {e}")))?
        .len();

    // -- 1. Skip preamble + meta group, land on the dataset start ------
    // A failure here is a READ failure, not "this instance needs no table":
    // the file opened and stat'd, so the bytes did not arrive as promised.
    // Collapsing it to `NoTable` made a truncated or unreadable blob
    // indistinguishable from a well-formed instance with nothing to table, and
    // the caller then wrote a manifest row saying the instance has no frames.
    let (dataset_start, encoding) = stream_scan_start_checked(file)
        .map_err(|e| ScanOutcome::Io(format!("dataset start: {e}")))?
        .ok_or(ScanOutcome::NoTable)?;

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
    // PhotometricInterpretation is a CS, so it is read into its own fixed
    // buffer. It outlives the walk because the frame size is computed after
    // PixelData is reached.
    let mut pi = [0u8; 16];
    let mut pi_len = 0usize;

    let (pd_offset, pd_len, pd_undefined) = loop {
        // A missing header means we ran out of the file without reaching
        // PixelData: no table (the caller keeps the whole-blob path), never a
        // partial one.
        let Some(header) = read_element_header(file, pos, encoding) else {
            return Ok(Vec::new());
        };
        let tag = (header.group, header.element);
        if tag == TAG_PIXEL_DATA {
            break (header.value_start, header.length, header.undefined);
        }

        // Geometry tag: read at most 16 bytes of the value, decode with the
        // SAME `parse_dicom_int` the slice path uses.
        //
        // PhotometricInterpretation is collected here too: it is a CS, not an
        // integer, and it decides whether the stored frame is as large as
        // SamplesPerPixel implies (PS3.3 C.7.6.3 - YBR_FULL_422 stores two
        // samples per pixel pair, not three).
        let geom_slot = match tag {
            TAG_ROWS => Some(&mut rows),
            TAG_COLUMNS => Some(&mut columns),
            TAG_SAMPLES_PER_PIXEL => Some(&mut samples),
            TAG_BITS_ALLOCATED => Some(&mut bits),
            TAG_NUMBER_OF_FRAMES => Some(&mut num_frames),
            _ => None,
        };
        if tag == TAG_PHOTOMETRIC_INTERPRETATION && !header.undefined {
            let n = header.length.min(pi.len() as u64) as usize;
            if n > 0 {
                stream_read_exact_at(file, header.value_start, &mut pi[..n])
                    .ok_or_else(|| ScanOutcome::Io("photometric value read".into()))?;
                pi_len = n;
            }
        }
        if let Some(slot) = geom_slot {
            if header.undefined {
                // An undefined-length integer makes no sense; treat the file
                // as unparsable rather than guessing.
                return Ok(Vec::new());
            }
            let n = header.length.min(geom.len() as u64) as usize;
            if n > 0 {
                stream_read_exact_at(file, header.value_start, &mut geom[..n])
                    .ok_or_else(|| ScanOutcome::Io("geometry value read".into()))?;
                // PS3.5 6.2: the VR fixes the representation. Rows/Columns are
                // US, NumberOfFrames is IS - reading both as ASCII turned a
                // Rows of 49 (`31 00`) into the text "1" and computed the
                // frame size from one row.
                *slot = match header.vr {
                    Some(vr) => parse_dicom_int_vr(&geom[..n], &vr, encoding.big_endian)
                        // An implicit-VR geometry element has no VR in the
                        // stream; fall back to the shape-blind reader.
                        .or_else(|| parse_dicom_int(&geom[..n])),
                    None => parse_dicom_int(&geom[..n]),
                };
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
            stream_undefined_end(file, header.value_start, encoding).ok_or(ScanOutcome::NoTable)?
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
    let declared_single = num_frames <= 1;
    if declared_single && !pd_undefined && pd_len == 0 {
        return Ok(Vec::new());
    }

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

    // -- 4. Determine encapsulation: header first, item probe as fallback ---
    // PS3.5 Table A.4-1 makes the encapsulated form undefined-length, so an
    // undefined header means encapsulated outright - and that verdict ignores
    // pixel content, which is the point: a native image whose first samples
    // spell FE FF 00 E0 was previously walked as an item stream and produced no
    // table at all.
    //
    // A DEFINED length still gets the item-tag probe, because encoders emit
    // encapsulated content defined as well (non-conformant under A.4, but
    // serviceable, and what this crate's fixtures build). Order matters: the
    // header decides when it says undefined, the probe only when it does not.
    // Both indexers use this same rule, so they cannot disagree on one file.
    let mut first4 = [0u8; 4];
    let probe = pd_end.saturating_sub(pd_offset).min(4) as usize;
    let probe_says_item = probe >= 4
        && stream_read_exact_at(file, pd_offset, &mut first4).is_some()
        && first4 == [0xFE, 0xFF, 0x00, 0xE0];
    let encapsulated = pd_undefined || probe_says_item;

    if encapsulated {
        // The cap is enforced by the walk, and its verdict is RETURNED: an
        // instance that stores more items than the cap allows must be rejected
        // even when it declares a single frame, because the item stream is
        // exactly the amplification the cap exists to bound.
        let table =
            chunks_encapsulated_stream(file, pd_offset, pd_end, num_frames, cap, pd_undefined)?;
        // A single-frame instance gets no table: it is served whole anyway. The
        // rows are dropped only AFTER the walk above ran, so the cap was
        // enforced on the real item count first - dropping first would have
        // skipped the walk and let a crafted single-frame declaration over
        // thousands of items pass silently.
        Ok(if declared_single { Vec::new() } else { table })
    } else if declared_single {
        // Native single frame: nothing to table and no item stream to count.
        Ok(Vec::new())
    } else {
        Ok(chunks_native_stream(
            pd_offset,
            pd_end,
            frame_size_for(
                rows as u16,
                columns as u16,
                samples as u16,
                bits as u16,
                // PS3.3 C.7.6.3: a YBR_FULL_422 frame stores two samples per
                // pixel pair, not SamplesPerPixel's nominal three.
                if pi_len > 0 {
                    Some(&pi[..pi_len])
                } else {
                    None
                },
            )
            .unwrap_or(0) as u64,
            num_frames,
            mid_byte_frame_bits(rows, columns, samples, bits),
        ))
    }
}

/// Whether the value at `pd_offset` begins with a Basic Offset Table Item.
///
/// Only the 4-byte Item tag is probed; the value's own length bound decides
/// whether the probe is possible at all.
/// The dataset encoding, resolved ONCE from the File Meta Information.
///
/// PS3.5 7.1: the dataset after group 0002 is Implicit VR or Explicit VR, and
/// little- or big-endian, and the only place that says which is
/// TransferSyntaxUID (0002,0010). A walker that infers the form from a
/// two-byte look-ahead guesses: in Implicit VR little endian the low bytes of a
/// value length can spell "OB", and in a big-endian dataset every tag and
/// length read little-endian is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DatasetEncoding {
    explicit_vr: bool,
    big_endian: bool,
}

impl DatasetEncoding {
    /// The encoding a TS UID denotes for the purposes of walking a dataset.
    ///
    /// The compressed transfer syntaxes (1.2.840.10008.1.2.4.x, .5.x) are all
    /// Explicit VR Little Endian datasets with encapsulated Pixel Data, and the
    /// deflated syntax is Explicit VR Little Endian under a compressed stream -
    /// which this walker does not inflate, so it is treated as explicit.
    fn from_ts_uid(uid: &[u8]) -> Self {
        let uid = std::str::from_utf8(uid).unwrap_or("");
        let uid = uid.trim_end_matches(['\0', ' ']);
        // 1.2.840.10008.1.2 is Implicit VR Little Endian; everything else the
        // engine accepts as a dataset encoding is Explicit VR. The two
        // big-endian syntaxes are explicit.
        let explicit_vr = uid != "1.2.840.10008.1.2" && !uid.is_empty();
        let big_endian = uid == "1.2.840.10008.1.2.2";
        Self {
            explicit_vr,
            big_endian,
        }
    }

    /// The default when a file has no File Meta Information: a bare dataset is
    /// Implicit VR Little Endian (PS3.5 A.1), the same default the parser uses
    /// for a file without the DICM prefix.
    const BARE: Self = Self {
        explicit_vr: false,
        big_endian: false,
    };
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
    /// The VR bytes in Explicit VR, `None` in Implicit VR where the VR is not
    /// in the stream at all.
    vr: Option<[u8; 2]>,
}

/// Read one element header at `pos` using the dataset's resolved encoding.
///
/// PS3.5 7.1.2 with 6.2 Table 6.2-1: in Explicit VR the two bytes after the tag
/// are a VR whose width class decides the header shape; in Implicit VR there is
/// no VR at all and the four bytes after the tag are a 32-bit length. Guessing
/// the form from those two bytes misreads an implicit length that happens to
/// spell a VR name. Returns `None` at/after EOF.
fn read_element_header(file: &mut File, pos: u64, enc: DatasetEncoding) -> Option<StreamHeader> {
    let mut h = [0u8; 12];
    let read_u16 = |b: [u8; 2]| {
        if enc.big_endian {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        }
    };
    let read_u32 = |b: [u8; 4]| {
        if enc.big_endian {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        }
    };

    if !enc.explicit_vr {
        // Implicit VR: tag(4) + ValueLength(4), no VR bytes.
        let mut h8 = [0u8; 8];
        stream_read_exact_at(file, pos, &mut h8)?;
        let raw = read_u32([h8[4], h8[5], h8[6], h8[7]]);
        return Some(StreamHeader {
            group: read_u16([h8[0], h8[1]]),
            element: read_u16([h8[2], h8[3]]),
            length: if raw == 0xFFFF_FFFF { 0 } else { raw as u64 },
            value_start: pos + 8,
            undefined: raw == 0xFFFF_FFFF,
            vr: None,
        });
    }

    // Explicit VR: the width class decides whether four more bytes follow.
    let mut h8 = [0u8; 8];
    stream_read_exact_at(file, pos, &mut h8)?;
    let group = read_u16([h8[0], h8[1]]);
    let element = read_u16([h8[2], h8[3]]);
    let vr = [h8[4], h8[5]];
    if !is_extended_vr_bytes(vr) {
        let raw = read_u16([h8[6], h8[7]]);
        return Some(StreamHeader {
            group,
            element,
            length: raw as u64,
            value_start: pos + 8,
            // A 16-bit field cannot express the undefined sentinel (PS3.5
            // 7.5.1), so 0xFFFF here is a real value length.
            undefined: false,
            vr: Some(vr),
        });
    }

    // Long form: VR(2) + reserved(2) + ValueLength(4).
    stream_read_exact_at(file, pos, &mut h)?;
    let raw = read_u32([h[8], h[9], h[10], h[11]]);
    Some(StreamHeader {
        group,
        element,
        length: if raw == 0xFFFF_FFFF { 0 } else { raw as u64 },
        value_start: pos + 12,
        undefined: raw == 0xFFFF_FFFF,
        vr: Some(vr),
    })
}

/// Dataset start: skip 128-byte preamble + "DICM" (when present) and the whole
/// meta group (group 0002), seeking past each value. Read+Seek port of
/// `pixel_data_scan_start`.
/// Dataset start and encoding: skip 128-byte preamble + "DICM" (when
/// present) and the whole meta group (group 0002), reading TransferSyntaxUID
/// (0002,0010) on the way.
///
/// PS3.5 7.1: the meta group is ALWAYS Explicit VR Little Endian, whatever the
/// dataset it describes is, so it is walked with that fixed encoding.
fn stream_scan_start(file: &mut File) -> Option<(u64, DatasetEncoding)> {
    stream_scan_start_checked(file).ok().flatten()
}

/// [`stream_scan_start`] keeping the difference between a SHORT READ and a
/// shape the walk cannot use.
///
/// `Ok(None)` is "no group-0002 element before the dataset"; `Err` is a read
/// that failed or bytes the walk could not decode. A caller that must tell an
/// unavailable blob from an absent prefix needs the two apart.
fn stream_scan_start_checked(
    file: &mut File,
) -> Result<Option<(u64, DatasetEncoding)>, std::io::Error> {
    const META: DatasetEncoding = DatasetEncoding {
        explicit_vr: true,
        big_endian: false,
    };
    let mut pos: u64 = 0;
    let mut magic = [0u8; 4];
    if stream_read_exact_at(file, 128, &mut magic).is_some() && &magic == b"DICM" {
        pos = 132;
    }
    // No File Meta Information (no DICM, or none were found before the
    // dataset): PS3.5 A.1 makes those files Implicit VR Little Endian.
    let mut encoding = DatasetEncoding::BARE;
    let file_len = file.metadata()?.len();
    // TransferSyntaxUID never exceeds 64 characters (PS3.5 6.2 UI).
    let mut uid = [0u8; 64];
    while pos + 8 <= file_len {
        let Some(header) = read_element_header(file, pos, META) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "truncated File Meta Information header",
            ));
        };
        if header.group != 0x0002 || header.undefined {
            return Ok(Some((pos, encoding)));
        }
        if (header.group, header.element) == (0x0002, 0x0010) {
            let n = header.length.min(uid.len() as u64) as usize;
            stream_read_exact_at(file, header.value_start, &mut uid[..n]).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "TransferSyntaxUID runs past EOF",
                )
            })?;
            encoding = DatasetEncoding::from_ts_uid(&uid[..n]);
        }
        pos = header
            .value_start
            .checked_add(header.length)
            .and_then(|v| v.checked_add(header.length % 2))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "File Meta Information element runs past EOF",
                )
            })?;
    }
    Ok(Some((pos, encoding)))
}

/// Why [`stream_undefined_end_bounded`] could not give an end.
#[derive(Debug, PartialEq, Eq)]
enum WalkStop {
    /// The value steps outside the caller's byte budget. Not a malformed file:
    /// the caller simply asked about a value larger than it will accept.
    OverBudget,
    /// The value has no delimiter, or is malformed. The file itself is wrong.
    Malformed,
}

/// [`stream_undefined_end`] that stops once `max_value` bytes have been walked.
///
/// The unbounded form answers "where does this value end", which for a value of
/// N tiny items costs N header reads. A caller with a byte budget only needs to
/// know whether the end lies INSIDE the budget, so the walk stops as soon as it
/// steps past it. The two failures are kept apart because only one of them says
/// anything about the file: an over-budget value is the caller's own limit, and
/// refusing it must not be reported as corruption.
fn stream_undefined_end_bounded(
    file: &mut File,
    pos: u64,
    enc: DatasetEncoding,
    max_value: usize,
) -> Result<u64, WalkStop> {
    const MAX_NESTING: u32 = 64;
    let file_len = file.metadata().map_err(|_| WalkStop::Malformed)?.len();
    // PS3.5 7.1.1: a value that ends EXACTLY at the budget is inside it, so the
    // bound is inclusive of the delimiter's final byte.
    let limit = pos
        .checked_add(max_value as u64)
        .ok_or(WalkStop::Malformed)?;
    let mut nesting: u32 = 0;
    let mut p = pos;
    while p + 8 <= file_len {
        if p > limit {
            return Err(WalkStop::OverBudget);
        }
        let mut h = [0u8; 8];
        stream_read_exact_at(file, p, &mut h).ok_or(WalkStop::Malformed)?;
        // PS3.5 7.5: Item (FFFE,E000) and the two delimitation items are
        // "not ruled by the VR encoding rules conveyed by the Transfer Syntax"
        // - they are encoded as Implicit VR, which 7.3 defines as Little
        // Endian. So the WHOLE 8-byte item header is little-endian even inside
        // a Big Endian data set. Reading the group and element in the data
        // set's order contradicted the length read two lines below and made an
        // item header unrecognisable there, so the walk could never find the
        // delimiter that ends an undefined-length value.
        let group = u16::from_le_bytes([h[0], h[1]]);
        let element = u16::from_le_bytes([h[2], h[3]]);
        if group == 0xFFFE && element == 0xE000 {
            let len = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
            p += 8;
            if len == 0xFFFF_FFFF {
                nesting += 1;
                if nesting > MAX_NESTING {
                    return Err(WalkStop::Malformed);
                }
            } else {
                p = p.checked_add(len as u64).ok_or(WalkStop::Malformed)?;
            }
            continue;
        }
        if group == 0xFFFE && (element == 0xE00D || element == 0xE0DD) {
            // The VALUE ends after the delimiter's own 8-byte header.
            p += 8;
            if p > limit {
                return Err(WalkStop::OverBudget);
            }
            if nesting == 0 {
                return Ok(p);
            }
            nesting -= 1;
            continue;
        }
        match read_element_header(file, p, enc).ok_or(WalkStop::Malformed)? {
            StreamHeader {
                undefined: true,
                value_start,
                ..
            } => {
                nesting += 1;
                if nesting > MAX_NESTING {
                    return Err(WalkStop::Malformed);
                }
                p = value_start;
            }
            StreamHeader {
                length,
                value_start,
                ..
            } => {
                p = value_start
                    .checked_add(length)
                    .and_then(|v| v.checked_add(length % 2))
                    .ok_or(WalkStop::Malformed)?;
            }
        }
    }
    Err(WalkStop::Malformed)
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
fn stream_undefined_end(file: &mut File, pos: u64, enc: DatasetEncoding) -> Option<u64> {
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
        // PS3.5 7.5: the 0xFFFE item header is Implicit VR, hence little-endian
        // for every transfer syntax - see the sibling walk above.
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
        match read_element_header(file, p, enc)? {
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
    let (mut pos, encoding) = stream_scan_start(&mut file)?;
    // The meta group may precede the dataset; to this walk it is just more
    // top-level elements, and the result is the same.
    let mut end = pos;
    while pos + 8 <= file_len {
        let h = read_element_header(&mut file, pos, encoding)?;
        if h.group == 0xFFFE {
            // A stray delimiter at the top level means the structure is not
            // what this walk assumes.
            return None;
        }
        pos = if h.undefined {
            stream_undefined_end(&mut file, h.value_start, encoding)?
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
///
/// `mid_byte_bits` is `Some` when a frame's bit length is not a multiple of 8 -
/// see `chunks_native`: such frames have no byte boundary, so no table.
fn chunks_native_stream(
    pd_offset: u64,
    pd_end: u64,
    frame_size: u64,
    num_frames: u32,
    mid_byte_bits: Option<u64>,
) -> Vec<FrameChunk> {
    if mid_byte_bits.is_some() {
        return Vec::new();
    }
    if frame_size == 0 {
        return Vec::new();
    }
    // PS3.5 8.1: frames are concatenated with no per-frame padding, so the
    // byte stride IS the frame size - see the slice variant, which must agree.
    let stride = frame_size;
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
        // PS3.5 8.2: see `chunks_native` - a short trailing frame is a
        // truncated instance, not a shorter frame, so the table is dropped.
        if len < frame_size {
            return Vec::new();
        }
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
    require_delimiter: bool,
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
    // Whether the value ended at a Sequence Delimitation Item. PS3.5
    // Table A.4-1: an encapsulated value is closed by one, so a value whose
    // buffer ends right after a whole fragment is truncated even though the
    // fragments in hand are intact.
    let mut terminated = false;
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
            // PS3.5 Table A.4-1: the delimiter carries length ZERO. A non-zero
            // length means the walk has been reading something other than the
            // item stream, so none of the rows above can be trusted.
            if u32::from_le_bytes([item[4], item[5], item[6], item[7]]) != 0 {
                return Ok(Vec::new());
            }
            terminated = true;
            break; // sequence delimiter
        }
        if element != 0xE000 {
            return Ok(Vec::new());
        }
        // F5: trip the cap before the push, so the table never grows past it.
        if frame_no >= cap {
            return Err(ScanOutcome::TooMany(FrameTableError::TooManyFrames {
                declared: num_frames,
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
    // A value that ran to EOF (undefined length) without a delimiter is
    // truncated, not merely unterminated: the instance promises a value that
    // ends at FFFE,E0DD and it does not. A DEFINED-length value is instead
    // bounded exactly by the element's own length, so reaching that bound is a
    // complete - if unconventional - encoding of the same items and stays
    // servable.
    if !terminated && require_delimiter {
        return Ok(Vec::new());
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
        let mut first = [0u8; 4];
        if stream_read_exact_at(file, bot_offset, &mut first).is_none() {
            return Ok(Vec::new());
        }
        // PS3.5 A.4: entry 0 is the distance from the first fragment's item
        // tag to itself, so it is zero. A table starting anywhere else
        // describes fragments before frame one and would shift every frame.
        if u32::from_le_bytes(first) != 0 {
            return Ok(Vec::new());
        }
        let mut table = vec![0u8; bot_len as usize];
        if stream_read_exact_at(file, bot_offset, &mut table).is_none() {
            return Ok(Vec::new());
        }
        for (chunk, entry) in out.iter().zip(table.chunks_exact(4)) {
            let start = u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]) as u64;
            // PS3.5 A.4: each offset counts bytes from the first byte of the
            // first ITEM TAG after the Basic Offset Table, and points at the
            // item tag of its own frame. `chunk.offset` is the first byte of
            // that frame's DATA, so the 8-byte item header is subtracted
            // before comparing. Measuring from the BOT's own tag instead added
            // that header to every expected value, so a valid table disagreed
            // with the walk and the whole table was discarded.
            let Some(expect) = chunk
                .offset
                .checked_sub(8)
                .and_then(|v| v.checked_sub(after_bot))
            else {
                return Ok(Vec::new());
            };
            if start != expect {
                return Ok(Vec::new());
            }
        }
        return Ok(out);
    }
    // PS3.5 A.4: an EMPTY Basic Offset Table says nothing about frame
    // boundaries. Only MORE items than declared frames means a frame spans
    // fragments (so item i is not frame i); fewer is the ordinary
    // over-declaration, whose items are still whole frames. See the slice
    // variant, which takes the same reading.
    if out.len() > num_frames as usize {
        return Ok(Vec::new());
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
    find_tag_range_with_mode(data, target).map(|(offset, len, _)| (offset, len))
}

/// [`find_tag_range`] that also reports whether the element was
/// UNDEFINED-length.
///
/// PS3.5 7.1.1: the two forms carry the same numbers to a caller that only
/// wants a byte range, but they differ in what closes the value - a delimiter
/// for the undefined form, the declared length for the defined one - so a
/// scanner that must know whether a terminator is required needs this.
fn find_tag_range_with_mode(data: &[u8], target: (u16, u16)) -> Option<(usize, usize, bool)> {
    let start = pixel_data_scan_start(data);
    let enc = dataset_encoding(data, start);
    let mut pos = start;
    while pos + 8 <= data.len() {
        let tag = (
            read_u16_enc(data, pos, enc.big_endian)?,
            read_u16_enc(data, pos + 2, enc.big_endian)?,
        );
        if tag == target {
            // An undefined-length TARGET (PixelData with 0xFFFFFFFF, the
            // standard encapsulation encoding: PS3.5 Table A.4-1) has no
            // declared length, but its value is legitimately "the rest of the
            // element stream". Report the value range to EOF; an encapsulated
            // consumer (`chunks_encapsulated`) walks the item stream and stops
            // at the FFFE,E0DD delimiter, so the to-EOF bound is exact. This
            // mirrors the file variant, which likewise uses EOF as the upper
            // bound for undefined-length PixelData.
            // PS3.5 7.1.1: the header width follows the dataset syntax, and
            // so does the length field - so the value start is measured from
            // the layout rather than guessed from the two bytes at the VR
            // offset. The byte sniff (`element_extent`) accepts the `FF FF` of
            // an undefined-length sentinel as a plausible VR and then reads a
            // 2-byte length of 65535, which puts the value start eight bytes
            // past the real one and yields a table of fragments that do not
            // exist.
            let (hdr_len, length, _) = element_layout(data, pos, enc)?;
            let value_start = pos + hdr_len;
            if value_start > data.len() {
                return None;
            }
            if length != UNDEFINED_LENGTH {
                if value_start.checked_add(length)? > data.len() {
                    return None;
                }
                return Some((value_start, length, false));
            }
            // An undefined-length value is legitimately "the rest of the
            // element stream" (PS3.5 Table A.4-1): an encapsulated consumer
            // walks the item stream and stops at the FFFE,E0DD delimiter, so
            // the to-EOF bound is exact. This mirrors the file variant.
            return Some((value_start, data.len() - value_start, true));
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
        match element_extent(data, pos, enc) {
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
                pos = undefined_length_end(data, value_start, enc)?;
            }
        }
    }
    None
}
/// Read a tag's value as u32 via [`find_tag_range`] plus a VR-aware decode.
///
/// PS3.5 6.2 Table 6.2-1 fixes the representation per VR, and the tag's own VR
/// bytes are in the stream, so there is no reason to guess: Rows = 49 is the
/// two bytes `31 00`, which an ASCII-first reading turns into the text "1".
fn find_tag_u32(data: &[u8], target: (u16, u16)) -> Option<u32> {
    let (offset, len) = find_tag_range(data, target)?;
    let end = offset.saturating_add(len).min(data.len());
    let value = data.get(offset..end)?;
    // PS3.5 7.1: a binary value's byte order follows the resolved dataset
    // encoding. Reading a Big Endian Rows/Columns pair little-endian turned
    // 512x256 into 2x1 - a wrong frame size that then sized every chunk.
    let start = pixel_data_scan_start(data);
    let big_endian = dataset_encoding(data, start).big_endian;
    // The VR sits 4 bytes before the value in Explicit VR; an Implicit VR
    // element carries none, so it comes from the dictionary (PS3.6) instead.
    // PS3.5 7.1: the VR is a property of the TAG, and the dictionary is the
    // only statement of it when the wire omits it - guessing from the value's
    // shape reads a US Rows of 49 (bytes `31 00`) as the ASCII text "1".
    let explicit_vr = match offset
        .checked_sub(4)
        .and_then(|vr_at| data.get(vr_at..vr_at + 2))
    {
        Some(vr) if crate::codec::is_long_form_vr(vr) || vr.iter().all(u8::is_ascii_uppercase) => {
            Some(vr)
        }
        _ => None,
    };
    match explicit_vr.and_then(|vr| parse_dicom_int_vr(value, vr, big_endian)) {
        Some(v) => Some(v),
        None => {
            // Implicit VR: the dictionary names the VR unconditionally.
            let dict_vr = crate::codec::vr_dict::vr_for_tag(target.0, target.1);
            parse_dicom_int_vr(value, dict_vr.as_bytes(), big_endian)
                // A tag the dictionary does not describe falls back to the
                // shape-blind reader; every geometry tag this crate reads is
                // in the dictionary, so this arm is defence only.
                .or_else(|| parse_dicom_int(value))
        }
    }
}

/// Raw VALUE bytes of a tag, or `None` when it is absent or runs past the end.
///
/// The caller gets the stored bytes without any interpretation, which is what a
/// CS attribute such as PhotometricInterpretation needs: its meaning is the
/// text itself, and decoding it as a number would be meaningless.
fn find_tag_value_bytes(data: &[u8], target: (u16, u16)) -> Option<&[u8]> {
    let (offset, len) = find_tag_range(data, target)?;
    let end = offset.checked_add(len)?.min(data.len());
    data.get(offset..end)
}

/// The header width and declared value length at `pos`, decoded by syntax.
///
/// PS3.5 7.1.1: an Explicit VR element carries its VR in the header, an
/// Implicit VR element carries none, and the two therefore differ in where the
/// length sits - and, for the long-form VRs, in its width. Reading the 4-byte
/// length of an Implicit VR element as a 2-byte one picks up the upper half of
/// the length word, which for any value under 64 KiB is zero.
///
/// Returns `(header_len, value_len, explicit)`; `value_len` is
/// `UNDEFINED_LENGTH` for an undefined-length element.
fn element_layout(data: &[u8], pos: usize, enc: DatasetEncoding) -> Option<(usize, usize, bool)> {
    if pos + 8 > data.len() {
        return None;
    }
    // PS3.5 7.1: the byte order belongs to the same resolved encoding as the VR
    // form. Reading a Big Endian Value Length little-endian yields a number
    // that is plausible and wrong, so the walk desynchronises silently.
    let read32 = |at: usize| -> Option<u32> { read_u32_enc(data, at, enc.big_endian) };
    let read16 = |at: usize| -> Option<u16> { read_u16_enc(data, at, enc.big_endian) };
    if !enc.explicit_vr {
        return Some((8, read32(pos + 4)? as usize, false));
    }
    let vr = [data[pos + 4], data[pos + 5]];
    if is_extended_vr_bytes(vr) {
        if pos + 12 > data.len() {
            return None;
        }
        return Some((12, read32(pos + 8)? as usize, true));
    }
    Some((8, read16(pos + 6)? as usize, true))
}

/// The dataset encoding resolved from the File Meta Information.
///
/// PS3.5 7.1: the form AND the byte order are properties of the
/// TransferSyntaxUID. Resolving both from the same element is what keeps a
/// header decode from mixing them - reading a Big Endian dataset's tags
/// little-endian yields plausible-looking tags and lengths that are simply
/// wrong, and the walk then reports either nothing or the wrong rows.
fn dataset_encoding(buf: &[u8], start: usize) -> DatasetEncoding {
    let mut pos = 0usize;
    // The File Meta Information is ALWAYS Explicit VR Little Endian and always
    // group 0002 (PS3.5 7.1), so it is walked in that form regardless of what
    // the dataset itself turns out to be.
    const META: DatasetEncoding = DatasetEncoding {
        explicit_vr: true,
        big_endian: false,
    };
    if buf.len() >= 132 && &buf[128..132] == b"DICM" {
        pos = 132;
    }
    while pos + 8 <= buf.len() {
        let (group, _element) = (read_u16_at(buf, pos), read_u16_at(buf, pos + 2));
        if group != 0x0002 {
            break;
        }
        let Some((hdr_len, len, _)) = element_layout(buf, pos, META) else {
            break;
        };
        if len == UNDEFINED_LENGTH {
            break;
        }
        if (group, _element) == (0x0002, 0x0010) {
            let end = pos + hdr_len + len;
            if end > buf.len() {
                break;
            }
            return DatasetEncoding::from_ts_uid(&buf[pos + hdr_len..end]);
        }
        // PS3.5 7.1.1: an odd-length Value Field is followed by a pad byte, so
        // the next element starts one byte later. Skipping it misaligned the
        // walk after any odd-length meta element and hid TransferSyntaxUID.
        match pos
            .checked_add(hdr_len)
            .and_then(|p| p.checked_add(len))
            .and_then(|p| p.checked_add(len % 2))
        {
            Some(next) if next > pos => pos = next,
            _ => break,
        }
    }
    // No File Meta Information before the dataset: PS3.5 A.1 makes a bare
    // dataset Implicit VR Little Endian, and this walk states that answer
    // rather than guessing an encoding from the first element's bytes.
    let _ = start;
    DatasetEncoding::BARE
}

/// The undefined-length sentinel of PS3.5 7.1.1, as a length in the walk.
///
/// frame.rs reads lengths with its own byte-order-aware helpers rather than
/// through `codec::vr_widths`, so the sentinel is spelled here too instead of
/// reaching across modules for one constant.
const UNDEFINED_LENGTH: usize = 0xFFFF_FFFF;

/// Write a DEFINED value length into the header that ends `buf` at
/// `pos + hdr_len`.
///
/// PS3.5 7.1.1: the field width follows the syntax - 4 bytes for a long-form
/// Explicit VR and for an Implicit VR element, 2 bytes for a short-form
/// Explicit VR one. Writing 4 bytes unconditionally left two bytes of the
/// length sitting in a short-form header, which the next element then read as
/// its tag.
fn write_value_len(buf: &mut [u8], pos: usize, hdr_len: usize, explicit: bool, len: usize) {
    let at = pos + hdr_len;
    if at < 4 || at > buf.len() {
        return;
    }
    let long_form = !explicit || is_extended_vr_bytes([buf[pos + 4], buf[pos + 5]]);
    if long_form {
        buf[at - 4..at].copy_from_slice(&(len as u32).to_le_bytes());
    } else {
        buf[at - 2..at].copy_from_slice(&(len as u16).to_le_bytes());
    }
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
    let start = pixel_data_scan_start(buf);
    let enc = dataset_encoding(buf, start);
    let mut pos = start;
    while let Some((hdr_len, len, _)) = element_layout(buf, pos, enc) {
        if (
            read_u16_enc(buf, pos, enc.big_endian),
            read_u16_enc(buf, pos + 2, enc.big_endian),
        ) == (Some(TAG_NUMBER_OF_FRAMES.0), Some(TAG_NUMBER_OF_FRAMES.1))
        {
            if text.len() > len || pos + hdr_len + len > buf.len() {
                return;
            }
            buf[pos + hdr_len..pos + hdr_len + len].fill(b' ');
            buf[pos + hdr_len..pos + hdr_len + text.len()].copy_from_slice(text.as_bytes());
            return;
        }
        if len == UNDEFINED_LENGTH {
            // PS3.5 7.5.1 scopes elements by tag order, not by form, so a
            // defined-length NumberOfFrames may follow an undefined-length
            // sequence - and in a WSI it does, behind the per-frame functional
            // groups. Giving up here left the declaration stale, so a spliced
            // single-frame header still claimed the original frame count and
            // every consumer sized the image from a number the buffer no longer
            // holds. Walk to the sequence's own delimiter instead and keep
            // going.
            match undefined_length_end(buf, pos + hdr_len, enc) {
                Some(next) if next > pos => pos = next,
                _ => return, // no delimiter: cannot step over it reliably
            }
            continue;
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
    let enc = dataset_encoding(head, start);
    let explicit = enc.explicit_vr;
    let mut pos = start;
    while pos + 8 <= head.len() {
        let tag = (
            read_u16_enc(head, pos, enc.big_endian)?,
            read_u16_enc(head, pos + 2, enc.big_endian)?,
        );
        if tag == TAG_PIXEL_DATA {
            // Header width depends on the encoding, and getting it wrong is
            // silent corruption: an explicit long VR (OB/OW/UN/SQ/...) carries
            // 2 reserved bytes plus a 4-byte length (12 total), every other
            // explicit VR carries a 2-byte length (8 total), and an Implicit VR
            // element has no VR bytes at all and a 4-byte length (8 total).
            // Reading the implicit case as 12 swallowed the first four pixel
            // bytes into the header and left a second length field behind.
            let (hdr_len, _, _) = element_layout(head, pos, enc)?;
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
            let declared = if explicit && !is_extended_vr_bytes([head[pos + 4], head[pos + 5]]) {
                read_u16_at(head, pos + hdr_len - 2) as u32
            } else {
                read_u32_at(head, pos + hdr_len - 4)
            };
            let encapsulated = declared == 0xFFFF_FFFF;

            if encapsulated {
                // New value = BOT item (empty) + one frame item + E0DD.
                let mut out2 = Vec::with_capacity(pos + hdr_len + frame.len() + 32);
                out2.extend_from_slice(&out);
                // PS3.5 Table A.4-1: an encapsulated value carries the
                // undefined-length sentinel, which is a 4-byte field - so the
                // header is cut back to where that field starts, whatever its
                // VR. The splice reads this mode from `element_layout` rather
                // than from the last four bytes of the header, so a short-form
                // VR with a 2-byte length field keeps its own width here.
                let sentinel_at = pos + hdr_len - 4;
                out2.truncate(sentinel_at);
                out2.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
                // Basic Offset Table item: (FFFE,E000) length 0.
                out2.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
                out2.extend_from_slice(&0u32.to_le_bytes());
                // The single frame item. PS3.5 7.1.1: an Item's Value Field
                // is padded to an even length and the pad byte belongs to the
                // declared length - writing the unpadded size left the parity
                // of the item stream and of the bytes on disk disagreeing.
                let padded = frame.len() + (frame.len() % 2);
                out2.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
                out2.extend_from_slice(&(padded as u32).to_le_bytes());
                out2.extend_from_slice(frame);
                if padded != frame.len() {
                    out2.push(0x00);
                }
                // Sequence delimiter (FFFE,E0DD).
                out2.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
                out2.extend_from_slice(&0u32.to_le_bytes());
                return Some(out2);
            }

            // Native: raw value is exactly the frame bytes. PS3.5 7.1.1: the
            // Value Field is padded to an even length and the padding byte is
            // PART of the value, so the declared length counts it. Declaring
            // the unpadded size made a 3-byte frame claim 3 and occupy 4, and
            // every element after PixelData (there are none today, but the
            // splice does not promise that) would be read one byte early.
            let padded = frame.len() + (frame.len() % 2);
            let mut out2 = Vec::with_capacity(pos + hdr_len + padded);
            out2.extend_from_slice(&out);
            write_value_len(&mut out2, pos, hdr_len, explicit, padded);
            out2.extend_from_slice(frame);
            if padded != frame.len() {
                out2.push(0x00); // PS3.5 7.1.1 value pad, counted by the length
            }
            return Some(out2);
        }
        if tag == TAG_PIXEL_DATA {
            return None;
        }
        // Advance by the element extent (skip nested undefined-length).
        match element_extent(head, pos, enc) {
            Some((len, _vs, ve)) => pos = ve + (len % 2),
            None => {
                let vr = [head[pos + 4], head[pos + 5]];
                let vs = if is_extended_vr_bytes(vr) {
                    pos + 12
                } else {
                    pos + 8
                };
                pos = undefined_length_end(head, vs, enc)?;
            }
        }
    }
    None
}

/// The File Meta Information encoding: Explicit VR Little Endian regardless of
/// what the dataset it describes uses (PS3.5 7.1, PS3.10 7.1).
const META_ENCODING: DatasetEncoding = DatasetEncoding {
    explicit_vr: true,
    big_endian: false,
};

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
        match element_extent(data, pos, META_ENCODING) {
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
fn element_extent(data: &[u8], pos: usize, enc: DatasetEncoding) -> Option<(usize, usize, usize)> {
    if pos + 8 > data.len() {
        return None;
    }
    // PS3.5 7.1: the Value Length follows the dataset's byte order as well, so
    // it comes from the resolved encoding rather than from a fixed reading.
    let vr = [data[pos + 4], data[pos + 5]];
    let (length, value_start) = if is_extended_vr_bytes(vr) {
        if pos + 12 > data.len() {
            return None;
        }
        (
            read_u32_enc(data, pos + 8, enc.big_endian)? as usize,
            pos + 12,
        )
    } else if enc.explicit_vr {
        (
            read_u16_enc(data, pos + 6, enc.big_endian)? as usize,
            pos + 8,
        )
    } else {
        // Implicit VR: no VR bytes, 4-byte length.
        (
            read_u32_enc(data, pos + 4, enc.big_endian)? as usize,
            pos + 8,
        )
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
fn undefined_length_end(data: &[u8], pos: usize, enc: DatasetEncoding) -> Option<usize> {
    undefined_length_end_at(data, pos, 0, enc)
}

/// Depth bound for [`undefined_length_end`]'s recursion.
///
/// Eight bytes of crafted input buy one level, so a 1 MiB payload buys ~130k
/// levels and the stack dies - under `panic=abort` that aborts the process.
/// frame.rs is reachable from STOW (`find_tag_range` ->
/// `compute_frame_chunks_capped`), so the depth must be bounded the way the
/// metadata parser already bounds its own walk (parser.rs
/// `MAX_UID_WALK_DEPTH`, same value): one rule, two files. Real objects nest a
/// handful of levels, so 64 is past anything PS3.5/PS3.3 produces.
const MAX_UNDEFINED_NESTING: usize = 64;

/// [`undefined_length_end`] with the depth explicitly counted.
fn undefined_length_end_at(
    data: &[u8],
    pos: usize,
    depth: usize,
    enc: DatasetEncoding,
) -> Option<usize> {
    // Fail closed: an over-deep walk yields "cannot measure", so the caller
    // keeps the whole-blob path instead of building a partial table.
    if depth >= MAX_UNDEFINED_NESTING {
        return None;
    }
    let mut nesting: u32 = 0;
    let mut p = pos;
    while p + 8 <= data.len() {
        // PS3.5 7.5: the 0xFFFE item header is Implicit VR, hence little-endian
        // for every transfer syntax; everything else follows the dataset.
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
        match element_extent(data, p, enc) {
            Some((len, _vs, ve)) => p = ve + (len % 2),
            None => {
                let vr = [data[p + 4], data[p + 5]];
                let value_start = if is_extended_vr_bytes(vr) {
                    p + 12
                } else {
                    p + 8
                };
                p = undefined_length_end_at(data, value_start, depth + 1, enc)?;
            }
        }
    }
    None
}

/// True for the explicit-VR VRs that carry a 4-byte length (PS3.5 7.1.2).
/// Whether a VR pair uses the long-form element header (PS3.5 6.2 Table 6.2-1).
///
/// Delegates to the shared classification: PS3.5 6.2 makes every VR outside
/// the short-form set long form, so `SV` and `UV` (added after the original
/// table) belong here even though an explicit allow-list written before them
/// did not name them. That list sent an `SV`/`UV` element down the short
/// branch, where its two reserved bytes were read as a 16-bit Value Length and
/// the walk desynchronised by two bytes at every such element.
fn is_extended_vr_bytes(vr: [u8; 2]) -> bool {
    crate::codec::is_long_form_vr(&vr)
}

// -- Metadata helpers -------------------------------------------------

/// The size of one stored frame, given the photometric interpretation.
///
/// PS3.3 C.7.6.3: `YBR_FULL_422` shares two chrominance samples between four
/// luminance samples, so `SamplesPerPixel` (3) is the NOMINAL channel count and
/// not the number of stored samples - a 422 frame holds `Rows*Columns*2`
/// samples, not `Rows*Columns*3`. Sizing it by `SamplesPerPixel` over-states
/// the frame by 50%, which puts every frame after the first at the wrong byte
/// offset: the offsets stay in range, so nothing complains, and each frame
/// decodes from the middle of its neighbour.
///
/// Every other photometric interpretation stores `SamplesPerPixel` samples per
/// pixel, so it delegates. Unknown or absent PI is treated as the general case:
/// guessing 422 from a missing attribute would shrink frames for the majority
/// of images, which is the worse error.
fn frame_size_for(
    rows: u16,
    columns: u16,
    samples_per_pixel: u16,
    bits_allocated: u16,
    photometric: Option<&[u8]>,
) -> Option<usize> {
    if is_ybr_full_422(photometric) {
        if rows == 0 || columns == 0 || bits_allocated == 0 || bits_allocated == 1 {
            // A 1-bit colour PI is not a shape the standard defines; leave it
            // to the general path rather than inventing a bit-packed 422.
            return compute_frame_size(rows, columns, samples_per_pixel, bits_allocated);
        }
        let bytes_per_sample = (bits_allocated as usize).div_ceil(8);
        if bytes_per_sample == 0 {
            return None;
        }
        // PS3.3 C.7.6.3: Rows and Columns describe the LUMINANCE plane; the
        // chrominance planes are half as wide, so two samples per pixel pair.
        return Some(rows as usize * columns as usize * 2 * bytes_per_sample);
    }
    compute_frame_size(rows, columns, samples_per_pixel, bits_allocated)
}

/// Whether a PhotometricInterpretation value is `YBR_FULL_422`.
///
/// PS3.5 6.2 makes CS space-padded and case-insensitive in comparison terms;
/// trimming and comparing exactly (no case folding) matches how every other CS
/// comparison in this crate reads the attribute, and an unexpected case falls
/// back to the general frame-size rule rather than to a wrong one.
fn is_ybr_full_422(photometric: Option<&[u8]>) -> bool {
    let Some(raw) = photometric else {
        return false;
    };
    let end = raw
        .iter()
        .rposition(|b| !b.is_ascii_whitespace() && *b != 0)
        .map_or(0, |i| i + 1);
    &raw[..end] == b"YBR_FULL_422"
}

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
        // PS3.5 8.1 packs bits CONTINUOUSLY across the Value Field: no per-row
        // byte boundary, and multi-frame native values are concatenated with
        // no per-frame padding (standard/ps3.5/bit-packed-pixel-data-8.1.json,
        // bitPackedContinuity + multiFrameConcatenation). A row-padded reading
        // (rows * ceil(columns/8)) shifted every row after the first, and on a
        // 100x100 frame put frame 2 fifty bytes late.
        let bits = rows as usize * columns as usize * samples_per_pixel as usize;
        return Some(bits.div_ceil(8));
    }
    let bytes_per_sample = (bits_allocated as usize).div_ceil(8);
    if bytes_per_sample == 0 {
        return None;
    }
    Some(rows as usize * columns as usize * samples_per_pixel as usize * bytes_per_sample)
}

/// Parse a DICOM integer value from raw bytes, using the element's VR.
///
/// PS3.5 6.2 Table 6.2-1 fixes the representation per VR: US and SS are a
/// 16-bit binary word, UL and SL a 32-bit one, IS and DS a character string.
/// Without the VR the two are indistinguishable by length alone - Rows = 49
/// is the bytes `31 00`, which an ASCII-first reading turns into the text "1"
/// and reports 1 row. That is exactly the wrong answer for a geometry tag, so
/// the caller that knows the VR states it.
pub fn parse_dicom_int_vr(data: &[u8], vr: &[u8], big_endian: bool) -> Option<u32> {
    if data.is_empty() {
        return None;
    }
    let read16 = |b: [u8; 2]| {
        if big_endian {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        }
    };
    let read32 = |b: [u8; 4]| {
        if big_endian {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        }
    };
    match vr {
        b"US" => {
            if data.len() < 2 {
                return None;
            }
            Some(u32::from(read16([data[0], data[1]])))
        }
        b"SS" => {
            if data.len() < 2 {
                return None;
            }
            let value = if big_endian {
                i16::from_be_bytes([data[0], data[1]])
            } else {
                i16::from_le_bytes([data[0], data[1]])
            };
            // A negative frame count or geometry is malformed rather than a
            // value to reinterpret, so it is refused instead of wrapped.
            u32::try_from(value).ok()
        }
        b"UL" => {
            if data.len() < 4 {
                return None;
            }
            Some(read32([data[0], data[1], data[2], data[3]]))
        }
        b"SL" => {
            if data.len() < 4 {
                return None;
            }
            let value = if big_endian {
                i32::from_be_bytes([data[0], data[1], data[2], data[3]])
            } else {
                i32::from_le_bytes([data[0], data[1], data[2], data[3]])
            };
            u32::try_from(value).ok()
        }
        // IS and DS are character strings (PS3.5 6.2): a leading or trailing
        // space is padding, and a value that does not parse is not zero.
        b"IS" | b"DS" => {
            let text = std::str::from_utf8(data).ok()?;
            text.trim_end_matches(['\0', ' '])
                .trim_start_matches(' ')
                .parse::<u32>()
                .ok()
        }
        _ => None,
    }
}

/// Parse a DICOM integer value from raw bytes without the VR.
///
/// Kept for callers that only have the value; it cannot tell a 2-byte binary
/// word from a 2-character string, so [`parse_dicom_int_vr`] is what a header
/// walker should use.
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

    /// Explicit VR Little Endian element with a 16-bit length.
    fn short_form(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
        buf.extend_from_slice(&group.to_le_bytes());
        buf.extend_from_slice(&element.to_le_bytes());
        buf.extend_from_slice(vr);
        buf.extend_from_slice(&(value.len() as u16).to_le_bytes());
        buf.extend_from_slice(value);
        if !value.len().is_multiple_of(2) {
            buf.push(0x00);
        }
    }

    /// `NumberOfFrames` must be rewritten even when an earlier element is an
    /// undefined-length sequence.
    ///
    /// PS3.5 7.5.1 orders elements by tag, not by length form, and a WSI puts
    /// the per-frame functional groups (5200,9230) - undefined length - ahead
    /// of PixelData, so a defined-length NumberOfFrames may well sit after one.
    /// The rewrite used to stop dead at the first undefined-length element,
    /// leaving the spliced single-frame header still declaring the original
    /// frame count: a consumer trusting it reads past the end of the value.
    #[test]
    fn number_of_frames_is_rewritten_after_an_undefined_length_sequence() {
        let mut buf = Vec::new();
        // The splice head this rewrites always carries File Meta Information,
        // and TransferSyntaxUID is what tells the walk the dataset is Explicit
        // VR. Without it the walk reads the dataset as Implicit and every
        // length comes out wrong.
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        // UI is SHORT form (PS3.5 6.2 Table 6.2-1), so the meta entry uses the
        // 2-byte length. Writing it long-form puts four length bytes where the
        // walk expects a 16-bit field and the UID is never found.
        short_form(&mut buf, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1\0");
        // An undefined-length sequence before NumberOfFrames.
        buf.extend_from_slice(&0x5200u16.to_le_bytes());
        buf.extend_from_slice(&0x9230u16.to_le_bytes());
        buf.extend_from_slice(b"SQ");
        buf.extend_from_slice(&[0u8, 0u8]);
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // One empty item, then the sequence delimiter.
        buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
        buf.extend_from_slice(&0u32.to_le_bytes());

        // NumberOfFrames comes AFTER the above; PS3.5 7.5.1 orders by tag, and
        // 5200,9230 sorts after 0028,0008 - so state it out of order
        // deliberately to prove the walk follows the stream, not the sort.
        short_form(&mut buf, 0x0028, 0x0008, b"IS", b"400 ");
        short_form(&mut buf, 0x0028, 0x0010, b"US", &[0x40, 0x00]);

        rewrite_number_of_frames(&mut buf, 1);

        // Locate the element by walking the same way the rewrite does, then
        // compare its VALUE bytes: the field keeps its original width, so the
        // new text is space-padded (PS3.5 6.2) and "400 " becomes "1   ".
        let start = pixel_data_scan_start(&buf);
        let enc = dataset_encoding(&buf, start);
        let mut pos = start;
        let mut found = None;
        while let Some((hdr_len, len, _)) = element_layout(&buf, pos, enc) {
            if (
                read_u16_enc(&buf, pos, enc.big_endian),
                read_u16_enc(&buf, pos + 2, enc.big_endian),
            ) == (Some(TAG_NUMBER_OF_FRAMES.0), Some(TAG_NUMBER_OF_FRAMES.1))
            {
                found = Some(buf[pos + hdr_len..pos + hdr_len + len].to_vec());
                break;
            }
            if len == UNDEFINED_LENGTH {
                match undefined_length_end(&buf, pos + hdr_len, enc) {
                    Some(next) if next > pos => pos = next,
                    _ => break,
                }
                continue;
            }
            match pos.checked_add(hdr_len).and_then(|p| p.checked_add(len)) {
                Some(next) if next > pos => pos = next,
                _ => break,
            }
        }
        assert_eq!(
            found.as_deref(),
            Some(b"1   ".as_slice()),
            "NumberOfFrames must be rewritten to 1 even after an \
             undefined-length sequence, and keep the original field width"
        );
    }

    /// The 0xFFFE item header is Implicit VR, so it stays little-endian even
    /// when the enclosing data set is Big Endian.
    ///
    /// PS3.5 7.5: Item (FFFE,E000), Item Delimitation (FFFE,E00D) and Sequence
    /// Delimitation (FFFE,E0DD) are "not ruled by the VR encoding rules
    /// conveyed by the Transfer Syntax" and "shall be encoded as Implicit VR";
    /// 7.3 defines Implicit VR as Little Endian. Reading the tag in the data
    /// set's order contradicted the length read beside it, so a Big Endian
    /// undefined-length value could never be walked to its delimiter.
    #[test]
    fn an_item_header_is_little_endian_even_in_a_big_endian_dataset() {
        // An empty item followed by a sequence delimiter, all little-endian -
        // which is the only conformant spelling whatever the data set uses.
        let mut value = Vec::new();
        value.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        value.extend_from_slice(&0u32.to_le_bytes());
        value.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
        value.extend_from_slice(&0u32.to_le_bytes());

        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "clarus-frame-be-item-{}-{:?}.bin",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, &value).expect("write fixture");
        let mut file = File::open(&path).expect("open fixture");
        let be = DatasetEncoding {
            explicit_vr: true,
            big_endian: true,
        };
        // The BOUNDED walk is where the byte order is observable: it has no
        // element-header fallback, so a misread item tag leaves it reading the
        // delimiter's length word as a tag and it never terminates.
        let bounded =
            stream_undefined_end_bounded(&mut file, 0, be, value.len()).expect("delimiter found");
        let unbounded = stream_undefined_end(&mut file, 0, be);
        drop(file);
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            bounded,
            value.len() as u64,
            "a Big Endian data set still has a little-endian item header"
        );
        assert_eq!(unbounded, Some(value.len() as u64));
    }

    /// A skipped value that runs past EOF is a read failure, not an absent tag.
    ///
    /// `checked_add` catches integer overflow only. A value whose declared
    /// length merely exceeds the bytes that follow computes a valid offset
    /// beyond the end; the walk then exits on its own bound and reports
    /// `Ok(None)`, which a caller reads as "this instance has no such tag" -
    /// a truncated blob presented as a valid negative answer.
    #[test]
    fn a_skipped_value_past_eof_is_an_error_not_an_absent_tag() {
        // Header claims a 4096-byte value; the file ends immediately after it.
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]); // delimiter-ish tag
        buf.extend_from_slice(&4096u32.to_le_bytes());
        buf.extend_from_slice(&[0u8; 4]);

        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "clarus-frame-past-eof-{}-{:?}.bin",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, &buf).expect("write fixture");
        let result = find_tag_value_in_file_checked(&path, (0x0010, 0x0010), 1024);
        let _ = std::fs::remove_file(&path);

        match result {
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
            Ok(v) => panic!("a value past EOF must not report absence, got {v:?}"),
        }
    }

    /// With an EMPTY Basic Offset Table, more items than declared frames means
    /// a frame spans fragments - so item i is not frame i and no table may be
    /// published.
    ///
    /// PS3.5 A.4: the table is the authoritative frame->fragment map, and a
    /// frame may own several fragments. An empty table says nothing about
    /// boundaries, so one-item-per-frame is only valid when the counts agree.
    /// Serving item i as frame i while a frame spans two fragments hands out
    /// half a codestream as a whole frame and shifts every later frame, with a
    /// row count that still looks plausible.
    #[test]
    fn an_empty_offset_table_with_more_items_than_frames_publishes_no_table() {
        // 2 declared frames, 4 stored items: each frame is two fragments.
        let pixel = make_encapsulated(&[b"AAAA", b"BBBB", b"CCCC", b"DDDD"]);
        let chunks =
            chunks_encapsulated(&pixel, 0, 2, MAX_FRAMES_CEILING, false).expect("no cap error");
        assert!(
            chunks.is_empty(),
            "4 items over 2 frames means a frame spans fragments, so a \
             byte-range table would be wrong, got {chunks:?}"
        );
        // Fewer items than declared stays servable: those items are whole
        // frames and the table is merely short.
        let chunks =
            chunks_encapsulated(&pixel, 0, 8, MAX_FRAMES_CEILING, false).expect("no cap error");
        assert_eq!(chunks.len(), 4, "over-declaration keeps the actual items");
    }

    /// A Basic Offset Table entry that does not address an Item tag is refused.
    ///
    /// PS3.5 A.4: an offset counts bytes from the first Item TAG to the first
    /// Item TAG of its frame, so the byte it addresses is an Item tag by
    /// definition. This case is ALREADY caught by the walk-equality check above
    /// it (an entry that is not where the walk found that frame's item cannot
    /// equal the walk's offset), so the assertion below pins existing correct
    /// behaviour rather than a change made in this pass. It is kept because the
    /// rule should be tested where it is stated, not only as a side effect of
    /// an arithmetic coincidence.
    #[test]
    fn an_offset_table_entry_that_is_not_an_item_tag_is_refused() {
        let pixel = make_encapsulated(&[b"AAAA", b"BBBB"]);
        // One bogus entry pointing into the middle of frame 1's payload.
        let mut bot = Vec::new();
        bot.extend_from_slice(&0u32.to_le_bytes());
        bot.extend_from_slice(&9u32.to_le_bytes());
        let mut rebuilt = Vec::new();
        rebuilt.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        rebuilt.extend_from_slice(&(bot.len() as u32).to_le_bytes());
        rebuilt.extend_from_slice(&bot);
        rebuilt.extend_from_slice(&pixel[8..]);

        let chunks =
            chunks_encapsulated(&rebuilt, 0, 2, MAX_FRAMES_CEILING, false).expect("no cap error");
        assert!(
            chunks.is_empty(),
            "an entry that does not address an Item tag is not a frame \
             boundary, got {chunks:?}"
        );
    }

    /// A well-formed table is still accepted, so the two guards above are not
    /// simply refusing every table.
    #[test]
    fn a_conforming_offset_table_still_produces_a_table() {
        let pixel = make_encapsulated_with_bot(&[b"AAAA", b"BBBB"]);
        let chunks =
            chunks_encapsulated(&pixel, 0, 2, MAX_FRAMES_CEILING, false).expect("no cap error");
        assert_eq!(chunks.len(), 2, "a valid table must still be served");
        assert_eq!(chunks[0].len, 4);
        assert_eq!(chunks[1].len, 4);
    }

    /// A native PixelData whose first samples spell an Item tag is still native.
    ///
    /// PS3.5 Table A.4-1 makes the encapsulated form undefined-length, so the
    /// ELEMENT HEADER decides the encoding. Sniffing the value's leading bytes
    /// instead misclassified any native image whose first samples are
    /// FE FF 00 E0 - which a 2-pixel 8-bit RGB frame reaches exactly
    /// (R=254 G=255 B=0 E=224) - and then walked the sample data as an item
    /// stream, so the instance got no chunk table at all.
    #[test]
    fn a_native_value_that_starts_with_an_item_tag_is_not_encapsulated() {
        // 2x1 RGB, 8 bits: 6 bytes per frame. Frame 1 is chosen to begin with
        // the four bytes of (FFFE,E000).
        let frame1 = [0xFEu8, 0xFF, 0x00, 0xE0, 0x11, 0x22];
        let frame2 = [0x33u8, 0x44, 0x55, 0x66, 0x77, 0x88];
        let mut pixel = Vec::new();
        pixel.extend_from_slice(&frame1);
        pixel.extend_from_slice(&frame2);

        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(1)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0002), b"US", &us(3)),
            ((0x0028, 0x0100), b"US", &us(8)),
            ((0x0028, 0x0008), b"IS", b"2 "),
            ((0x0028, 0x0004), b"CS", b"RGB "),
            // DEFINED length: the header says native, whatever the bytes say.
            ((0x7FE0, 0x0010), b"OB", &pixel),
        ]);

        let chunks = compute_frame_chunks(&file);
        assert_eq!(
            chunks.len(),
            2,
            "the header says native, so both frames must be tabled, got {chunks:?}"
        );
        assert_eq!(chunks[0].len, 6);
        assert_eq!(chunks[1].len, 6);
        // Frame 2 must start exactly one frame in, not at some item boundary.
        assert_eq!(chunks[1].offset, chunks[0].offset + 6);
    }

    /// A slice-path header is decoded with the dataset's RESOLVED byte order.
    ///
    /// PS3.5 7.1: the byte order is a property of TransferSyntaxUID, the same
    /// element that decides the VR form. Reading a Big Endian dataset's tags
    /// little-endian finds different tags (0010,0028 rather than 0028,0010) and
    /// different lengths, so the walk reports nothing or the wrong geometry -
    /// and the values it does read are plausible enough that nothing complains.
    #[test]
    fn geometry_is_decoded_in_the_resolved_byte_order() {
        // Explicit VR Big Endian (1.2.840.10008.1.2.2): every tag, length and
        // binary value is big-endian.
        fn be_element(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], val: &[u8]) {
            buf.extend_from_slice(&group.to_be_bytes());
            buf.extend_from_slice(&element.to_be_bytes());
            buf.extend_from_slice(vr);
            // US/IS are short form; OB/OW/UN/SQ take reserved + 4-byte length.
            if matches!(vr, b"OB" | b"OW" | b"UN" | b"SQ") {
                buf.extend_from_slice(&[0u8, 0u8]);
                buf.extend_from_slice(&(val.len() as u32).to_be_bytes());
            } else {
                buf.extend_from_slice(&(val.len() as u16).to_be_bytes());
            }
            buf.extend_from_slice(val);
            if !val.len().is_multiple_of(2) {
                buf.push(0x00);
            }
        }

        let mut buf = Vec::new();
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        // TransferSyntaxUID is short-form in the meta group and ALWAYS explicit
        // VR little-endian (PS3.10 7.1).
        buf.extend_from_slice(&0x0002u16.to_le_bytes());
        buf.extend_from_slice(&0x0010u16.to_le_bytes());
        buf.extend_from_slice(b"UI");
        let uid = b"1.2.840.10008.1.2.2\0";
        buf.extend_from_slice(&(uid.len() as u16).to_le_bytes());
        buf.extend_from_slice(uid);
        // 2x2 8-bit monochrome, 2 frames = 4 bytes each = 8 bytes of pixels.
        be_element(&mut buf, 0x0028, 0x0010, b"US", &2u16.to_be_bytes());
        be_element(&mut buf, 0x0028, 0x0011, b"US", &2u16.to_be_bytes());
        be_element(&mut buf, 0x0028, 0x0002, b"US", &1u16.to_be_bytes());
        be_element(&mut buf, 0x0028, 0x0100, b"US", &8u16.to_be_bytes());
        be_element(&mut buf, 0x0028, 0x0008, b"IS", b"2 ");
        be_element(&mut buf, 0x7FE0, 0x0010, b"OB", &[1, 2, 3, 4, 5, 6, 7, 8]);

        let start = pixel_data_scan_start(&buf);
        let enc = dataset_encoding(&buf, start);
        assert!(enc.explicit_vr, "1.2.840.10008.1.2.2 is Explicit VR");
        assert!(enc.big_endian, "1.2.840.10008.1.2.2 is Big Endian");

        // End to end: the table must hold both frames at the right offsets.
        // A little-endian reading of these big-endian headers finds other tags
        // and other lengths, so the walk reaches PixelData never or walks past
        // it - and the answer is an empty or mis-sized table either way.
        let chunks = compute_frame_chunks(&buf);
        assert_eq!(
            chunks.len(),
            2,
            "Big Endian geometry must be read and both frames tabled, got {chunks:?}"
        );
        assert_eq!(chunks[0].len, 4);
        assert_eq!(chunks[1].offset, chunks[0].offset + 4);
    }

    /// An Implicit VR geometry value is decoded by the DICTIONARY's VR.
    ///
    /// Implicit VR carries no VR on the wire (PS3.5 7.1/6.2), so the VR is a
    /// property of the tag and PS3.6 is the only statement of it. Falling back
    /// to a shape-blind reader tried ASCII first: Rows = 49 is the two bytes
    /// `31 00`, which reads as the text "1" - so a 49-row image was sized from
    /// one row and every frame after the first came from the wrong offset.
    #[test]
    fn an_implicit_geometry_value_uses_the_dictionary_vr() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        // Implicit VR Little Endian (1.2.840.10008.1.2).
        buf.extend_from_slice(&0x0002u16.to_le_bytes());
        buf.extend_from_slice(&0x0010u16.to_le_bytes());
        buf.extend_from_slice(b"UI");
        let uid = b"1.2.840.10008.1.2\0";
        buf.extend_from_slice(&(uid.len() as u16).to_le_bytes());
        buf.extend_from_slice(uid);
        // Rows (0028,0010) as US = 49, implicit: tag + 4-byte length + value,
        // no VR bytes at all.
        buf.extend_from_slice(&0x0028u16.to_le_bytes());
        buf.extend_from_slice(&0x0010u16.to_le_bytes());
        buf.extend_from_slice(&2u32.to_le_bytes());
        buf.extend_from_slice(&49u16.to_le_bytes());

        assert_eq!(
            find_tag_u32(&buf, TAG_ROWS),
            Some(49),
            "the dictionary says (0028,0010) is US, so 0x31 0x00 is 49 - not \
             the ASCII text \"1\""
        );
    }

    /// The declared-frame cap is enforced independently of the geometry.
    ///
    /// The declared check used to sit AFTER the Rows/Columns lookups, so an
    /// instance that declared more frames than the cap allows but was also
    /// missing geometry returned `Ok(vec![])` - "keep the whole-blob path" -
    /// and was accepted. The declaration is the fact the caller must act on,
    /// and it is readable without any geometry at all.
    #[test]
    fn the_declared_cap_is_enforced_without_geometry() {
        // Declares 500 frames, cap is 8, and states no Rows/Columns at all.
        let file = make_file(&[((0x0028, 0x0008), b"IS", b"500 ")]);
        assert_eq!(
            find_tag_u32(&file, TAG_ROWS),
            None,
            "the fixture must lack Rows"
        );
        match compute_frame_chunks_capped(&file, 8) {
            Err(FrameTableError::TooManyFrames {
                declared: 500,
                cap: 8,
                ..
            }) => {}
            other => panic!(
                "an over-declaration must be reported even with no geometry, \
                 got {other:?}"
            ),
        }
    }

    /// The FILE API must return the BOT/walked cap error, not swallow it.
    ///
    /// The streaming walk folds the cap signal into `ScanOutcome::TooMany`, and
    /// `compute_frame_chunks_from_file_capped` is the only thing that turns it
    /// back into `Err`. On the single-frame path the call used to be written
    /// `let _ = chunks_encapsulated_stream(...)`, inside an
    /// `else if declared_single` that could never be reached while
    /// `encapsulated` was true - so the walk never ran at all and a crafted
    /// single-frame declaration over thousands of items was accepted silently.
    /// The rows are now dropped only AFTER the walk enforces the cap.
    #[test]
    fn the_file_api_returns_the_walked_cap_error() {
        let empty: Vec<&[u8]> = vec![&[]; 20];
        let pixel = make_encapsulated(&empty);
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0100), b"US", &us(8)),
            // No NumberOfFrames element at all: the instance DECLARES one frame.
            ((0x7FE0, 0x0010), b"OB", &pixel),
        ]);
        let (_dir, path) = write_temp(&file);

        match compute_frame_chunks_from_file_capped(&path, 8) {
            Err(FrameTableError::TooManyFrames { cap: 8, .. }) => {}
            other => panic!(
                "20 stored items over a cap of 8 must be reported as an error, \
                 got {other:?}"
            ),
        }
        // And the uncapped wrapper still yields the historical empty table.
        assert!(compute_frame_chunks_from_file(&path).is_empty());
    }

    /// A truncated blob is a READ failure, not an instance with no frames.
    ///
    /// `compute_frame_chunks_from_file_capped` returns `Ok(vec![])` to mean
    /// "keep the whole-blob serve path", and the caller writes a manifest row
    /// on that basis. A file that opened but whose bytes do not arrive must not
    /// borrow that meaning: the row would claim the instance has no frames to
    /// serve when in fact nobody read it. The failure is `Err(Io)` instead.
    #[test]
    fn a_truncated_blob_is_an_io_error_not_an_empty_table() {
        // A valid group-0002 header whose TransferSyntaxUID value is cut off:
        // the meta walk reads the length, then fails to read that many bytes.
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        buf.extend_from_slice(&0x0002u16.to_le_bytes());
        buf.extend_from_slice(&0x0010u16.to_le_bytes());
        buf.extend_from_slice(b"UI");
        buf.extend_from_slice(&64u16.to_le_bytes()); // claims 64 bytes
        buf.extend_from_slice(b"1.2.840"); // ...but supplies 8
        let (_dir, path) = write_temp(&buf);

        match compute_frame_chunks_from_file_capped(&path, 1024) {
            Err(FrameTableError::Io(_)) => {}
            other => panic!(
                "a short read must surface as an IO error, not as \"no table\", \
                 got {other:?}"
            ),
        }
    }

    /// An odd-sized native frame is strided by its own length, not rounded up.
    ///
    /// PS3.5 8.1: "the individual Frames are not padded. The individual Frames
    /// shall be concatenated and padding bits (if necessary) applied to the
    /// COMPLETE Value Field" - so only the value as a whole is made even. An
    /// even stride put frame 2 one byte late for every odd frame size, and the
    /// resulting table looked plausible because every offset stayed in range.
    #[test]
    fn an_odd_native_frame_size_is_not_rounded_up_to_the_stride() {
        // Three 3-byte frames = 9 bytes, plus the single value pad byte.
        let pixel = [0xAAu8, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22, 0x33, 0x00];
        let chunks = chunks_native(0, pixel.len(), 3, 3, None);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].offset, 0);
        assert_eq!(chunks[1].offset, 3, "frame 2 starts one 3-byte frame in");
        assert_eq!(chunks[2].offset, 6);
        assert_eq!(chunks[0].len, 3);
    }

    /// D2: both halves of "what is one frame's bytes", in one place.
    ///
    /// (a) Chroma subsampling. PS3.3 C.7.6.3: for YBR_FULL_422 "two chrominance
    /// samples are shared between four luminance samples", and SamplesPerPixel
    /// (3) is the NOMINAL channel count, not the stored sample count. Sizing a
    /// 422 frame by SamplesPerPixel over-states it by 50%, so every frame after
    /// the first is read from the middle of its neighbour.
    ///
    /// (b) Mid-byte frames. PS3.5 8.1 packs samples continuously and concatenates
    /// frames with no per-frame padding, so when the frame's bit length is not a
    /// multiple of 8 the boundary falls inside a byte. No byte range can name
    /// such a frame: it necessarily contains bits of its neighbour, and nothing
    /// in the chunk table lets a reader detect that.
    ///
    /// They are one test because they answer one question. A table that is
    /// wrong in either way hands out byte ranges that decode to the wrong
    /// image, so neither half may land alone.
    #[test]
    fn a_frames_byte_range_accounts_for_subsampling_and_mid_byte_starts() {
        // (a) 4x2 YBR_FULL_422, 8 bits: the luminance plane is 8 samples, so
        // the frame is 8*2 = 16 bytes, NOT 8*3 = 24.
        assert_eq!(
            frame_size_for(2, 4, 3, 8, Some(b"YBR_FULL_422")),
            Some(16),
            "YBR_FULL_422 stores two samples per pixel pair (PS3.3 C.7.6.3)"
        );
        // The same geometry in RGB does store three samples per pixel.
        assert_eq!(frame_size_for(2, 4, 3, 8, Some(b"RGB")), Some(24));
        // Absent PI falls back to the general rule, not to 422.
        assert_eq!(frame_size_for(2, 4, 3, 8, None), Some(24));

        // The stride difference is what corrupts later frames: with 24-byte
        // frames over a 422 value, frame 2 starts 8 bytes past its real start.
        let mut value = [0u8; 16 * 3 + 4];
        value[0] = 0xEE;
        let chunks = chunks_native(0, value.len(), 16, 3, None);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[1].offset, 16, "frame 2 starts one 16-byte frame in");

        // (b) 3x3 at 1 bit = 9 bits per frame, which is not a byte multiple.
        assert_eq!(mid_byte_frame_bits(3, 3, 1, 1), Some(9));
        // ...so no byte-range table may be published for it.
        let sub_byte = [0u8; 64];
        let chunks = chunks_native(0, sub_byte.len(), 2, 8, mid_byte_frame_bits(3, 3, 1, 1));
        assert!(
            chunks.is_empty(),
            "a mid-byte frame has no byte range; publishing one would mix two \
             frames, got {chunks:?}"
        );
        // A byte-aligned frame of the same kind still tables normally.
        let aligned = [0u8; 64];
        let chunks = chunks_native(0, aligned.len(), 2, 8, mid_byte_frame_bits(4, 4, 1, 1));
        assert_eq!(chunks.len(), 8, "8x 1-bit frames ARE byte-aligned");
    }

    /// The shared VR-width classification must treat `SV` and `UV` as long form.
    ///
    /// PS3.5 6.2 Table 6.2-1 lists them among the VRs with reserved bytes and
    /// a 32-bit Value Length. A local allow-list written before they existed
    /// sent them down the short branch, where the two reserved bytes were read
    /// as a 16-bit length and the walk desynchronised by two bytes.
    #[test]
    fn sv_and_uv_use_the_long_form_header() {
        assert!(is_extended_vr_bytes(*b"SV"), "SV is long form (PS3.5 6.2)");
        assert!(is_extended_vr_bytes(*b"UV"), "UV is long form (PS3.5 6.2)");
        // Controls: the short-form VRs must stay short.
        assert!(!is_extended_vr_bytes(*b"US"));
        assert!(!is_extended_vr_bytes(*b"IS"));
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
        assert_eq!(result.to_vec(), b"\xAA\xBB\xCC".to_vec());
    }

    /// The filled-table control: a BOT that AGREES with the items must give
    /// the same answer as the empty-table path.
    #[test]
    fn a_filled_offset_table_agreeing_with_the_items_is_used() {
        let enc = make_encapsulated_with_bot(&[b"\xAA\xBB\xCC", b"\xDD\xEE\xFF"]);
        assert_eq!(
            extract_frame(&enc, 1, 0, 2).unwrap().to_vec(),
            b"\xAA\xBB\xCC".to_vec()
        );
        assert_eq!(
            extract_frame(&enc, 2, 0, 2).unwrap().to_vec(),
            b"\xDD\xEE\xFF".to_vec()
        );
    }

    /// PS3.5 A.4: a frame may span several fragments, and a byte-range view
    /// cannot express one (the item headers sit between the fragments), so an
    /// instance whose table says so is left to the whole-blob path. What must
    /// not happen is the old answer: item 1 served as if it were frame 1, i.e.
    /// a truncated codestream presented as a whole one.
    ///
    /// The assertion is stronger than "not equal to fragment 1": the answer
    /// must not be a PREFIX of the concatenated fragments either, because a
    /// truncated codestream is exactly such a prefix.
    #[test]
    fn a_frame_spanning_two_fragments_is_not_reported_as_one_fragment() {
        let (first, second) = (b"\x01\x02\x03".as_slice(), b"\x04\x05\x06".as_slice());
        let third = b"\xAA\xBB";
        // BOT: frame 1 at 0, frame 2 at the THIRD item tag.
        let bot_at_2 = (8 + first.len() + 8 + second.len()) as u32;
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        buf.extend_from_slice(&8u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&bot_at_2.to_le_bytes());
        for frag in [first, second, third] {
            buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
            buf.extend_from_slice(&(frag.len() as u32).to_le_bytes());
            buf.extend_from_slice(frag);
        }
        buf.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0]);

        let mut whole: Vec<u8> = first.to_vec();
        whole.extend_from_slice(second);
        // Either the whole frame or a refusal; a truncated codestream is
        // neither, and it is exactly what item-per-frame produced.
        if let Ok(frame) = extract_frame(&buf, 1, 0, 3) {
            assert_eq!(frame, whole, "when answered, the whole frame");
        }
    }

    /// A frame that spans two fragments must be ANSWERED, not refused:
    /// extraction returns an owned buffer, so the item headers between the
    /// pieces are encapsulation and can be dropped.
    ///
    /// PS3.5 A.4 (standard/ps3.5/encapsulated-pixel-data-A.4.json) allows a
    /// frame to be split across fragments. Refusing the frame made every such
    /// instance unreadable through `/frames/{N}`, even though the Basic Offset
    /// Table that describes it is complete and in agreement with the items.
    #[test]
    fn a_frame_spanning_two_fragments_is_assembled() {
        let (first, second) = (b"\x01\x02\x03".as_slice(), b"\x04\x05\x06".as_slice());
        let third = b"\xAA\xBB";
        // BOT: frame 1 at 0, frame 2 at the THIRD item tag.
        let bot_at_2 = (8 + first.len() + 8 + second.len()) as u32;
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        buf.extend_from_slice(&8u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&bot_at_2.to_le_bytes());
        for frag in [first, second, third] {
            buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
            buf.extend_from_slice(&(frag.len() as u32).to_le_bytes());
            buf.extend_from_slice(frag);
        }
        buf.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0]);

        let frame = extract_frame(&buf, 1, 0, 2).expect("frame 1 spans two fragments");
        assert_eq!(
            frame,
            b"\x01\x02\x03\x04\x05\x06".to_vec(),
            "the fragments are joined without their item headers"
        );
        // Frame 2 is the third fragment alone.
        let frame2 = extract_frame(&buf, 2, 0, 2).expect("frame 2");
        assert_eq!(frame2, third.to_vec());
    }

    /// A Basic Offset Table whose first entry is not zero describes fragments
    /// before frame one, so it must not be used.
    ///
    /// PS3.5 A.4 (standard/ps3.5/encapsulated-pixel-data-A.4.json) counts every
    /// offset from the first byte of the first Item TAG, so entry 0 is the
    /// distance from that tag to itself: zero. A table starting elsewhere can
    /// still be strictly increasing and still land on fragment boundaries, so
    /// ordering alone accepts it and shifts every frame by one fragment.
    #[test]
    fn an_offset_table_that_does_not_start_at_zero_is_ignored() {
        let enc = make_encapsulated_with_bot(&[b"AAAA", b"BBBB"]);
        // Rewrite entry 0 to name the SECOND fragment.
        let mut shifted = enc.clone();
        shifted[8..12].copy_from_slice(&12u32.to_le_bytes());
        // The second offset must stay above the first to keep the table
        // "increasing" - that is exactly the shape ordering alone misses.
        shifted[12..16].copy_from_slice(&14u32.to_le_bytes());

        // Frame 1 must still be the FIRST fragment, not the second.
        assert_eq!(
            extract_frame(&shifted, 1, 0, 2).expect("frame 1"),
            b"AAAA".to_vec(),
            "a table that does not start at zero is not a frame map"
        );
    }

    /// A table that does not agree with the items is ignored, and the items
    /// remain the only reading (one item per frame).
    #[test]
    fn a_disagreeing_offset_table_falls_back_to_item_per_frame() {
        let mut enc = make_encapsulated_with_bot(&[b"\xAA\xBB", b"\xCC\xDD"]);
        // Corrupt the FIRST offset (byte 8 is the table's start: BOT header is
        // 8 bytes) so it names nothing.
        enc[8..12].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(
            extract_frame(&enc, 2, 0, 2).unwrap().to_vec(),
            b"\xCC\xDD".to_vec()
        );
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
        // PS3.5 8.1: the bits of one frame are packed continuously, with no
        // per-row alignment. 776x776 -> ceil(776*776/8) = 75272 (the 20.09 SEG
        // shape - unchanged, because 776 is a multiple of 8).
        let size = compute_frame_size(776, 776, 1, 1).unwrap();
        assert_eq!(size, 75_272);
        // 100x100 -> ceil(10000/8) = 1250. A row-padded reading gives
        // 100 * ceil(100/8) = 1300 and shifts frame 2 by 50 bytes.
        let size = compute_frame_size(100, 100, 1, 1).unwrap();
        assert_eq!(size, 1_250);
        // An odd product still rounds UP: 3x3 -> ceil(9/8) = 2.
        assert_eq!(compute_frame_size(3, 3, 1, 1), Some(2));
    }

    /// A 1-bit frame split from a two-frame value starts at its BIT offset,
    /// not at the next byte boundary of a row.
    ///
    /// 100 columns is not a whole number of bytes, so a row-padded stride
    /// leaves frame 2 fifty bytes late and the last 50 bytes of frame 2 hang
    /// off the end of the value.
    #[test]
    fn a_one_bit_second_frame_starts_at_its_bit_offset() {
        // 100 columns is not a whole number of bytes, so a row-padded stride
        // (100 * 13 = 1300) would leave frame 2 fifty bytes late.
        let frame_size = compute_frame_size(100, 100, 1, 1).unwrap();
        assert_eq!(frame_size, 1_250);
        let mut pixel_data = vec![0xAAu8; frame_size];
        let second = vec![0x55u8; frame_size];
        pixel_data.extend_from_slice(&second);

        let extracted = extract_frame(&pixel_data, 2, frame_size, 2).expect("frame 2");
        assert_eq!(extracted, second, "frame 2 must be the second 1250 bytes");
    }

    /// A frame that does not start on a byte boundary is repacked from its OWN
    /// first bit, not served as a byte range.
    ///
    /// PS3.5 8.1 (standard/ps3.5/bit-packed-pixel-data-8.1.json,
    /// bitPackedContinuity + multiFrameConcatenation): a native Value Field is
    /// one continuous bit stream with no per-frame padding, so frame 2 of two
    /// 20-bit frames begins at BIT 20 - byte 2, bit 4. A byte stride would
    /// serve bits 32..52 instead, which is the wrong twelve bits.
    ///
    /// The fixture is therefore a REAL bit stream, not two byte-aligned frames:
    /// a padded fixture makes a byte stride accidentally correct and the test
    /// could never fail.
    #[test]
    fn a_frame_that_starts_mid_byte_is_served_from_its_own_bit_offset() {
        // Two 20-bit frames packed LSB-first within each byte, which is the
        // order PS3.5 8.1 requires: "the first bit ... is encoded in the least
        // significant bit, immediately followed by the next bit ... in the next
        // most significant bit"
        // (standard/ps3.5/bit-packed-pixel-data-8.1.json, bitPackedContinuity).
        const F1: u32 = 0x1_2345;
        const F2: u32 = 0xE_DCBA;
        const BITS: usize = 20;
        let total = BITS * 2;
        let mut pixel_data = vec![0u8; total.div_ceil(8)];
        for (i, v) in [(0usize, F1), (BITS, F2)] {
            for bit in 0..BITS {
                if v >> bit & 1 == 1 {
                    let at = i + bit;
                    pixel_data[at / 8] |= 1u8 << (at % 8);
                }
            }
        }
        assert_eq!(pixel_data.len(), 5);

        let frame2 = extract_native_frame_bits(&pixel_data, 2, BITS, 2).expect("frame 2");
        // The 20 bits are repacked from bit 0 of the output in the same
        // LSB-first order, so frame 2's samples come back unchanged.
        let mut expected = vec![0u8; BITS.div_ceil(8)];
        for bit in 0..BITS {
            if F2 >> bit & 1 == 1 {
                expected[bit / 8] |= 1u8 << (bit % 8);
            }
        }
        assert_eq!(
            frame2, expected,
            "frame 2's twenty bits start at bit 20, not at byte 4"
        );
        // The byte stride the indexers used would have served this instead.
        assert_ne!(
            frame2,
            pixel_data[2..].to_vec(),
            "a byte-stride answer is the wrong bits"
        );

        // Frame 1 is byte-aligned at bit 0 and must keep working. Its 20 bits
        // are NOT the first three bytes: byte 2 also carries the low bits of
        // frame 2's first samples, so the trailing 4 bits must not survive the
        // repack. Taking the raw prefix would compare frame 1 against a
        // mixture of two frames - the very confusion this function exists to
        // prevent - so the expected value is built from F1's own bits.
        let frame1 = extract_native_frame_bits(&pixel_data, 1, BITS, 2).expect("frame 1");
        let mut expected1 = vec![0u8; BITS.div_ceil(8)];
        for bit in 0..BITS {
            if F1 >> bit & 1 == 1 {
                expected1[bit / 8] |= 1u8 << (bit % 8);
            }
        }
        assert_eq!(frame1, expected1);
        assert_eq!(
            frame1[2], 0x01,
            "frame 1's last byte may keep only its own 4 bits, not frame 2's"
        );
    }

    #[test]
    fn test_extract_frame_1bit_packed_second() {
        // Two 1-bit frames of 2 rows x 10 columns = 20 bits = 3 bytes each.
        //
        // PS3.5 8.1: "the individual Frames are not padded. The individual
        // Frames shall be concatenated and padding bits (if necessary) applied
        // to the complete Value Field"
        // (standard/ps3.5/pixel-data-padding.json, framesNotIndividuallyPadded).
        // So the value is 6 bytes - the two frames back to back - with no pad
        // byte between them.
        let rows = 2u16;
        let cols = 10u16;
        let frame_size = compute_frame_size(rows, cols, 1, 1).unwrap();
        assert_eq!(frame_size, 3);
        let mut pixel_data = Vec::new();
        pixel_data.extend_from_slice(&[0b1010_1010, 0b1111_0000, 0b1010_1010]);
        pixel_data.extend_from_slice(&[0b0101_0101, 0b0000_1111, 0b0101_0101]);
        let second = extract_frame(&pixel_data, 2, frame_size, 2).unwrap();
        assert_eq!(second, vec![0b0101_0101, 0b0000_1111, 0b0101_0101]);
    }

    /// PS3.5 8.1: frames of a native multi-frame image are concatenated with
    /// NO per-frame padding, so the stride is exactly `frame_size`.
    ///
    /// Two 3-byte frames occupy bytes 0..3 and 3..6. The old stride rounded
    /// each frame up to 4, so frame 2 was read from byte 4 - off by one - and
    /// the second frame came back as `TruncatedData` for a 6-byte value.
    #[test]
    fn native_frames_are_concatenated_without_padding() {
        let frame_size = 3usize;
        let pixel_data = vec![
            0xAA, 0xBB, 0xCC, // frame 1
            0x11, 0x22, 0x33, // frame 2, immediately after
        ];
        let second = extract_frame(&pixel_data, 2, frame_size, 2).unwrap();
        assert_eq!(
            second,
            vec![0x11, 0x22, 0x33],
            "frame 2 starts at byte 3, not at byte 4"
        );
    }

    /// A native frame whose first samples LOOK like an Item tag stays native.
    ///
    /// PS3.5 A.4: encapsulation is a property of the Element encoding, not of
    /// the bytes inside the value. When the caller says "native", pixel samples
    /// equal to `FE FF 00 E0` must be returned as samples.
    #[test]
    fn a_native_frame_that_looks_like_an_item_tag_is_still_native() {
        let frame_size = 4usize;
        let pixel_data = vec![0xFE, 0xFF, 0x00, 0xE0];
        let frame =
            extract_frame_with_encoding(&pixel_data, 1, frame_size, 1, PixelDataEncoding::Native)
                .unwrap();
        assert_eq!(frame, pixel_data, "the samples are returned unchanged");
    }

    /// And the encoded path still unwraps a real encapsulated value.
    #[test]
    fn an_encapsulated_value_is_unwrapped_when_the_caller_says_so() {
        // Basic Offset Table item (empty), then one fragment holding 3 bytes.
        let mut value = Vec::new();
        value.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]); // (FFFE,E000)
        value.extend_from_slice(&0u32.to_le_bytes()); // empty BOT
        value.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]); // fragment item
        value.extend_from_slice(&3u32.to_le_bytes());
        value.extend_from_slice(&[0x11, 0x22, 0x33]);
        let frame =
            extract_frame_with_encoding(&value, 1, 3, 1, PixelDataEncoding::Encapsulated).unwrap();
        assert_eq!(frame, vec![0x11, 0x22, 0x33]);
    }

    /// 200 nested undefined-length elements must be refused, not walked.
    ///
    /// Each level costs 8 bytes of the pattern below, so without the depth
    /// bound the walk recurses once per level - 200 frames do not fit a
    /// 64 KiB stack, and under `panic=abort` the overflow aborts the process,
    /// which a crafted STOW reaches through `find_tag_range`. The refusal is
    /// `None`: no frame table, so the caller keeps the whole-blob path.
    #[test]
    fn a_deeply_nested_undefined_sequence_is_refused_not_walked() {
        // (0008,1115) SQ, undefined length, then one undefined-length item per
        // level, each holding another (0008,1115). 200 levels, no delimiters.
        let mut data = Vec::new();
        for _ in 0..200 {
            data.extend_from_slice(&0x0008u16.to_le_bytes());
            data.extend_from_slice(&0x1115u16.to_le_bytes());
            data.extend_from_slice(b"SQ");
            data.extend_from_slice(&[0x00, 0x00]);
            data.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            data.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
            data.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        }
        // The walk starts INSIDE the explicit sequence VALUE, not at the first
        // element: `undefined_length_end` measures from the value's first byte,
        // which for this fixture is the first item tag at offset 12. Starting
        // anywhere else measures a byte range that is not the sequence's value
        // and the depth it reports would not be the depth of this nesting.
        let sequence_value_start = 12;
        assert_eq!(
            &data[sequence_value_start..sequence_value_start + 4],
            &[0xFE, 0xFF, 0x00, 0xE0],
            "the walk must begin at the first Item tag of the sequence value"
        );
        // A 64 KiB stack is the point of the test: it makes one frame per
        // level impossible to survive, so the bound is what the answer
        // depends on.
        let handle = std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(move || undefined_length_end(&data, sequence_value_start, DatasetEncoding::BARE))
            .expect("spawn");
        let answer = handle.join().expect("the walk must return");
        assert_eq!(answer, None, "an over-deep walk is refused");
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
        // PS3.5 7.1: the dataset encoding comes from TransferSyntaxUID, and a
        // dataset with no File Meta Information is Implicit VR LE (PS3.5 A.1).
        // The elements below are Explicit VR, so the fixture must declare that
        // - otherwise it is not a Part 10 file and the walker correctly reads
        // each VR as half a length.
        push_elem(&mut file, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
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

    /// Prefix a fixture with the File Meta Information that declares its
    /// dataset encoding.
    ///
    /// PS3.5 7.1: TransferSyntaxUID is the only place a dataset's encoding is
    /// stated, and the walker reads it there instead of guessing from the
    /// element bytes. A fixture that is Explicit VR must say so, or it is not
    /// a file the walker can be asked to read.
    fn with_explicit_vr_meta(ts: &str, dataset: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(dataset.len() + 32);
        out.extend_from_slice(&[0u8; 128]);
        out.extend_from_slice(b"DICM");
        // PS3.5 6.2: a UI value is padded with NUL to even length, and the pad
        // is not part of the declared length.
        let pad = if ts.len().is_multiple_of(2) { 0 } else { 1 };
        out.extend_from_slice(&0x0002u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"UI");
        out.extend_from_slice(&(ts.len() as u16).to_le_bytes());
        out.extend_from_slice(ts.as_bytes());
        out.extend(std::iter::repeat_n(0, pad));
        out.extend_from_slice(dataset);
        out
    }

    /// Build a minimal explicit-VR-little-endian DICOM file:
    /// preamble + "DICM" + the File Meta Information that declares the
    /// encoding + the given (tag, VR, value) elements in order.
    fn make_file(elements: &[TestElement<'_>]) -> Vec<u8> {
        let mut buf = Vec::new();
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
        with_explicit_vr_meta("1.2.840.10008.1.2.1", &buf)
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

    /// A single-frame declaration must not silence the walked-item cap.
    ///
    /// The single-frame fallback returns an empty table by design (the
    /// instance is served whole), but the item stream is still walked by
    /// nobody else. Returning empty BEFORE the walk let a crafted file declare
    /// one frame, hold thousands of items, and reach STOW with no signal at
    /// all - the exact amplification the cap exists to bound.
    #[test]
    fn a_single_frame_declaration_still_trips_the_walked_item_cap() {
        let empty: Vec<&[u8]> = vec![&[]; 20];
        let pixel = make_encapsulated(&empty);
        // `make_encapsulated` closes with a delimiter; that is fine - the walk
        // reaches the cap long before it.
        assert!(pixel.len() > 8);
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0100), b"US", &us(8)),
            ((0x7FE0, 0x0010), b"OB", &pixel),
        ]);
        assert_eq!(
            find_tag_u32(&file, TAG_NUMBER_OF_FRAMES),
            None,
            "the fixture must DECLARE a single frame"
        );
        assert!(
            matches!(
                compute_frame_chunks_capped(&file, 8),
                Err(FrameTableError::TooManyFrames { cap: 8, .. })
            ),
            "20 stored items over a cap of 8 must be reported even when one frame is declared"
        );
    }

    /// A native value holding fewer than `num_frames * frame_size` bytes has a
    /// truncated LAST frame. Publishing it as a shorter chunk hands the serve
    /// path bytes that `extract_frame` refuses for the same frame number.
    #[test]
    fn a_truncated_trailing_native_frame_is_not_advertised() {
        // 2x2 8-bit = 4 bytes/frame, three frames declared, only six bytes.
        let short: Vec<u8> = (0u8..6).collect();
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0100), b"US", &us(8)),
            ((0x0028, 0x0008), b"IS", b"3 "),
            ((0x7FE0, 0x0010), b"OB", &short),
        ]);
        assert!(
            compute_frame_chunks(&file).is_empty(),
            "frame 3 cannot be two bytes; the table is dropped, not shortened"
        );
        let (_dir, path) = write_temp(&file);
        assert!(
            compute_frame_chunks_from_file(&path).is_empty(),
            "the file variant must agree"
        );

        // The extraction path refuses the same frame, which is the point.
        let pd = find_tag_range(&file, TAG_PIXEL_DATA).expect("PixelData");
        assert!(extract_frame(&file[pd.0..pd.0 + pd.1], 3, 4, 3).is_err());
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
        // The file API promises no partial table, so the slice variant must
        // agree: a fragment whose payload runs past the value is a truncated
        // instance, not a table with a shorter last frame. Asserting only "no
        // chunk points past EOF" let an implementation that returned the first
        // fragment and dropped the truncated one pass.
        assert!(
            compute_frame_chunks(&file).is_empty(),
            "a truncated encapsulation must discard the WHOLE table"
        );
        let (_dir, path) = write_temp(&file);
        assert!(
            compute_frame_chunks_from_file(&path).is_empty(),
            "the file variant must discard the whole table too"
        );
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

    /// Append a non-empty Basic Offset Table (PS3.5 A.4) to an item stream.
    ///
    /// Each offset is the byte distance from the first byte of the first item
    /// tag after the table to the first byte of that frame's item tag, which
    /// is why the table has to be built from the fragment lengths.
    fn make_encapsulated_with_bot(frames: &[&[u8]]) -> Vec<u8> {
        let mut buf = Vec::new();
        let bot_len = (frames.len() * 4) as u32;
        buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]); // (FFFE,E000)
        buf.extend_from_slice(&bot_len.to_le_bytes());
        let mut offset: u32 = 0;
        for frame_data in frames {
            buf.extend_from_slice(&offset.to_le_bytes());
            // Each item is an 8-byte header plus its data.
            offset += 8 + frame_data.len() as u32;
        }
        for frame_data in frames {
            buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
            buf.extend_from_slice(&(frame_data.len() as u32).to_le_bytes());
            buf.extend_from_slice(frame_data);
        }
        buf.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        buf
    }

    /// A non-empty Basic Offset Table must be validated against the right
    /// origin.
    ///
    /// PS3.5 A.4 counts each offset from the first byte of the first item TAG
    /// after the table. Measuring from the table's own tag added the 8-byte
    /// item header to every expected value, so a perfectly valid table
    /// disagreed with the walk and the WHOLE table was discarded - every frame
    /// then fell back to serving the entire blob.
    ///
    /// Falsification anchor: the offsets below are computed exactly as the
    /// standard states, so only the wrong origin makes them mismatch.
    #[test]
    fn a_non_empty_bot_offsets_from_the_first_item_tag() {
        let frames: [&[u8]; 3] = [b"AAAA", b"BBBBBB", b"CC"];
        let pixel = make_encapsulated_with_bot(&frames);
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(1)),
            ((0x0028, 0x0011), b"US", &us(1)),
            ((0x0028, 0x0008), b"IS", b"3 "),
            ((0x7FE0, 0x0010), b"OB", &pixel),
        ]);

        let chunks = compute_frame_chunks(&file);
        assert_eq!(chunks.len(), 3, "a valid BOT yields one chunk per frame");
        assert_eq!(chunks[0].len, 4);
        assert_eq!(chunks[1].len, 6);
        assert_eq!(chunks[2].len, 2);
        // Both variants agree, which is what the serve path relies on.
        assert_file_matches_bytes(&file);
    }

    /// The same table is REJECTED when an offset really does disagree.
    ///
    /// Without this half the positive test above would also pass on an
    /// implementation that simply ignores the table.
    #[test]
    fn a_disagreeing_bot_discards_the_table() {
        let frames: [&[u8]; 2] = [b"AAAA", b"BBBB"];
        let mut pixel = make_encapsulated_with_bot(&frames);
        // Corrupt the first offset: it is the first 4 bytes of the table body.
        pixel[8] = 0xFF;
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(1)),
            ((0x0028, 0x0011), b"US", &us(1)),
            ((0x0028, 0x0008), b"IS", b"2 "),
            ((0x7FE0, 0x0010), b"OB", &pixel),
        ]);
        assert!(
            compute_frame_chunks(&file).is_empty(),
            "a table that contradicts the walk is not served"
        );
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
        // PS3.5 7.1: the dataset encoding is stated in TransferSyntaxUID,
        // and the walker reads it there rather than guessing from the
        // element bytes.
        push_elem(&mut file, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
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
        // PS3.5 7.1: the dataset encoding is stated in TransferSyntaxUID,
        // and the walker reads it there rather than guessing from the
        // element bytes.
        push_elem(&mut file, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
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
        assert!(
            compute_frame_chunks_from_file(&p2).is_empty(),
            "file variant must discard the whole table, not keep the whole frames"
        );

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
    /// A value that ends right after a whole fragment, with no Sequence
    /// Delimitation Item, is TRUNCATED - not a complete table.
    ///
    /// PS3.5 Table A.4-1 (standard/ps3.5/undefined-length-and-delimiters.json):
    /// an undefined-length value is closed by the delimiter, so its absence
    /// means the instance promises a value that is not there. Every fragment
    /// in hand being whole does not make the value whole.
    #[test]
    fn an_undefined_length_value_without_its_delimiter_is_empty() {
        let mut pixel = Vec::new();
        pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
        for tile in [b"AAAA".as_slice(), b"BBBB".as_slice()] {
            pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
            pixel.extend_from_slice(&(tile.len() as u32).to_le_bytes());
            pixel.extend_from_slice(tile);
        }
        // NO delimiter: the buffer ends exactly after the last whole fragment.

        let mut file = vec![0u8; 128];
        file.extend_from_slice(b"DICM");
        push_elem(&mut file, 0x0028, 0x0010, b"US", &us(2));
        push_elem(&mut file, 0x0028, 0x0011, b"US", &us(2));
        push_elem(&mut file, 0x0028, 0x0008, b"IS", b"2 ");
        push_elem_undefined(&mut file, 0x7FE0, 0x0010, b"OB", &pixel);

        assert!(
            compute_frame_chunks(&file).is_empty(),
            "a missing delimiter is a truncation, so no table is published"
        );
        let (_dir, path) = write_temp(&file);
        assert!(
            compute_frame_chunks_from_file(&path).is_empty(),
            "the file variant agrees"
        );
    }

    /// A Sequence Delimitation Item carrying a non-zero length is not one.
    ///
    /// PS3.5 7.5 gives the delimiter an empty Value Field; a length there means
    /// the walk has been reading something other than the item stream, so every
    /// row above it is unfounded.
    #[test]
    fn a_delimiter_with_a_nonzero_length_discards_the_table() {
        let mut pixel = make_encapsulated(&[b"AAAA", b"BBBB"]);
        // Last 8 bytes are the delimiter; give it length 4.
        let n = pixel.len();
        pixel[n - 4..].copy_from_slice(&4u32.to_le_bytes());

        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0008), b"IS", b"2 "),
            ((0x7FE0, 0x0010), b"OB", &pixel),
        ]);
        assert!(
            compute_frame_chunks(&file).is_empty(),
            "a delimiter with a length is not a delimiter"
        );
        let (_dir, path) = write_temp(&file);
        assert!(compute_frame_chunks_from_file(&path).is_empty());
    }

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
        let answer = stream_undefined_end(&mut f, 132, DatasetEncoding::BARE);
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
        //
        // The emptiness assertion alone would also pass if the walk never
        // reached PixelData at all, which is the failure the test is supposed
        // to rule out - so the fixture first proves the walk DOES arrive: the
        // geometry is readable and PixelData is located as undefined-length.
        let mut pixel = Vec::new();
        pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
        // One item that claims 999999 bytes but has only 4.
        pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        pixel.extend_from_slice(&999_999u32.to_le_bytes());
        pixel.extend_from_slice(b"junk");

        let mut file = vec![0u8; 128];
        file.extend_from_slice(b"DICM");
        // File Meta Information naming Explicit VR Little Endian. Without it the
        // dataset is read as Implicit VR (PS3.5 A.1) and none of the explicit
        // elements below parse - which is exactly the trap this test's earlier
        // form fell into: the walk never reached PixelData and "empty table"
        // was satisfied vacuously.
        push_elem(&mut file, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1\0");
        push_elem(&mut file, 0x0028, 0x0010, b"US", &us(2));
        push_elem(&mut file, 0x0028, 0x0011, b"US", &us(2));
        push_elem(&mut file, 0x0028, 0x0008, b"IS", b"5 ");
        push_elem_undefined(&mut file, 0x7FE0, 0x0010, b"OB", &pixel);

        // The walk must actually arrive at PixelData, or the emptiness below
        // proves nothing: geometry decodes and the element is seen as
        // undefined-length at a real offset.
        assert_eq!(find_tag_u32(&file, TAG_ROWS), Some(2), "Rows must decode");
        assert_eq!(
            find_tag_u32(&file, TAG_COLUMNS),
            Some(2),
            "Columns must decode"
        );
        let (pd_offset, _len, pd_undefined) =
            find_tag_range_with_mode(&file, TAG_PIXEL_DATA).expect("PixelData must be found");
        assert!(
            pd_undefined,
            "the fixture must present PixelData with undefined length"
        );
        assert!(
            pd_offset > 0 && pd_offset < file.len(),
            "PixelData must be located inside the file, not at an edge"
        );

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

    /// The spliced PixelData length must COUNT the pad byte.
    ///
    /// PS3.5 7.1.1 (standard/ps3.5/string-value-padding.json,
    /// evenLengthValueFields): a Value Field has an even length, and the pad
    /// byte is PART of it - so a three-byte native frame is declared as four
    /// bytes, not three. Declaring three and writing four left the parity of
    /// the declared length and the bytes on disk disagreeing, which any
    /// consumer that seeks past the value reads as a corrupt dataset.
    #[test]
    fn a_spliced_odd_length_frame_declares_its_pad_byte() {
        let tiles: Vec<&[u8]> = vec![b"\xAA\xBB\xCC", b"\xDD\xEE\xFF"];
        let full = make_undefined_length_pixeldata_file(&tiles);
        let table = compute_frame_chunks(&full);
        assert_eq!(table.len(), 2);

        let pd_tag_pos = full
            .windows(4)
            .position(|w| w == [0xE0, 0x7F, 0x10, 0x00])
            .expect("PixelData tag present");
        let head = &full[..pd_tag_pos + 12];

        // The ENCAPSULATED branch: an odd-length frame is one Item whose
        // declared length must be the padded size.
        let f1 = table[0];
        let frame = &full[f1.offset as usize..(f1.offset + f1.len) as usize];
        assert_eq!(frame.len(), 3, "the fixture frame is genuinely odd");
        let spliced = splice_frame_into_head(head, frame).expect("splice ok");

        let (pd_off, _) = find_tag_range(&spliced, TAG_PIXEL_DATA).expect("PixelData");
        let value = &spliced[pd_off..];
        let item = &value[8..]; // past the empty Basic Offset Table item
        let declared = u32::from_le_bytes([item[4], item[5], item[6], item[7]]) as usize;
        assert_eq!(
            declared, 4,
            "the Item length must count the pad byte (PS3.5 7.1.1)"
        );
        assert_eq!(&item[8..11], frame, "the frame bytes are unchanged");
        assert_eq!(item[11], 0x00, "the pad byte itself is present");
        // And the whole value parses back as one frame carrying those bytes.
        let back = extract_frame(value, 1, 0, 1).expect("the spliced frame");
        assert_eq!(back, vec![frame[0], frame[1], frame[2], 0x00]);
    }

    /// A walked-cap trip must report the DECLARED frame count, not zero.
    ///
    /// The diagnostic exists so STOW can log honestly, and "declared=0" on an
    /// instance that declares two frames is a false statement about the file.
    #[test]
    fn a_walked_cap_trip_reports_the_declared_frame_count() {
        let empty: Vec<&[u8]> = vec![&[]; 20];
        // Declares 2 (within the cap of 8) but holds 20 items.
        let pixel = make_encapsulated(&empty);
        let file = make_file(&[
            ((0x0028, 0x0010), b"US", &us(2)),
            ((0x0028, 0x0011), b"US", &us(2)),
            ((0x0028, 0x0100), b"US", &us(8)),
            ((0x0028, 0x0008), b"IS", b"2 "),
            ((0x7FE0, 0x0010), b"OB", &pixel),
        ]);
        match compute_frame_chunks_capped(&file, 8) {
            Err(FrameTableError::TooManyFrames {
                declared,
                walked,
                cap,
            }) => {
                assert_eq!(declared, 2, "the instance declares two frames");
                assert_eq!(cap, 8);
                assert!(walked > 8, "the walk stopped where the cap said");
            }
            other => panic!("expected a cap trip, got {other:?}"),
        }

        let (_dir, path) = write_temp(&file);
        match compute_frame_chunks_from_file_capped(&path, 8) {
            Err(FrameTableError::TooManyFrames { declared, .. }) => {
                assert_eq!(declared, 2, "the file variant reports it too");
            }
            other => panic!("expected a cap trip, got {other:?}"),
        }
    }

    /// An unreadable blob is an ERROR, not "this instance needs no table".
    ///
    /// A missing file used to produce the same empty success as a genuinely
    /// single-frame instance, so the caller wrote a manifest row built from no
    /// bytes at all - the silent-success shape this walk exists to avoid.
    #[test]
    fn an_unreadable_blob_is_an_error_not_an_empty_table() {
        let missing = std::path::Path::new("Z:/definitely/not/here.dcm");
        match compute_frame_chunks_from_file_capped(missing, MAX_FRAMES_CEILING) {
            Err(FrameTableError::Io(msg)) => assert!(
                msg.contains("not/here.dcm"),
                "the diagnostic names the path, got: {msg}"
            ),
            other => panic!("a missing file must be an I/O error, got {other:?}"),
        }
        // The uncapped convenience wrapper keeps its historical shape.
        assert!(compute_frame_chunks_from_file(missing).is_empty());
    }

    /// The seek-based tag reader keeps a read failure distinct from absence.
    #[test]
    fn a_tag_read_failure_is_not_reported_as_an_absent_tag() {
        let missing = std::path::Path::new("Z:/definitely/not/here.dcm");
        assert!(
            find_tag_value_in_file_checked(missing, (0x0028, 0x0010), 1024).is_err(),
            "an unreadable file is an error"
        );
        // A readable file without the tag is Ok(None), which is the distinct
        // answer the checked entry point exists to give.
        let file = make_file(&[((0x0028, 0x0010), b"US", &us(2))]);
        let (_dir, path) = write_temp(&file);
        assert_eq!(
            find_tag_value_in_file_checked(&path, (0x0028, 0x1101), 1024).expect("readable"),
            None
        );
        assert!(
            find_tag_value_in_file_checked(&path, (0x0028, 0x0010), 1024)
                .expect("readable")
                .is_some()
        );
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

    /// The byte budget must bound the WALK, not only the allocation.
    ///
    /// A value of many tiny items costs one header read per item, so a walk
    /// that runs to the delimiter and only then compares lengths does unbounded
    /// work for a request that asked for a few hundred bytes. That has no
    /// effect on the ANSWER - both walks refuse - so the test makes the work
    /// itself observable: the file is cut off INSIDE the value, far past the
    /// budget. A walk that stops at the budget never reaches the cut and
    /// answers `Ok(None)`; a walk that runs on hits the cut and calls the file
    /// malformed. Only a bounded walk can tell those apart.
    #[test]
    fn a_budget_below_the_value_extent_stops_the_undefined_walk() {
        let mut value = Vec::new();
        for _ in 0..4000 {
            value.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
            value.extend_from_slice(&0u32.to_le_bytes());
        }
        value.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);

        let mut whole = vec![0u8; 128];
        whole.extend_from_slice(b"DICM");
        // PS3.5 7.1: the dataset encoding comes from TransferSyntaxUID, so the
        // fixture states it - an Explicit VR element read as Implicit VR hands
        // its VR bytes to the length field.
        push_elem(&mut whole, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
        push_elem(&mut whole, 0x0008, 0x0060, b"CS", b"XA");
        push_elem_undefined(&mut whole, 0x0028, 0x1101, b"OB", &value);

        // Whole file: the extent is returned when the budget covers it, and
        // one byte short is refused.
        let (_d0, p0) = write_temp(&whole);
        assert_eq!(
            find_tag_value_in_file(&p0, (0x0028, 0x1101), value.len())
                .expect("readable")
                .len(),
            value.len()
        );
        assert!(find_tag_value_in_file(&p0, (0x0028, 0x1101), value.len() - 1).is_none());

        // Truncate INSIDE the value, ~28 KB past a 1 KB budget.
        let mut cut = whole.clone();
        let keep = cut.len() - 4096;
        cut.truncate(keep);
        let (_d1, p1) = write_temp(&cut);

        assert!(
            find_tag_value_in_file_checked(&p1, (0x0028, 0x1101), 1024).is_ok(),
            "a walk bounded by 1024 bytes never reaches a cut 28 KB later, so the file is not malformed from where it stopped"
        );
        assert!(
            find_tag_value_in_file_checked(&p1, (0x0028, 0x1101), value.len()).is_err(),
            "a walk covering the whole extent does reach the cut and must report it"
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
        // PS3.5 7.1: the dataset encoding is stated in TransferSyntaxUID, and
        // the walker reads it there rather than guessing from the bytes.
        push_elem(&mut file, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
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

    /// Build an Explicit VR Little Endian instance with a given transfer
    /// syntax and native Pixel Data of `frames` frames.
    fn build_explicit_instance(ts: &str, rows: u16, columns: u16, frames: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 128]);
        out.extend_from_slice(b"DICM");
        // (0002,0010) TransferSyntaxUID - PS3.5 7.1: the meta group is always
        // Explicit VR Little Endian.
        out.extend_from_slice(&0x0002u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"UI");
        let len = if ts.len().is_multiple_of(2) {
            ts.len()
        } else {
            ts.len() + 1
        };
        out.extend_from_slice(&(len as u16).to_le_bytes());
        out.extend_from_slice(ts.as_bytes());
        if !ts.len().is_multiple_of(2) {
            out.push(0);
        }
        let us = |out: &mut Vec<u8>, tag: (u16, u16), value: u16| {
            out.extend_from_slice(&tag.0.to_le_bytes());
            out.extend_from_slice(&tag.1.to_le_bytes());
            out.extend_from_slice(b"US");
            out.extend_from_slice(&2u16.to_le_bytes());
            out.extend_from_slice(&value.to_le_bytes());
        };
        us(&mut out, TAG_ROWS, rows);
        us(&mut out, TAG_COLUMNS, columns);
        us(&mut out, TAG_SAMPLES_PER_PIXEL, 1);
        us(&mut out, TAG_BITS_ALLOCATED, 8);
        if frames > 1 {
            let text = frames.to_string();
            out.extend_from_slice(&TAG_NUMBER_OF_FRAMES.0.to_le_bytes());
            out.extend_from_slice(&TAG_NUMBER_OF_FRAMES.1.to_le_bytes());
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
        // (7FE0,0010) OW, defined length, native samples.
        let frame_bytes = rows as usize * columns as usize;
        out.extend_from_slice(&TAG_PIXEL_DATA.0.to_le_bytes());
        out.extend_from_slice(&TAG_PIXEL_DATA.1.to_le_bytes());
        out.extend_from_slice(b"OW");
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&((frame_bytes * frames as usize) as u32).to_le_bytes());
        for i in 0..frame_bytes * frames as usize {
            out.push((i % 251) as u8);
        }
        out
    }

    /// PS3.5 7.1: TransferSyntaxUID names the dataset encoding. A dataset
    /// whose element lengths happen to spell a VR name must not be read as
    /// Explicit VR, and a big-endian dataset must not have its tags read
    /// little-endian. The chunk table has to agree with the slice path.
    #[test]
    fn the_encoding_comes_from_the_transfer_syntax() {
        // Rows 49 is `31 00`: the byte after the 2-byte length is 0x00, and a
        // look-ahead VR guess would be no VR pair - but the point here is that
        // the explicit form is read explicitly, not guessed.
        for ts in [
            "1.2.840.10008.1.2.1", // Explicit VR Little Endian
            "1.2.840.10008.1.2",   // Implicit VR Little Endian
        ] {
            let bytes = build_explicit_instance(ts, 8, 8, 3);
            let enc = DatasetEncoding::from_ts_uid(ts.as_bytes());
            assert_eq!(
                enc.explicit_vr,
                ts == "1.2.840.10008.1.2.1",
                "explicit VR is stated by the UID"
            );
            assert!(!enc.big_endian);
            if enc.explicit_vr {
                assert_file_matches_bytes(&bytes);
            }
        }

        // A big-endian UID is recognised as big-endian.
        let be = DatasetEncoding::from_ts_uid(b"1.2.840.10008.1.2.2");
        assert!(be.explicit_vr && be.big_endian);
    }

    /// Big-endian tags and lengths are read in the file's byte order.
    #[test]
    fn a_big_endian_header_is_read_big_endian() {
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 128]);
        out.extend_from_slice(b"DICM");
        // (0002,0010) UI, Explicit VR Little Endian meta (PS3.5 7.1).
        out.extend_from_slice(&0x0002u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"UI");
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(b"1.2.840.10008.1.2.2\0");
        // Dataset: Explicit VR Big Endian.
        out.extend_from_slice(&[0x00, 0x28, 0x00, 0x10]); // (0028,0010) BE
        out.extend_from_slice(b"US");
        out.extend_from_slice(&[0x00, 0x02]); // length 2, BE
        out.extend_from_slice(&[0x00, 0x10]); // value 16, BE

        let (_dir, path) = write_temp(&out);
        let mut file = File::open(&path).expect("open");
        let (start, enc) = stream_scan_start(&mut file).expect("dataset start");
        assert!(enc.big_endian, "the TS UID says big endian");
        let header = read_element_header(&mut file, start, enc).expect("header");
        assert_eq!((header.group, header.element), (0x0028, 0x0010));
        assert_eq!(header.length, 2);
        assert_eq!(header.vr, Some(*b"US"));
    }

    /// The streaming walk reads Rows through the VR.
    ///
    /// Rows = 49 is the two bytes `31 00`. Read as text that is "1", so the
    /// frame size is computed from ONE row and every frame offset after the
    /// first is wrong - while the table still has the right number of rows, so
    /// only the offsets betray it.
    #[test]
    fn a_us_rows_value_is_not_read_as_text_by_the_streaming_walk() {
        // Rows 49 is `31 00`, the two bytes an ASCII-first reader trims to
        // "1". 49 x 49 x 3 is odd, so use 50 columns to keep the native value
        // an even length (PS3.5 8.1.1: frames are concatenated).
        let rows: u16 = 49;
        let columns: u16 = 50;
        let frames: u32 = 3;
        let mut file = vec![0u8; 128];
        file.extend_from_slice(b"DICM");
        push_elem(&mut file, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
        push_elem(&mut file, 0x0028, 0x0010, b"US", &us(rows));
        push_elem(&mut file, 0x0028, 0x0011, b"US", &us(columns));
        push_elem(&mut file, 0x0028, 0x0100, b"US", &us(8));
        push_elem(&mut file, 0x0028, 0x0008, b"IS", b"3 ");
        let frame_size = rows as usize * columns as usize;
        let native: Vec<u8> = (0..frame_size * frames as usize)
            .map(|i| (i % 251) as u8)
            .collect();
        push_elem(&mut file, 0x7FE0, 0x0010, b"OW", &native);

        // BOTH scanners: the slice variant and the streaming variant read the
        // geometry through their own code, so a fix in one does not cover the
        // other.
        let chunks = compute_frame_chunks(&file);
        assert_eq!(chunks.len() as u32, frames, "one chunk per frame (slice)");
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(
                chunk.len, frame_size as u64,
                "frame {i} must be rows x columns, not one row (slice)"
            );
        }

        let (_dir, path) = write_temp(&file);
        let streamed = compute_frame_chunks_from_file(&path);
        assert_eq!(
            streamed.len() as u32,
            frames,
            "one chunk per frame (streaming)"
        );
        for (i, chunk) in streamed.iter().enumerate() {
            assert_eq!(
                chunk.len, frame_size as u64,
                "frame {i} must be rows x columns, not one row (streaming)"
            );
            assert_eq!(chunk.offset, chunks[i].offset, "frame {i} offset");
        }
    }

    /// PS3.5 6.2: US is a 16-bit binary word. Rows = 49 is `31 00`, which an
    /// ASCII-first reader trims to "1" - one row, and a frame size computed
    /// from it.
    #[test]
    fn a_two_byte_us_is_read_as_binary_not_as_text() {
        assert_eq!(parse_dicom_int_vr(&[49, 0], b"US", false), Some(49));
        assert_eq!(parse_dicom_int_vr(&[0, 49], b"US", true), Some(49));
        assert_eq!(parse_dicom_int(&[49, 0]), Some(1), "the shape-blind reader");
    }

    /// IS is a character string, so its digits are text and its padding is
    /// padding (PS3.5 6.2).
    #[test]
    fn an_is_value_is_read_as_text() {
        assert_eq!(parse_dicom_int_vr(b"3 ", b"IS", false), Some(3));
        assert_eq!(parse_dicom_int_vr(b"64\0", b"IS", false), Some(64));
        assert_eq!(parse_dicom_int_vr(b"xx", b"IS", false), None);
        assert_eq!(parse_dicom_int_vr(&[0x31], b"UL", false), None, "short UL");
    }
}
