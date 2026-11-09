// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! RLE Lossless codec - PackBits compression, pure Rust.
//!
//! Implements DICOM RLE Lossless (1.2.840.10008.1.2.5) per PS3.5 Section 8.2 and Section G.
//!
//! # Algorithm
//!
//! PackBits is a simple run-length encoding:
//! - n in [0, 127]: next n+1 bytes are literals (copy as-is)
//! - n in [-128, -1]: next byte repeated -n+1 times (RLE run)
//! - n = -128: no-op
//!
//! In DICOM RLE (PS3.5 Annex G), each FRAME is preceded by a fixed 64-byte
//! header: [u32 number_of_segments][u32 offset per segment][zero padding].
//! offset[0] = 64 (first byte after the header). Segments are ordered per
//! channel, most significant byte plane first (8-bit RGB: R, G, B).
//! Multi-frame data = per-frame [64-byte header + segments] concatenated.

#![forbid(unsafe_code)]

use super::{CodecError, DecodedPixels, DicomCodec, DicomMeta, ts_uid};
use super::{
    TAG_BITS_ALLOCATED, TAG_BITS_STORED, TAG_COLUMNS, TAG_NUMBER_OF_FRAMES,
    TAG_PHOTOMETRIC_INTERPRETATION, TAG_PIXEL_DATA, TAG_PIXEL_REPRESENTATION,
    TAG_PLANAR_CONFIGURATION, TAG_ROWS, TAG_SAMPLES_PER_PIXEL,
};

/// Codec for RLE Lossless (1.2.840.10008.1.2.5).
pub struct RleCodec;

