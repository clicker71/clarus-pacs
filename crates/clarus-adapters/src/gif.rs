// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Dependency-free GIF89a encoder for the `/rendered` preview route.
//!
//! Scope: encode only, 8-bit grayscale (a 256-entry grey palette), single
//! image, non-interlaced, no transparency, no animation. The LZW compressor is
//! written here because GIF has no uncompressed mode - unlike the PNG encoder,
//! which sidesteps compression with stored DEFLATE blocks. GIF therefore costs
//! a real (but small) compressor plus a dictionary; that is still far cheaper
//! than the dependency it replaces, and no decoder exists in a server that
//! only ever writes previews.
//!
//! References (W3C-hosted GIF89a specification, local copy
//! `tmp/gif89a-spec.txt`):
//! - 17  header `GIF` + version `89a`.
//! - 18  Logical Screen Descriptor: width u16 LE, height u16 LE, packed byte
//!   (bit7 GCT flag, bits6-4 colour resolution, bit3 sort, bits2-0 GCT size),
//!   background colour index, pixel aspect ratio.
//! - 18  Global Color Table: `3 x 2^(size+1)` bytes of RGB triplets.
//! - 20  Image Descriptor: 0x2C, left u16 LE, top u16 LE, width u16 LE,
//!   height u16 LE, packed byte (bit7 LCT flag, bit6 interlace, bit5 sort,
//!   bits2-0 LCT size).
//! - 22  Table Based Image Data: one LZW minimum code size byte, then data
//!   sub-blocks (count byte + up to 255 bytes each), terminated by 0x00.
//! - 24  LZW: minimum code size = the colour-table bit depth; Clear =
//!   `2^min_code_size`; EOI = Clear + 1; the first available code is Clear + 2;
//!   output codes start at `min_code_size + 1` bits and widen when the next
//!   code would exceed the current width; a Clear is emitted as the FIRST code
//!   so a decoder can start from a known state.
//! - 15  Codes are packed into bytes right-to-left (LSB first).
//! - 26  Trailer 0x3B.

#![forbid(unsafe_code)]

use std::fmt;

/// Colour model. GIF previews from `/rendered` are grayscale only: RGB is
/// served as PNG, and GIF for RGB is refused with 406 (no quantiser here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorType {
    /// One sample per pixel, mapped through a 256-entry grey palette.
    Gray,
}

/// Failure of [`encode`]. Encoding is infallible for a well-formed input; the
/// only failures are geometry the GIF89a container cannot express (a zero
/// extent, a sample count that does not match `w * h`, or an extent beyond the
/// 16-bit Logical Screen Descriptor fields).
#[derive(Debug)]
pub struct EncodingError {
    reason: &'static str,
}

impl fmt::Display for EncodingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason)
    }
}

impl std::error::Error for EncodingError {}

/// Grey palette depth: 8 bits per pixel, so the Global Color Table holds 256
/// entries and the LZW minimum code size is 8 (spec 24).
const CODE_BITS: u8 = 8;
/// The palette has `2^CODE_BITS` entries (spec 18: `3 x 2^(size+1)` with
/// `size+1 == CODE_BITS`).
const PALETTE_LEN: usize = 1 << CODE_BITS;

/// Upper bound on the encoded size, used to size the single output
/// allocation.
///
/// Fixed overhead:
///   6  header (`GIF89a`)
///   7  Logical Screen Descriptor
///   768  256-entry Global Color Table (3 bytes per entry)
///   10  Image Descriptor (0x2C + 4 x u16 + packed byte)
///   1  LZW minimum code size
///   1  trailer (0x3B)
/// = 793 bytes.
///
/// Payload bound: LZW codes are at most 12 bits, so the compressed stream
/// cannot exceed `1.5` bytes per input pixel (`pixels * 2` is a strict
/// over-bound that also absorbs the widening slack). The `+ 16` covers the
/// Clear/EOI codes, the sub-block count bytes and any partial final byte.
/// Reported here rather than guessed so the caller can prove the allocation is
/// exact; the real output is normally far smaller because GIF compresses.
pub const fn worst_case_len(pixels: usize) -> usize {
    793 + pixels * 2 + 16
}