impl DicomCodec for RleCodec {
    fn transfer_syntax_uid(&self) -> &'static str {
        ts_uid::RLE_LOSSLESS
    }

    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
        let meta = parse_rle_meta(dicom_bytes)?;
        let stream = extract_pixel_data(dicom_bytes)
            .ok_or_else(|| CodecError::DecodeFailed("RLE pixel data not found".into()))?;

        if stream.is_empty() {
            return Err(CodecError::DecodeFailed("RLE pixel data is empty".into()));
        }

        let frames = meta.frames.max(1) as usize;
        let spp = meta.samples_per_pixel as usize;
        let bytes_per_sample = (meta.bits_allocated as usize) >> 3;
        if bytes_per_sample == 0 {
            return Err(CodecError::InvalidMetadata(
                "RLE: BitsAllocated must be >= 8".into(),
            ));
        }
        // PS3.5 Annex G: one segment per (channel, byte plane); 15 max.
        let segments_per_frame = spp * bytes_per_sample;
        if segments_per_frame > 15 {
            return Err(CodecError::InvalidMetadata(format!(
                "RLE: {} segments per frame exceeds the DICOM limit of 15",
                segments_per_frame
            )));
        }

        let pixel_count = meta.rows as usize * meta.columns as usize;
        // A frame costs at least its 64-byte header in the stream, so a frame
        // count above `len/64` cannot be honest. Without this check a declared
        // NumberOfFrames of 4e9 asked for ~1 PB of output and the process was
        // killed by the allocator before the first frame was ever read.
        if frames > stream.len() / 64 {
            return Err(CodecError::InvalidMetadata(format!(
                "RLE: NumberOfFrames {} exceeds the {} frames the {} byte stream can hold",
                frames,
                stream.len() / 64,
                stream.len()
            )));
        }
        // Output layout (codec convention): per-plane, frame-major -
        // all frames of channel 0, then all frames of channel 1...
        let out_len = frames
            .checked_mul(spp)
            .and_then(|v| v.checked_mul(bytes_per_sample))
            .and_then(|v| v.checked_mul(pixel_count))
            .ok_or_else(|| {
                CodecError::InvalidMetadata("RLE: decoded size overflows usize".into())
            })?;
        let mut decoded = vec![0u8; out_len];

        // Walk the stream frame by frame: each frame starts with its own
        // 64-byte header. The last segment's byte-end is found by decoding
        // until `pixel_count` bytes are produced (PackBits is
        // output-count-delimited).
        let mut cur = 0usize;
        for frame in 0..frames {
            if cur + 64 > stream.len() {
                return Err(CodecError::DecodeFailed(format!(
                    "RLE frame {} header extends past input",
                    frame
                )));
            }
            let hdr = &stream[cur..cur + 64];
            let n_seg = u32_from_slice(&hdr[0..4]) as usize;
            if n_seg == 0 || n_seg > 15 {
                return Err(CodecError::DecodeFailed(format!(
                    "RLE frame {}: invalid segment count {}",
                    frame, n_seg
                )));
            }
            if n_seg != segments_per_frame {
                return Err(CodecError::DecodeFailed(format!(
                    "RLE frame {}: {} segments (expected {})",
                    frame, n_seg, segments_per_frame
                )));
            }
            let mut offsets = Vec::with_capacity(n_seg);
            for i in 0..n_seg {
                offsets.push(u32_from_slice(&hdr[4 + 4 * i..8 + 4 * i]) as usize);
            }
            // PS3.5 Annex G: offsets are absolute within the frame, first = 64.
            if offsets[0] != 64 {
                return Err(CodecError::DecodeFailed(format!(
                    "RLE frame {}: first segment offset {} != 64",
                    frame, offsets[0]
                )));
            }

            let mut frame_end = cur;
            for i in 0..n_seg {
                let seg_start = offsets[i];
                let abs_start = cur + seg_start;
                if abs_start >= stream.len() {
                    return Err(CodecError::DecodeFailed(format!(
                        "RLE frame {} segment {} start out of bounds",
                        frame, i
                    )));
                }
                let bounded = i + 1 < n_seg;
                let slice = if bounded {
                    let abs_end = cur + offsets[i + 1];
                    if abs_end <= abs_start || abs_end > stream.len() {
                        return Err(CodecError::DecodeFailed(format!(
                            "RLE frame {} segment {} invalid end offset",
                            frame, i
                        )));
                    }
                    &stream[abs_start..abs_end]
                } else {
                    &stream[abs_start..]
                };

                let (segment, consumed) = packbits_decode_consumed(slice, pixel_count)?;
                if !bounded {
                    frame_end = abs_start + consumed;
                }

                // Segment order per PS3.5 Annex G: per channel, MSB plane
                // first. Output is per-plane frame-major, byte planes LSB
                // first within each sample.
                let channel = i / bytes_per_sample;
                let plane_stream = i % bytes_per_sample; // 0 = MSB plane
                let plane_lsb = bytes_per_sample - 1 - plane_stream;
                let base = channel * frames * pixel_count * bytes_per_sample
                    + frame * pixel_count * bytes_per_sample
                    + plane_lsb;
                for (p, &b) in segment.iter().enumerate().take(pixel_count) {
                    decoded[base + p * bytes_per_sample] = b;
                }
            }
            cur = frame_end;
            // Real-world tolerance (field file, 01.09): some encoders emit a
            // dead trailing byte after the last segment (e.g. a literal-run
            // header with its data byte missing). Such bytes cannot begin a
            // valid next-frame header, whose first u32 is the segment count
            // (1..=15), so skipping 0x00 / 0x80 (no-op) is safe.
            if frame + 1 < frames {
                while cur < stream.len() && (stream[cur] == 0x00 || stream[cur] == 0x80) {
                    cur += 1;
                }
            }
        }

        Ok(DecodedPixels {
            data: decoded,
            rows: meta.rows,
            columns: meta.columns,
            samples_per_pixel: meta.samples_per_pixel,
            bits_allocated: meta.bits_allocated,
            photometric_interpretation: meta.photometric_interpretation,
        })
    }

    fn encode(&self, pixels: &DecodedPixels, meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
        let frame_blobs = encode_frames(pixels, meta)?;
        let total: usize = frame_blobs.iter().map(|f| f.len()).sum();
        let mut out = Vec::with_capacity(total);
        for blob in &frame_blobs {
            out.extend_from_slice(blob);
        }
        Ok(out)
    }

    fn encode_pixeldata(
        &self,
        pixels: &DecodedPixels,
        meta: &DicomMeta,
    ) -> Result<Vec<u8>, CodecError> {
        // One fragment per frame (PS3.5 A.4): every fragment carries its own
        // 64-byte RLE header + segments, so decoders can split frames.
        let frame_blobs = encode_frames(pixels, meta)?;
        Ok(crate::codec::build_encapsulated_pixeldata_multi(
            &frame_blobs,
            1,
            false,
        ))
    }
}

// ===========================================================================
// RLE frame encode (spec-correct, shared by encode/encode_pixeldata)
// ===========================================================================

/// Build one [64-byte header + segments] blob per frame.
///
/// Input layout (codec convention): per-plane, frame-major - all frames of
/// channel 0, then all frames of channel 1..., byte planes LSB first within
/// each sample. Segment order per PS3.5 Annex G: per channel, MSB plane
/// first, so 8-bit RGB emits R, G, B and 16-bit monochrome emits MSB, LSB.
fn encode_frames(pixels: &DecodedPixels, meta: &DicomMeta) -> Result<Vec<Vec<u8>>, CodecError> {
    let bytes_per_sample = (pixels.bits_allocated as usize) >> 3;
    if bytes_per_sample == 0 {
        return Err(CodecError::EncodeFailed(
            "BitsAllocated must be >= 8 for RLE encoding".into(),
        ));
    }

    let spp = pixels.samples_per_pixel as usize;
    let segments_per_frame = spp * bytes_per_sample;
    if segments_per_frame > 15 {
        return Err(CodecError::EncodeFailed(format!(
            "RLE: {} segments per frame exceeds the DICOM limit of 15",
            segments_per_frame
        )));
    }

    let frames = meta.frames.max(1) as usize;
    let pixel_count = pixels.pixel_count();
    let expected = frames * spp * bytes_per_sample * pixel_count;
    if pixels.data.len() != expected {
        return Err(CodecError::EncodeFailed(format!(
            "RLE: pixel data size {} != expected {}",
            pixels.data.len(),
            expected
        )));
    }

    let mut frame_blobs = Vec::with_capacity(frames);
    for frame in 0..frames {
        let mut segs: Vec<Vec<u8>> = Vec::with_capacity(segments_per_frame);
        for channel in 0..spp {
            for plane_stream in 0..bytes_per_sample {
                let plane_lsb = bytes_per_sample - 1 - plane_stream;
                let base = channel * frames * pixel_count * bytes_per_sample
                    + frame * pixel_count * bytes_per_sample
                    + plane_lsb;
                let mut segment = Vec::with_capacity(pixel_count);
                for p in 0..pixel_count {
                    segment.push(pixels.data[base + p * bytes_per_sample]);
                }
                segs.push(packbits_encode(&segment));
            }
        }

        // 64-byte header: [num_segments][offset per segment][zero padding].
        let mut header = vec![0u8; 64];
        header[0..4].copy_from_slice(&(segments_per_frame as u32).to_le_bytes());
        let mut offset = 64u32;
        for (i, seg) in segs.iter().enumerate() {
            header[4 + 4 * i..8 + 4 * i].copy_from_slice(&offset.to_le_bytes());
            offset = offset.saturating_add(seg.len() as u32);
        }

        let mut blob = Vec::with_capacity(64 + segs.iter().map(|s| s.len()).sum::<usize>());
        blob.extend_from_slice(&header);
        for seg in &segs {
            blob.extend_from_slice(seg);
        }
        frame_blobs.push(blob);
    }
    Ok(frame_blobs)
}

// ===========================================================================
// PackBits algorithm (pure Rust)
// ===========================================================================

/// Decode PackBits-compressed data (test convenience wrapper).
#[cfg(test)]
fn packbits_decode(data: &[u8], expected_len: usize) -> Result<Vec<u8>, CodecError> {
    packbits_decode_consumed(data, expected_len).map(|(out, _)| out)
}

/// Decode PackBits and report how many input bytes were consumed.
///
/// The consumed count is what lets multi-frame RLE parsing find the next
/// frame's 64-byte header after the last (unbounded) segment of a frame.
fn packbits_decode_consumed(
    data: &[u8],
    expected_len: usize,
) -> Result<(Vec<u8>, usize), CodecError> {
    let mut output = Vec::with_capacity(expected_len);
    let mut i = 0;

    while i < data.len() && output.len() < expected_len {
        let n = data[i] as i8;
        i += 1;

        if n == -128 {
            continue; // no-op
        } else if n >= 0 {
            // Literal run: copy n+1 bytes
            let count = n as usize + 1;
            if i + count > data.len() {
                return Err(CodecError::DecodeFailed(format!(
                    "PackBits literal run extends past input: need {count} bytes"
                )));
            }
            output.extend_from_slice(&data[i..i + count]);
            i += count;
        } else {
            // RLE run: repeat next byte -n+1 times
            let count = (-n) as usize + 1;
            if i >= data.len() {
                return Err(CodecError::DecodeFailed(
                    "PackBits RLE run missing repeat byte".into(),
                ));
            }
            let byte = data[i];
            i += 1;
            output.resize(output.len() + count, byte);
        }
    }

    if output.len() < expected_len {
        output.resize(expected_len, 0);
    } else if output.len() > expected_len {
        output.truncate(expected_len);
    }

    Ok((output, i))
}