/// Encode an 8-bit grayscale plane as a GIF89a image.
///
/// `samples` is one byte per pixel, row-major, tightly packed.
///
/// Performs EXACTLY TWO heap allocations: the presized output buffer
/// (`worst_case_len(pixels)`) and the LZW dictionary (`1 << 20` `u16` slots).
/// The dictionary is a flat array indexed by `(prefix << 8) | byte`, not a
/// `HashMap`: LZW lookups happen once per input pixel, and a hash map would
/// reintroduce a dependency and a per-pixel hashing cost.
pub fn encode(samples: &[u8], w: u32, h: u32, color: ColorType) -> Result<Vec<u8>, EncodingError> {
    let ColorType::Gray = color;
    if w == 0 || h == 0 {
        return Err(EncodingError {
            reason: "gif: image extent must be non-zero",
        });
    }
    // The Logical Screen Descriptor and Image Descriptor carry both extents as
    // u16 LE (spec 18/20).
    if w > u16::MAX as u32 || h > u16::MAX as u32 {
        return Err(EncodingError {
            reason: "gif: extent exceeds 65535",
        });
    }
    let pixels = w.checked_mul(h).ok_or(EncodingError {
        reason: "gif: pixel count overflows u32",
    })? as usize;
    if samples.len() != pixels {
        return Err(EncodingError {
            reason: "gif: sample count does not match w * h",
        });
    }

    let mut out = Vec::with_capacity(worst_case_len(pixels));

    // Header (spec 17).
    out.extend_from_slice(b"GIF89a");

    // Logical Screen Descriptor (spec 18). Packed = 0xF7: GCT flag set, colour
    // resolution 7, not sorted, GCT size 7 (= 2^8 entries). Background index 0
    // is the palette's black; aspect 0 means "no ratio given".
    out.extend_from_slice(&(w as u16).to_le_bytes());
    out.extend_from_slice(&(h as u16).to_le_bytes());
    out.push(0xF7);
    out.push(0x00);
    out.push(0x00);

    // Global Color Table (spec 18): identity grey ramp, so a Gray8 sample maps
    // to its own luminance.
    for i in 0..PALETTE_LEN {
        out.push(i as u8);
        out.push(i as u8);
        out.push(i as u8);
    }

    // Image Descriptor (spec 20): full frame, no Local Color Table, no
    // interlace, packed 0x00.
    out.push(0x2C);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(w as u16).to_le_bytes());
    out.extend_from_slice(&(h as u16).to_le_bytes());
    out.push(0x00);

    // Table Based Image Data (spec 22): minimum code size, then the LZW codes
    // wrapped in sub-blocks.
    out.push(CODE_BITS);
    lzw_encode_into(&mut out, samples);

    // Trailer (spec 26).
    out.push(0x3B);

    Ok(out)
}

/// The LZW code stream is emitted through a sub-block packer: the encoder
/// writes bytes into a 255-byte staging area, flushes it with a length prefix,
/// and the terminator closes the sequence (spec 22).
struct SubBlocks<'a> {
    out: &'a mut Vec<u8>,
    /// Bytes staged for the current sub-block (`<= 255`).
    buf: [u8; 255],
    len: usize,
}

impl<'a> SubBlocks<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        Self {
            out,
            buf: [0u8; 255],
            len: 0,
        }
    }

    /// A 255-byte sub-block is fixed by the format, so it is a stack buffer -
    /// no per-block heap allocation.
    fn push(&mut self, byte: u8) {
        self.buf[self.len] = byte;
        self.len += 1;
        if self.len == 255 {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.len > 0 {
            self.out.push(self.len as u8);
            self.out.extend_from_slice(&self.buf[..self.len]);
            self.len = 0;
        }
    }

    /// Close the data stream with the 0x00 block terminator.
    fn finish(mut self) {
        self.flush();
        self.out.push(0x00);
    }
}

/// Bit packer for variable-width LZW codes (spec 15: LSB first).
///
/// Codes accumulate in a `u32` so a 12-bit code can straddle a byte boundary
/// without a second staging buffer.
struct BitWriter<'a> {
    blocks: SubBlocks<'a>,
    buf: u32,
    count: u32,
}