/// Encode data using PackBits algorithm.
fn packbits_encode(data: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len());
    let mut i = 0;
    let len = data.len();

    while i < len {
        let mut run_len = 1;
        while i + run_len < len && data[i + run_len] == data[i] && run_len < 128 {
            run_len += 1;
        }

        if run_len >= 3 {
            // RLE run
            output.push(1u8.wrapping_sub(run_len as u8)); // = -(run_len-1) as u8
            output.push(data[i]);
            i += run_len;
        } else {
            // Literal run
            let literal_start = i;
            i += 1;
            while i < len {
                let mut future_run = 1;
                while i + future_run < len && data[i + future_run] == data[i] && future_run < 3 {
                    future_run += 1;
                }
                if future_run >= 3 {
                    break;
                }
                i += 1;
                if i - literal_start >= 128 {
                    break;
                }
            }
            let literal_count = i - literal_start;
            output.push(literal_count as u8 - 1);
            output.extend_from_slice(&data[literal_start..i]);
        }
    }

    output
}

// ===========================================================================
// DICOM tag parsing helpers (self-contained)
// ===========================================================================

fn parse_rle_meta(data: &[u8]) -> Result<DicomMeta, CodecError> {
    let rows = find_tag_u16(data, TAG_ROWS)
        .ok_or_else(|| CodecError::InvalidMetadata("Missing Rows (0028,0010)".into()))?;
    let columns = find_tag_u16(data, TAG_COLUMNS)
        .ok_or_else(|| CodecError::InvalidMetadata("Missing Columns (0028,0011)".into()))?;
    let samples_per_pixel: u8 = find_tag_u16(data, TAG_SAMPLES_PER_PIXEL)
        .map(|v| v as u8)
        .unwrap_or(1);
    let bits_allocated: u8 = find_tag_u16(data, TAG_BITS_ALLOCATED).unwrap_or(8) as u8;
    let bits_stored: u8 = find_tag_u16(data, TAG_BITS_STORED)
        .map(|v| v as u8)
        .unwrap_or(bits_allocated);
    let photometric_interpretation = find_tag_str(data, TAG_PHOTOMETRIC_INTERPRETATION)
        .unwrap_or_else(|| "MONOCHROME2".to_string());
    let planar_configuration = find_tag_u16(data, TAG_PLANAR_CONFIGURATION);
    // NumberOfFrames (0028,0008) is IS - a decimal string, not a binary
    // US value (field fact 01.09: "10" as bytes 0x31 0x30).
    let frames: u32 = find_tag_str(data, TAG_NUMBER_OF_FRAMES)
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1);

    Ok(DicomMeta {
        rows,
        columns,
        samples_per_pixel,
        bits_allocated,
        bits_stored,
        photometric_interpretation,
        planar_configuration,
        pixel_representation: find_tag_u16(data, TAG_PIXEL_REPRESENTATION).unwrap_or(0),
        frames,
    })
}

fn extract_pixel_data(data: &[u8]) -> Option<Vec<u8>> {
    find_tag_bytes(data, TAG_PIXEL_DATA)
}

fn find_tag_bytes(data: &[u8], target: (u16, u16)) -> Option<Vec<u8>> {
    let mut pos = dicom_start(data)?;
    // Skip meta-header
    while pos + 4 <= data.len() {
        if u16_le(data, pos) != 0x0002 {
            break;
        }
        pos += 4;
        if !skip_explicit(data, &mut pos) {
            return None;
        }
    }
    // Walk dataset
    while pos + 4 <= data.len() {
        let g = u16_le(data, pos);
        let e = u16_le(data, pos + 2);
        pos += 4;
        // Handle group FFFE: Item (FFFE,E000), Item Delimitation (FFFE,E00D),
        // Sequence Delimiter (FFFE,E0DD). These have no VR, just 4-byte length.
        if g == 0xFFFE {
            if pos + 4 > data.len() {
                return None;
            }
            let len = u32_from_slice(&data[pos..pos + 4]) as usize;
            pos += 4;
            if len == 0xFFFF_FFFF {
                // Undefined length - skip by scanning for next FFFE delimiter
                // (encapsulated pixel data uses undefined-length Items)
                continue;
            }
            if pos + len > data.len() {
                return None;
            }
            pos += len;
            continue;
        }
        if g == 0x0000 || g == 0x0004 {
            if !skip_implicit(data, &mut pos) {
                return None;
            }
            continue;
        }
        if (g, e) == target {
            return read_explicit_val(data, &mut pos);
        }
        if !skip_explicit(data, &mut pos) {
            return None;
        }
    }
    None
}

fn find_tag_u16(data: &[u8], tag: (u16, u16)) -> Option<u16> {
    let bytes = find_tag_bytes(data, tag)?;
    if bytes.len() >= 2 {
        Some(u16::from_le_bytes([bytes[0], bytes[1]]))
    } else if bytes.len() == 1 {
        Some(bytes[0] as u16)
    } else {
        None
    }
}

fn find_tag_str(data: &[u8], tag: (u16, u16)) -> Option<String> {
    let bytes = find_tag_bytes(data, tag)?;
    let s = String::from_utf8_lossy(&bytes);
    let t = s.trim_end_matches([' ', '\0']);
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn dicom_start(data: &[u8]) -> Option<usize> {
    if data.len() >= 132 && &data[128..132] == b"DICM" {
        Some(132)
    } else if data.len() >= 4 && &data[0..4] == b"DICM" {
        Some(4)
    } else {
        None
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

fn read_explicit_val(data: &[u8], pos: &mut usize) -> Option<Vec<u8>> {
    if *pos + 4 > data.len() {
        return None;
    }
    let vr = std::str::from_utf8(&data[*pos..*pos + 2]).ok()?;
    *pos += 2;
    let extended = matches!(
        vr,
        "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
    );
    let len: usize = if extended {
        if *pos + 6 > data.len() {
            return None;
        }
        *pos += 2;
        u32_from_slice(&data[*pos..*pos + 4]) as usize
    } else {
        u16_le(data, *pos) as usize
    };
    *pos += if extended { 4 } else { 2 };

    if len == 0xFFFF_FFFF {
        // Encapsulated (undefined-length) PixelData.
        // Collect the payloads of ALL data items (skipping the Basic Offset
        // Table) and concatenate them into one RLE stream.
        read_encapsulated_rle_stream(data, pos)
    } else {
        if *pos + len > data.len() {
            return None;
        }
        let v = data[*pos..*pos + len].to_vec();
        super::skip_value(pos, len);
        Some(v)
    }
}

/// Collect all RLE data item payloads from an encapsulated PixelData value.
///
/// Non-standard but real (field 01.09): a multi-frame RLE file whose pixel
/// value is item-wrapped - a Basic Offset Table item followed by one Item per
/// frame. Each Item payload is a [64-byte header + segments] frame blob, so
/// concatenating payloads yields the standard multi-frame RLE stream.
///
/// BOT rule (same as the JPEG path): an item whose payload is a multiple of
/// 4 and starts with a zero u32 is a Basic Offset Table, not RLE data.
fn read_encapsulated_rle_stream(data: &[u8], pos: &mut usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        if *pos + 8 > data.len() {
            break;
        }
        let tag = (u16_le(data, *pos), u16_le(data, *pos + 2));
        if tag == (0xFFFE, 0xE0DD) {
            // Sequence Delimiter - end of encapsulated value.
            *pos += 8;
            break;
        }
        if tag != (0xFFFE, 0xE000) {
            break; // unexpected tag -- stop
        }
        let len = u32_from_slice(&data[*pos + 4..*pos + 8]) as usize;
        *pos += 8;
        if len == 0xFFFF_FFFF {
            return None; // undefined-length item -- unsupported
        }
        if *pos + len > data.len() {
            return None;
        }
        let payload = &data[*pos..*pos + len];
        let is_bot =
            len.is_multiple_of(4) && payload.len() >= 4 && u32_from_slice(&payload[0..4]) == 0;
        if !is_bot {
            out.extend_from_slice(payload);
        }
        *pos += len;
    }
    if out.is_empty() { None } else { Some(out) }
}

fn skip_explicit(data: &[u8], pos: &mut usize) -> bool {
    if *pos + 4 > data.len() {
        return false;
    }
    let vr = match std::str::from_utf8(&data[*pos..*pos + 2]) {
        Ok(v) => v,
        Err(_) => return false,
    };
    *pos += 2;
    let extended = matches!(
        vr,
        "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
    );
    let len: usize = if extended {
        if *pos + 6 > data.len() {
            return false;
        }
        *pos += 2;
        u32_from_slice(&data[*pos..*pos + 4]) as usize
    } else {
        u16_le(data, *pos) as usize
    };
    *pos += if extended { 4 } else { 2 };
    if len == 0xFFFF_FFFF {
        // Sequence (SQ/UN) with undefined length: skip items up to the
        // matching Sequence Delimiter (see the same helper in
        // jpeg_baseline.rs; field fact 01.09 ZONARE US).
        return skip_undefined_sequence(data, pos);
    }
    if *pos + len > data.len() {
        return false;
    }
    super::skip_value(pos, len);
    true
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

fn skip_implicit(data: &[u8], pos: &mut usize) -> bool {
    if *pos + 4 > data.len() {
        return false;
    }
    let len = u32_from_slice(&data[*pos..*pos + 4]) as usize;
    *pos += 4;
    if *pos + len > data.len() {
        return false;
    }
    super::skip_value(pos, len);
    true
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packbits_literal_only() {
        let input = vec![1, 2, 3, 4, 5];
        let encoded = packbits_encode(&input);
        let decoded = packbits_decode(&encoded, input.len()).unwrap();
        assert_eq!(decoded, input);
    }

    #[test]
    fn test_packbits_long_run() {
        let input = vec![42u8; 50];
        let encoded = packbits_encode(&input);
        let decoded = packbits_decode(&encoded, input.len()).unwrap();
        assert_eq!(decoded, input);
        assert!(encoded.len() < input.len());
    }

    #[test]
    fn test_packbits_mixed() {
        let input: Vec<u8> = (0..256).map(|i| (i % 64) as u8).collect();
        let encoded = packbits_encode(&input);
        let decoded = packbits_decode(&encoded, input.len()).unwrap();
        assert_eq!(decoded, input);
    }

    #[test]
    fn test_packbits_roundtrip_random() {
        let mut input = vec![0u8; 1000];
        for (i, b) in input.iter_mut().enumerate() {
            *b = (i.wrapping_mul(17).wrapping_add(13)) as u8;
        }
        let encoded = packbits_encode(&input);
        let decoded = packbits_decode(&encoded, input.len()).unwrap();
        assert_eq!(decoded, input);
    }

    #[test]
    fn test_packbits_noop() {
        // n = -128 (0x80) is a no-op.
        // After skipping the no-op, n=1 -> 2 literal bytes [2, 3].
        let encoded = vec![0x80u8, 1, 2, 3];
        let decoded = packbits_decode(&encoded, 3).unwrap();
        // 0x80 skipped, then 2 literals [2,3], padded to 3 bytes -> [2, 3, 0]
        assert_eq!(decoded, vec![2, 3, 0]);
    }

    #[test]
    fn test_rle_codec_transfer_syntax() {
        assert_eq!(RleCodec.transfer_syntax_uid(), ts_uid::RLE_LOSSLESS);
    }

    #[test]
    fn test_rle_encode_monochrome() {
        let pc = 10 * 10;
        let data: Vec<u8> = (0..pc).map(|i| (i % 256) as u8).collect();
        let pixels = DecodedPixels {
            data,
            rows: 10,
            columns: 10,
            samples_per_pixel: 1,
            bits_allocated: 8,
            photometric_interpretation: "MONOCHROME2".into(),
        };
        let meta = DicomMeta {
            rows: 10,
            columns: 10,
            samples_per_pixel: 1,
            bits_allocated: 8,
            bits_stored: 8,
            photometric_interpretation: "MONOCHROME2".into(),
            planar_configuration: None,
            pixel_representation: 0,
            frames: 1,
        };
        let encoded = RleCodec.encode(&pixels, &meta).unwrap();
        assert!(!encoded.is_empty());
        // PS3.5 Annex G header: [num_segments][offset 0 = 64]. The end of
        // the last segment is implicit (decoders bound it by output count).
        assert_eq!(u32_from_slice(&encoded[0..4]), 1);
        assert_eq!(u32_from_slice(&encoded[4..8]), 64);
        let (_, consumed) = packbits_decode_consumed(&encoded[64..], pc).unwrap();
        assert_eq!(64 + consumed, encoded.len());
    }

    #[test]
    fn test_rle_encode_color() {
        let pc = 10 * 10;
        let mut data = vec![0u8; pc * 3];
        for i in 0..pc {
            data[i] = (i % 256) as u8;
            data[pc + i] = ((i * 2) % 256) as u8;
            data[2 * pc + i] = ((i * 3) % 256) as u8;
        }
        let pixels = DecodedPixels {
            data,
            rows: 10,
            columns: 10,
            samples_per_pixel: 3,
            bits_allocated: 8,
            photometric_interpretation: "RGB".into(),
        };
        let meta = DicomMeta {
            rows: 10,
            columns: 10,
            samples_per_pixel: 3,
            bits_allocated: 8,
            bits_stored: 8,
            photometric_interpretation: "RGB".into(),
            planar_configuration: None,
            pixel_representation: 0,
            frames: 1,
        };
        let encoded = RleCodec.encode(&pixels, &meta).unwrap();
        assert!(encoded.len() >= 64);
        // Header: 3 segments, first segment offset = 64.
        assert_eq!(u32_from_slice(&encoded[0..4]), 3);
        assert_eq!(u32_from_slice(&encoded[4..8]), 64);
        // Offset table must be strictly increasing.
        let off0 = u32_from_slice(&encoded[4..8]);
        let off1 = u32_from_slice(&encoded[8..12]);
        let off2 = u32_from_slice(&encoded[12..16]);
        assert!(off0 < off1 && off1 < off2);
    }

    // -- Mini-DICOM builder ------------------------------------------

    /// Build a minimal RLE DICOM: meta TS=RLE + Rows/Cols/SamplesPerPixel/
    /// BitsAllocated/NumberOfFrames/Photometric + PixelData.
    /// `encapsulated` wraps `stream` in items (undefined length + delimiter).
    fn build_rle_test_dicom(
        stream: &[u8],
        rows: u16,
        cols: u16,
        spp: u16,
        bits: u16,
        frames: u16,
        encapsulated: bool,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 128]);
        out.extend_from_slice(b"DICM");

        // TransferSyntaxUID (0002,0010) UI = RLE Lossless. PS3.5 7.1: odd-
        // length values get a physical pad byte - write it like a real file
        // (the parser's skip_value consumes it).
        let ts = b"1.2.840.10008.1.2.5";
        out.extend_from_slice(&0x0002u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"UI");
        out.extend_from_slice(&(ts.len() as u16).to_le_bytes());
        out.extend_from_slice(ts);
        if !ts.len().is_multiple_of(2) {
            out.push(0x00);
        }

        let us_tag = |out: &mut Vec<u8>, tag: (u16, u16), v: u16| {
            out.extend_from_slice(&tag.0.to_le_bytes());
            out.extend_from_slice(&tag.1.to_le_bytes());
            out.extend_from_slice(b"US");
            out.extend_from_slice(&2u16.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        };
        us_tag(&mut out, (0x0028, 0x0010), rows);
        us_tag(&mut out, (0x0028, 0x0011), cols);
        us_tag(&mut out, (0x0028, 0x0002), spp);
        us_tag(&mut out, (0x0028, 0x0100), bits);
        if frames > 1 {
            // NumberOfFrames (0028,0008) is IS - a decimal string.
            let s = frames.to_string();
            out.extend_from_slice(&0x0028u16.to_le_bytes());
            out.extend_from_slice(&0x0008u16.to_le_bytes());
            out.extend_from_slice(b"IS");
            out.extend_from_slice(&(s.len() as u16).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
            if !s.len().is_multiple_of(2) {
                out.push(b' '); // PS3.5 7.1 pad byte
            }
        }

        // PhotometricInterpretation (0028,0004) CS "MONOCHROME2"
        out.extend_from_slice(&0x0028u16.to_le_bytes());
        out.extend_from_slice(&0x0004u16.to_le_bytes());
        out.extend_from_slice(b"CS");
        let pi = b"MONOCHROME2";
        out.extend_from_slice(&(pi.len() as u16).to_le_bytes());
        out.extend_from_slice(pi);
        if !pi.len().is_multiple_of(2) {
            out.push(b' '); // PS3.5 7.1 pad byte
        }

        // PixelData (7FE0,0010) OB
        out.extend_from_slice(&0x7FE0u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"OB");
        out.extend_from_slice(&[0x00, 0x00]);
        if encapsulated {
            out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            out.extend_from_slice(stream); // items pre-built by the caller
            out.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]); // Sequence Delimiter
            out.extend_from_slice(&0u32.to_le_bytes());
        } else {
            out.extend_from_slice(&(stream.len() as u32).to_le_bytes());
            out.extend_from_slice(stream);
        }
        out
    }

    fn rle_item(payload: &[u8]) -> Vec<u8> {
        let mut it = Vec::new();
        it.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]); // (FFFE,E000)
        it.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        it.extend_from_slice(payload);
        it
    }

    fn rle_frame_blob(pixels: &[u8]) -> Vec<u8> {
        let mut header = vec![0u8; 64];
        header[0..4].copy_from_slice(&1u32.to_le_bytes()); // 1 segment
        header[4..8].copy_from_slice(&64u32.to_le_bytes()); // segment at 64
        let mut blob = header;
        blob.extend_from_slice(&packbits_encode(pixels));
        blob
    }

    #[test]
    fn test_rle_decode_multiframe_item_wrapped() {
        // Field shape (01.09): multi-frame RLE whose pixel value is wrapped
        // in items - a Basic Offset Table item + one Item per frame.
        let frame0 = [10u8, 20, 30, 40];
        let frame1 = [50u8, 60, 70, 80];
        let f0 = rle_frame_blob(&frame0);
        // Real-world tolerance: field frames may carry a dead trailing byte
        // (literal-run header with its data missing) - decode must skip it.
        let f1 = [rle_frame_blob(&frame1), vec![0x00]].concat();

        // BOT: 2 frame offsets. Offsets count from the first byte of the
        // first Item after the BOT item (PS3.5 F.2.1), i.e. item headers
        // included - the decoder does not rely on them anyway.
        let mut bot = Vec::new();
        bot.extend_from_slice(&0u32.to_le_bytes());
        bot.extend_from_slice(&(8 + f0.len() as u32).to_le_bytes());
        let mut encapsulated = rle_item(&bot);
        encapsulated.extend_from_slice(&rle_item(&f0));
        encapsulated.extend_from_slice(&rle_item(&f1));

        let dicom = build_rle_test_dicom(&encapsulated, 2, 2, 1, 8, 2, true);
        let pixels = RleCodec.decode(&dicom).unwrap();
        assert_eq!(pixels.data, vec![10, 20, 30, 40, 50, 60, 70, 80]);
        assert_eq!(pixels.rows, 2);
        assert_eq!(pixels.columns, 2);
    }

    #[test]
    fn test_rle_encode_decode_roundtrip_multiframe() {
        // 2 frames, 2x2, 8-bit monochrome.
        let pixels = DecodedPixels {
            data: vec![10, 20, 30, 40, 50, 60, 70, 80],
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
            frames: 2,
        };
        let encoded = RleCodec.encode(&pixels, &meta).unwrap();
        // Two frame blobs, each with a spec header.
        assert_eq!(u32_from_slice(&encoded[0..4]), 1);
        let dicom = build_rle_test_dicom(&encoded, 2, 2, 1, 8, 2, false);
        let decoded = RleCodec.decode(&dicom).unwrap();
        assert_eq!(decoded.data, pixels.data);
    }

    #[test]
    fn test_rle_16bit_msb_plane_first() {
        // Two 16-bit pixels: 0x0A0B, 0x0C0D. Input layout is per-plane,
        // frame-major with per-pixel LE bytes: [0B, 0A, 0D, 0C].
        let pixels = DecodedPixels {
            data: vec![0x0B, 0x0A, 0x0D, 0x0C],
            rows: 1,
            columns: 2,
            samples_per_pixel: 1,
            bits_allocated: 16,
            photometric_interpretation: "MONOCHROME2".into(),
        };
        let meta = DicomMeta {
            rows: 1,
            columns: 2,
            samples_per_pixel: 1,
            bits_allocated: 16,
            bits_stored: 16,
            photometric_interpretation: "MONOCHROME2".into(),
            planar_configuration: None,
            pixel_representation: 0,
            frames: 1,
        };
        let encoded = RleCodec.encode(&pixels, &meta).unwrap();
        // Header: 2 segments; segment 0 = MSB plane, segment 1 = LSB plane.
        assert_eq!(u32_from_slice(&encoded[0..4]), 2);
        let off0 = u32_from_slice(&encoded[4..8]) as usize;
        let off1 = u32_from_slice(&encoded[8..12]) as usize;
        let seg0 = packbits_decode(&encoded[off0..off1], 2).unwrap();
        let seg1 = packbits_decode_consumed(&encoded[off1..], 2).unwrap().0;
        assert_eq!(seg0, vec![0x0A, 0x0C]); // MSB plane first
        assert_eq!(seg1, vec![0x0B, 0x0D]);

        let dicom = build_rle_test_dicom(&encoded, 1, 2, 1, 16, 1, false);
        let decoded = RleCodec.decode(&dicom).unwrap();
        assert_eq!(decoded.data, pixels.data);
    }
}