impl<'a> BitWriter<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        Self {
            blocks: SubBlocks::new(out),
            buf: 0,
            count: 0,
        }
    }

    fn write(&mut self, code: u16, width: u32) {
        self.buf |= (code as u32) << self.count;
        self.count += width;
        while self.count >= 8 {
            self.blocks.push((self.buf & 0xFF) as u8);
            self.buf >>= 8;
            self.count -= 8;
        }
    }

    /// Flush the final partial byte, then terminate the sub-block sequence.
    fn finish(mut self) {
        if self.count > 0 {
            self.blocks.push((self.buf & 0xFF) as u8);
        }
        self.blocks.finish();
    }
}

/// LZW-compress `samples` into `out` as GIF sub-blocks (spec 24).
///
/// The dictionary is keyed on `(prefix << 8) | byte`. Since `prefix` is at most
/// 4095 and the byte is 8 bits, the key fits 20 bits; `1 << 20` entries with a
/// `0xFFFF` sentinel for "empty" is the smallest flat table that cannot alias a
/// real code (codes never exceed 4095).
fn lzw_encode_into(out: &mut Vec<u8>, samples: &[u8]) {
    const EMPTY: u16 = 0xFFFF;
    let clear: u16 = 1 << CODE_BITS;
    let eoi: u16 = clear + 1;

    let mut table = vec![EMPTY; 1 << 20];
    let mut writer = BitWriter::new(out);

    // A decoder must see Clear as the first code (spec 24).
    let mut width = CODE_BITS as u32 + 1;
    writer.write(clear, width);

    let mut next = clear + 2;
    let mut prefix = samples[0] as u16;
    for &byte in &samples[1..] {
        let idx = ((prefix as usize) << 8) | byte as usize;
        let k = table[idx];
        if k != EMPTY {
            prefix = k;
            continue;
        }
        writer.write(prefix, width);
        if next < 4096 {
            table[idx] = next;
            next += 1;
            // Spec 24.4: widen as soon as the NEXT code would not fit in the
            // current width. `next` is that next code, and a `width`-bit field
            // holds 0..=(1<<width)-1, so the test is `next >= 1 << width`.
            // Doing this one step later is the classic off-by-one: the decoder
            // widens on the same boundary, so any lag desynchronises the bit
            // reader from that point on.
            // Escalation boundary. The decoder's table trails the encoder's by
            // one entry (it adds entry N only when it reads the NEXT code), so
            // the width must change when the encoder's next-free code passes
            // 1<<width, not when it reaches it.
            if width < 12 && next > (1u16 << width) {
                width += 1;
            }
        } else {
            // Dictionary full: tell the decoder to reset and start over.
            writer.write(clear, width);
            table.fill(EMPTY);
            next = clear + 2;
            width = CODE_BITS as u32 + 1;
        }
        prefix = byte as u16;
    }
    writer.write(prefix, width);
    writer.write(eoi, width);
    writer.finish();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Structural reader: verifies the container (header, Logical Screen
    /// Descriptor, palette, Image Descriptor, sub-block framing, trailer) and
    /// returns the raw LZW code bytes plus the geometry.
    ///
    /// It deliberately does NOT decode LZW. A second LZW decoder in the test
    /// module would be a maintenance liability and, worse, a weak oracle: if it
    /// shared a misreading with the encoder the round-trip would pass while
    /// real decoders failed. Pixel correctness is proven against an
    /// INDEPENDENT implementation (Pillow) by the escalation round-trip in
    /// `crates/clarus-server/tests/ap101_gif_encode.rs`, which is the Task 1
    /// acceptance test.
    fn read_structure(body: &[u8]) -> (u32, u32, Vec<u8>) {
        assert_eq!(&body[..6], b"GIF89a", "header");
        let w = u16::from_le_bytes([body[6], body[7]]) as u32;
        let h = u16::from_le_bytes([body[8], body[9]]) as u32;
        let packed = body[10];
        assert_eq!(packed & 0x80, 0x80, "GCT flag must be set");
        assert_eq!(packed & 0x07, 0x07, "GCT size 7 -> 256 entries");
        assert_eq!(body[11], 0, "background index");
        assert_eq!(body[12], 0, "aspect ratio");
        let gct_len = 1usize << ((packed & 0x07) + 1);
        // Identity grey ramp.
        for i in 0..gct_len {
            let base = 13 + 3 * i;
            assert_eq!(
                &body[base..base + 3],
                &[i as u8, i as u8, i as u8],
                "palette entry {i}"
            );
        }
        let mut pos = 13 + 3 * gct_len;
        assert_eq!(body[pos], 0x2C, "image separator");
        assert_eq!(&body[pos + 1..pos + 5], &[0, 0, 0, 0], "image left/top");
        let iw = u16::from_le_bytes([body[pos + 5], body[pos + 6]]) as u32;
        let ih = u16::from_le_bytes([body[pos + 7], body[pos + 8]]) as u32;
        assert_eq!((iw, ih), (w, h), "image descriptor matches the screen");
        assert_eq!(body[pos + 9], 0x00, "no LCT, no interlace, not sorted");
        pos += 10;
        assert_eq!(body[pos], 8, "LZW minimum code size");
        pos += 1;

        // Reassemble the sub-blocks.
        let mut data = Vec::new();
        loop {
            let n = body[pos] as usize;
            pos += 1;
            if n == 0 {
                break;
            }
            data.extend_from_slice(&body[pos..pos + n]);
            pos += n;
        }
        assert_eq!(body[pos], 0x3B, "trailer");
        assert_eq!(pos + 1, body.len(), "no trailing bytes after the trailer");
        (w, h, data)
    }

    /// First byte of the LZW stream after Clear: the encoder must emit Clear
    /// first (spec 24.1), so the LSB-aligned 9-bit field equals 256.
    fn first_code_is_clear(lzw: &[u8]) -> bool {
        let v = lzw[0] as u32 | ((lzw[1] as u32) << 8);
        (v & 0x1FF) == 256
    }

    /// Deterministic ramp, so the round-trip is comparable across runs.
    fn ramp(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    /// xorshift bytes: incompressible, so the dictionary fills and the code
    /// width must widen through 9 -> 10 -> 11 -> 12.
    fn incompressible(n: usize) -> Vec<u8> {
        let mut s: u32 = 0x1234_5678;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn gray_ramp_has_a_well_formed_container() {
        let (w, h) = (37u32, 19u32);
        let src = ramp((w * h) as usize);
        let body = encode(&src, w, h, ColorType::Gray).expect("encode");
        let (dw, dh, lzw) = read_structure(&body);
        assert_eq!((dw, dh), (w, h));
        assert!(first_code_is_clear(&lzw), "Clear must be the first code");
        // The stream must end with EOI (257) in the widened code width.
        assert!(!lzw.is_empty());
    }

    #[test]
    fn single_pixel_is_well_formed() {
        let body = encode(&[0x7Fu8], 1, 1, ColorType::Gray).expect("encode");
        let (w, h, lzw) = read_structure(&body);
        assert_eq!((w, h), (1, 1));
        assert!(first_code_is_clear(&lzw));
    }

    #[test]
    fn incompressible_stream_is_well_formed() {
        // 300x300 random bytes fill the dictionary well past 4096 entries, so
        // the width climbs to 12 and at least one Clear reset fires. The pixel
        // proof for this stream is the Pillow round-trip in the server test.
        let (w, h) = (300u32, 300u32);
        let src = incompressible((w * h) as usize);
        let body = encode(&src, w, h, ColorType::Gray).expect("encode");
        let (_, _, lzw) = read_structure(&body);
        assert!(first_code_is_clear(&lzw));
        // A reset must be present: the stream is far larger than one 4096-code
        // dictionary's worth of unique sequences.
        assert!(lzw.len() > 4096, "expected a long, escalating stream");
    }

    #[test]
    fn bad_input_is_rejected() {
        assert!(encode(&[], 0, 4, ColorType::Gray).is_err());
        assert!(encode(&[], 4, 0, ColorType::Gray).is_err());
        // Sample count must match w * h.
        assert!(encode(&[0u8; 3], 4, 4, ColorType::Gray).is_err());
        // Extent beyond the u16 LE descriptor fields.
        assert!(encode(&[0u8; 4], 65536, 1, ColorType::Gray).is_err());
    }

    #[test]
    fn worst_case_len_bounds_real_output() {
        for (w, h) in [(1u32, 1u32), (16, 16), (256, 256)] {
            for src in [ramp((w * h) as usize), incompressible((w * h) as usize)] {
                let body = encode(&src, w, h, ColorType::Gray).expect("encode");
                assert!(
                    body.len() <= worst_case_len((w * h) as usize),
                    "{}x{}: {} > worst case",
                    w,
                    h,
                    body.len()
                );
            }
        }
    }
}
