// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Dependency-free PNG encoder for the `/rendered` preview route.
//!
//! Scope: encode only, 8-bit, color type 0 (Gray) and 2 (RGB), non-interlaced,
//! filter 0 (None) on every scanline, and a zlib stream built from STORED
//! DEFLATE blocks. Stored blocks carry no LZ77 match finder and no Huffman
//! coder, so this module contains no compression logic whatsoever - it is a
//! chunk writer plus two checksums. That is what makes a hand-rolled encoder
//! the safe choice here: the security-sensitive part of DEFLATE (the decoder)
//! does not exist in a server that only ever writes previews.
//!
//! References:
//! - PNG (second edition), W3C Recommendation 10 November 2003: 5.2 (chunk
//!   layout), 11.2.2 (IHDR), 9.2 (filter method 0), 10.3 (IDAT/zlib).
//! - RFC 1950 (zlib), 2.2/2.3 (CMF/FLG + Adler-32), RFC 1951 (DEFLATE), 3.2.4
//!   (stored / BTYPE=00 blocks).
//! - RFC 1952 / ISO 3309: the CRC-32 used by PNG chunk trailers.
//!
//! Measured (2026-10-13, Docker transcode build, 512x512 RGB8 incompressible
//! noise, N=30, `tmp/bench_rendered_before.py`): stored-block encode is
//! 87.1 -> 49.2 ms median vs the removed `png` crate, wire size unchanged -
//! noise does not compress, so the DEFLATE pass that crate paid was pure CPU
//! cost on this route.

#![forbid(unsafe_code)]

use std::fmt;

/// PNG color type (PNG spec 11.2.2, Table 11.1). Only the two types the
/// `/rendered` route emits are modelled; the names match the previous
/// `png` crate so the call sites in `wado_pure.rs` are unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorType {
    /// Color type 0: one sample per pixel, one channel.
    Grayscale,
    /// Color type 2: three samples per pixel, R/G/B in that order.
    Rgb,
}

impl ColorType {
    /// Bytes per pixel in the encoded raw (unfiltered) row.
    fn channels(self) -> u32 {
        match self {
            ColorType::Grayscale => 1,
            ColorType::Rgb => 3,
        }
    }

    /// The IHDR color-type byte (PNG spec Table 11.1).
    fn code(self) -> u8 {
        match self {
            ColorType::Grayscale => 0,
            ColorType::Rgb => 2,
        }
    }
}

/// Bit depth (PNG spec 11.2.2). The route only produces 8-bit previews; the
/// enum exists so the former call signature stays valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitDepth {
    /// 8 bits per sample.
    Eight,
}

impl BitDepth {
    fn code(self) -> u8 {
        match self {
            BitDepth::Eight => 8,
        }
    }
}

/// Failure of [`encode`]. Encoding is infallible for input whose sample count
/// matches `w * h * channels`; the only failure is geometry that cannot be
/// expressed in the 32-bit fields of the PNG chunk headers.
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

/// Little-endian u32 append, the byte order of the PNG chunk length/trailer.
/// Kept local so the encoder does not pull the `byteorder` crate (owned by
/// the ap101 core, not this crate).
#[inline]
fn push_be_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// zlib/DEFLATE stored-block LEN/NLEN are the only little-endian fields in
/// the whole stream (RFC 1951 3.2.4).
#[inline]
fn push_le_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

// ---------------------------------------------------------------------------
// CRC-32 (PNG chunk trailer, spec 5.2 / ISO 3309)
// ---------------------------------------------------------------------------

/// Reflected CRC-32 polynomial 0xEDB88320, expanded one byte at a time.
///
/// Slice-by-4 with `const fn` iteration keeps the table in `.rodata` and the
/// inner loop branch-free without a build script or a lazily initialised
/// `static mut` (which `#![forbid(unsafe_code)]` would reject anyway).
const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            // Branchless: (c & 1) ? (c >> 1) ^ poly : (c >> 1).
            c = (c >> 1) ^ (0xEDB8_8320u32 & (0u32.wrapping_sub(c & 1)));
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC32_TABLE: [u32; 256] = crc32_table();

/// One step of the CRC-32 state machine over `data`, returning the raw
/// (un-finalised) register. Chainable: `crc32_raw(crc32_raw(SEED, a), b)` is
/// the same register as `crc32_raw(SEED, a ++ b)`, which is what lets the
/// encoder checksum a chunk written across several `extend_from_slice` calls.
const fn crc32_seed() -> u32 {
    0xFFFF_FFFF
}

fn crc32_raw(crc: u32, data: &[u8]) -> u32 {
    let mut c = crc;
    for &b in data {
        c = (c >> 8) ^ CRC32_TABLE[((c ^ b as u32) & 0xFF) as usize];
    }
    c
}

/// Finalise: PNG's CRC-32 is the register complemented (ISO 3309 / PNG 5.5).
#[inline]
fn crc32_final(crc: u32) -> u32 {
    crc ^ 0xFFFF_FFFF
}

/// CRC-32 of a single contiguous `data` run. The encoder chains
/// [`crc32_raw`] directly because a chunk is written in two pieces (type,
/// then data); this one-shot form is the test reference.
#[cfg(test)]
fn crc32(data: &[u8]) -> u32 {
    crc32_final(crc32_raw(crc32_seed(), data))
}

// ---------------------------------------------------------------------------
// Adler-32 (zlib stream trailer, RFC 1950 9.2)
// ---------------------------------------------------------------------------

/// Adler-32 of `data`: s1 = 1 + sum(bytes), s2 = sum(s1 over the bytes), both
/// mod 65521, combined as `(s2 << 16) | s1`. The encoder advances the same
/// arithmetic incrementally through [`Adler32`]; this one-shot form is the
/// reference the tests compare it against.
#[cfg(test)]
fn adler32(data: &[u8]) -> u32 {
    let mut state = Adler32::new();
    let _ = state.update(data);
    state.finish()
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Bytes of a complete PNG with a single IDAT chunk, all-stored DEFLATE and
/// zlib's two-byte header (no FDICT, FCHECK=1 -> CMF/FLG = 78 01).
///
/// Layout: 8 signature + (12 + 13) IHDR + (12 + zlib_len) IDAT + 12 IEND,
/// and `zlib_len = 2 + 5 * blocks + filtered + 4` with
/// `blocks = ceil(filtered / 65535)`, floored at 1 because a zlib stream
/// always carries at least one (possibly empty) DEFLATE block. Each stored
/// block costs a 5-byte header (RFC 1951 3.2.4: one tag byte + LEN + NLEN)
/// ahead of its payload.
pub const fn encoded_len(filtered: usize) -> usize {
    // 8 signature + (4 len + 4 type + 13 data + 4 crc) IHDR
    // + (4 + 4 + zlib + 4) IDAT + (4 + 4 + 0 + 4) IEND.
    8 + 25 + (12 + zlib_stream_len(filtered)) + 12
}

/// Bytes of the zlib stream wrapping the filtered data: 2 header bytes,
/// one 5-byte stored-block header per block, the payload, and the 4-byte
/// Adler-32 trailer (RFC 1950 2.2, RFC 1951 3.2.4).
pub const fn zlib_stream_len(filtered: usize) -> usize {
    let blocks = if filtered == 0 {
        1
    } else {
        filtered.div_ceil(65535)
    };
    2 + 5 * blocks + filtered + 4
}

/// Encode raw 8-bit samples as a PNG.
///
/// `samples` is the unfiltered image data in PNG order: for
/// [`ColorType::Grayscale`] one byte per pixel, for [`ColorType::Rgb`] three
/// bytes per pixel (R, G, B). Rows are tightly packed with no padding; a
/// source `BitsAllocated` greater than 8 must already have been reduced.
///
/// Exactly one allocation is performed, sized to [`encoded_len`], and the
/// returned buffer satisfies `len() == capacity()`. Fails only when the
/// geometry cannot be represented: a zero extent, or `w * h * channels`
/// overflowing `u32`.
pub fn encode(
    samples: &[u8],
    w: u32,
    h: u32,
    color: ColorType,
    depth: BitDepth,
) -> Result<Vec<u8>, EncodingError> {
    if w == 0 || h == 0 {
        return Err(EncodingError {
            reason: "png: image extent must be non-zero",
        });
    }
    let ch = color.channels();
    // Filter 0 prepends one tag byte per scanline (PNG spec 9.2), so the
    // filtered stream is h * (1 + w*ch).
    let row = w.checked_mul(ch).ok_or(EncodingError {
        reason: "png: row size overflows u32",
    })?;
    // The caller's buffer must match the geometry exactly. A short buffer
    // panicked mid-encode on a slice index in the /rendered route, and a long
    // one silently shifted every row, so the preview showed the wrong pixels.
    let expected = (row as usize)
        .checked_mul(h as usize)
        .ok_or(EncodingError {
            reason: "png: sample count overflows usize",
        })?;
    if samples.len() != expected {
        return Err(EncodingError {
            reason: "png: sample count does not match the image extent",
        });
    }
    let filtered = (h as usize) * (row as usize + 1);

    let total = encoded_len(filtered);
    let mut out = Vec::with_capacity(total);

    // PNG signature (spec 5.2).
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    // IHDR: width, height, bit depth, color type, compression 0, filter 0,
    // interlace 0 (spec 11.2.2). 13 data bytes.
    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&w.to_be_bytes());
    ihdr[4..8].copy_from_slice(&h.to_be_bytes());
    ihdr[8] = depth.code();
    ihdr[9] = color.code();
    ihdr[10] = 0;
    ihdr[11] = 0;
    ihdr[12] = 0;
    write_chunk(&mut out, b"IHDR", &ihdr);

    // IDAT: the whole zlib stream in one chunk (spec 10.3 allows any split;
    // one chunk keeps the sizing formula exact). It is written straight into
    // `out` - buffering it separately would cost a second allocation, and the
    // ap101 gate pins the encoder at exactly one.
    let zlib_len = zlib_stream_len(filtered);
    push_be_u32(&mut out, zlib_len as u32);
    let idat_start = out.len();
    out.extend_from_slice(b"IDAT");
    out.extend_from_slice(&[0x78, 0x01]); // CMF/FLG: DEFLATE, 32K window, FCHECK=1
    let adler = write_stored_blocks(&mut out, samples, w, h, ch);
    out.extend_from_slice(&adler.to_be_bytes());
    debug_assert_eq!(out.len() - idat_start, 4 + zlib_len);
    let crc = crc32_raw(crc32_seed(), &out[idat_start..idat_start + 4]);
    let crc = crc32_raw(crc, &out[idat_start + 4..]);
    push_be_u32(&mut out, crc32_final(crc));

    // IEND: empty (spec 11.2.5).
    write_chunk(&mut out, b"IEND", &[]);

    debug_assert_eq!(out.len(), total);
    debug_assert_eq!(out.len(), out.capacity());
    Ok(out)
}

/// Append a chunk: 4-byte BE length + 4-byte type + data + 4-byte BE CRC32
/// over type+data (spec 5.2).
fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    push_be_u32(out, data.len() as u32);
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32_raw(crc32_seed(), kind);
    let crc = crc32_raw(crc, data);
    push_be_u32(out, crc32_final(crc));
}

/// Running Adler-32 state, so the checksum can be advanced chunk by chunk as
/// the filtered stream is emitted instead of re-scanning it.
///
/// The reduction is applied after every `NMAX` bytes, the zlib bound; keeping
/// the cadence in one place is what makes the incremental result identical to
/// [`adler32`] over the whole stream.
struct Adler32 {
    s1: u32,
    s2: u32,
    since_reduce: usize,
}

impl Adler32 {
    const NMAX: usize = 5552;

    fn new() -> Self {
        Self {
            s1: 1,
            s2: 0,
            since_reduce: 0,
        }
    }

    /// Fold `data` in, returning the number of bytes consumed. The caller
    /// re-invokes with the remainder: splitting anywhere is allowed because
    /// the reduction schedule depends only on the total byte count.
    fn update(&mut self, data: &[u8]) -> usize {
        let mut used = 0usize;
        while used < data.len() {
            let take = (Self::NMAX - self.since_reduce).min(data.len() - used);
            for &b in &data[used..used + take] {
                self.s1 += b as u32;
                self.s2 += self.s1;
            }
            self.since_reduce += take;
            used += take;
            if self.since_reduce == Self::NMAX {
                self.s1 %= 65521;
                self.s2 %= 65521;
                self.since_reduce = 0;
            }
        }
        used
    }

    fn finish(self) -> u32 {
        ((self.s2 % 65521) << 16) | (self.s1 % 65521)
    }
}

/// Emit the filtered scanlines as a sequence of STORED DEFLATE blocks
/// (RFC 1951 3.2.4): block header byte, LE LEN, LE NLEN (= !LEN), raw bytes.
/// Returns the Adler-32 of the filtered stream, which the caller appends as
/// the zlib trailer - computing it here avoids re-scanning the payload and,
/// more importantly, avoids materialising the filtered stream at all.
///
/// A stored block carries at most 65535 payload bytes, so the filtered stream
/// is split on that boundary. The final block sets BFINAL.
fn write_stored_blocks(out: &mut Vec<u8>, samples: &[u8], w: u32, h: u32, ch: u32) -> u32 {
    let row = (w * ch) as usize;
    let filtered = (h as usize) * (row + 1);
    let blocks = if filtered == 0 {
        1
    } else {
        filtered.div_ceil(65535)
    };
    // The filtered stream is a plain byte sequence: a block may end mid-row
    // and the next block simply continues it. Emitting it incrementally
    // therefore needs only the current position within the row.
    let mut adler = Adler32::new();
    let mut block_index = 0usize;
    let mut cur_row = 0u32;
    let mut row_pos = 0usize;
    let mut filter_tag_pending = true;
    while block_index < blocks {
        let len = (filtered - block_index * 65535).min(65535);
        let last = block_index + 1 == blocks;
        out.push(if last { 0x01 } else { 0x00 });
        push_le_u16(out, len as u16);
        push_le_u16(out, !(len as u16));
        let mut written = 0usize;
        while written < len {
            if filter_tag_pending {
                out.push(0x00); // filter type 0 (None) for the next scanline
                adler.update(&[0]);
                filter_tag_pending = false;
                written += 1;
                if written == len {
                    break;
                }
            }
            let take = (row - row_pos).min(len - written);
            let chunk = &samples[(cur_row as usize) * row + row_pos..][..take];
            out.extend_from_slice(chunk);
            adler.update(chunk);
            row_pos += take;
            written += take;
            if row_pos == row {
                row_pos = 0;
                cur_row += 1;
                filter_tag_pending = true;
            }
        }
        block_index += 1;
    }
    adler.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Self-contained stored-block PNG reader used only to prove that the
    /// encoder's output round-trips: it verifies the CRC of every chunk, the
    /// zlib header/trailer, and reassembles the filtered stream into the
    /// original samples. Byte-for-byte comparison against the previous
    /// encoder is impossible (compression differs), so pixel equality is the
    /// criterion.
    fn decode_gray(body: &[u8]) -> (u32, u32, Vec<u8>) {
        decode_pixels(body, 1)
    }

    /// Verify every chunk CRC, the zlib header/trailer and the stored-block
    /// framing, then strip the per-scanline filter tags back off.
    fn decode_pixels(body: &[u8], ch: usize) -> (u32, u32, Vec<u8>) {
        assert_eq!(
            &body[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "signature"
        );
        let mut pos = 8usize;
        let mut idat = Vec::new();
        let mut w = 0u32;
        let mut h = 0u32;
        while pos < body.len() {
            let len = u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap()) as usize;
            let kind = &body[pos + 4..pos + 8];
            let data = &body[pos + 8..pos + 8 + len];
            let stored =
                u32::from_be_bytes(body[pos + 8 + len..pos + 12 + len].try_into().unwrap());
            let crc = crc32_raw(crc32_seed(), kind);
            assert_eq!(
                crc32_final(crc32_raw(crc, data)),
                stored,
                "chunk CRC {:?}",
                kind
            );
            match kind {
                b"IHDR" => {
                    assert_eq!(
                        &data[8..13],
                        &[8, if ch == 1 { 0 } else { 2 }, 0, 0, 0],
                        "depth/color/method"
                    );
                    w = u32::from_be_bytes(data[0..4].try_into().unwrap());
                    h = u32::from_be_bytes(data[4..8].try_into().unwrap());
                }
                b"IDAT" => idat.extend_from_slice(data),
                b"IEND" => break,
                _ => panic!("unexpected chunk"),
            }
            pos += 12 + len;
        }
        assert_eq!(&idat[..2], &[0x78, 0x01], "zlib header");

        let mut filtered = Vec::new();
        let mut p = 2usize;
        loop {
            let final_block = idat[p] & 1 == 1;
            assert_eq!(idat[p] & 0b110, 0, "BTYPE must be stored");
            let len = u16::from_le_bytes(idat[p + 1..p + 3].try_into().unwrap());
            let nlen = u16::from_le_bytes(idat[p + 3..p + 5].try_into().unwrap());
            assert_eq!(!len, nlen, "NLEN is the one's complement of LEN");
            filtered.extend_from_slice(&idat[p + 5..p + 5 + len as usize]);
            p += 5 + len as usize;
            if final_block {
                break;
            }
        }
        assert_eq!(p, idat.len() - 4, "stream ends after the final block");

        // The zlib trailer covers the DEFLATE payload, i.e. the reassembled
        // filtered bytes - NOT the raw bytes after the header, which also
        // contain the stored-block LEN/NLEN framing (RFC 1950 2.2).
        let want_adler = u32::from_be_bytes(idat[idat.len() - 4..].try_into().unwrap());
        assert_eq!(adler32(&filtered), want_adler, "adler32");

        let row = (w as usize) * ch;
        assert_eq!(filtered.len(), h as usize * (row + 1));
        let mut samples = Vec::with_capacity(row * h as usize);
        for y in 0..h as usize {
            assert_eq!(filtered[y * (row + 1)], 0, "filter type 0");
            samples.extend_from_slice(&filtered[y * (row + 1) + 1..y * (row + 1) + 1 + row]);
        }
        (w, h, samples)
    }

    fn ramp(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn gray8_round_trips() {
        let (w, h) = (37u32, 19u32);
        let src = ramp((w * h) as usize);
        let body = encode(&src, w, h, ColorType::Grayscale, BitDepth::Eight).expect("encode");
        let (dw, dh, got) = decode_gray(&body);
        assert_eq!((dw, dh), (w, h));
        assert_eq!(got, src);
    }

    #[test]
    fn encoded_len_matches_exact_wire_size() {
        let (w, h) = (37u32, 19u32);
        let filtered = (h as usize) * (w as usize + 1);
        let body = encode(
            &ramp((w * h) as usize),
            w,
            h,
            ColorType::Grayscale,
            BitDepth::Eight,
        )
        .expect("encode");
        assert_eq!(body.len(), encoded_len(filtered));
        assert_eq!(body.len(), body.capacity(), "no slack, no realloc");
    }

    #[test]
    fn multi_block_stream_round_trips() {
        // 300x300 gray = 90_300 filtered bytes -> 2 stored blocks, training the
        // LEN/NLEN boundary and the BFINAL flag on a non-first block.
        let (w, h) = (300u32, 300u32);
        let src = ramp((w * h) as usize);
        let body = encode(&src, w, h, ColorType::Grayscale, BitDepth::Eight).expect("encode");
        let filtered = (h as usize) * (w as usize + 1);
        assert_eq!(body.len(), encoded_len(filtered));
        assert_eq!(body.len(), 63 + 5 * filtered.div_ceil(65535) + filtered);
        let (_, _, got) = decode_gray(&body);
        assert_eq!(got, src);
    }

    #[test]
    fn rgb8_round_trips() {
        let (w, h) = (23u32, 11u32);
        let src = ramp((w * h * 3) as usize);
        let body = encode(&src, w, h, ColorType::Rgb, BitDepth::Eight).expect("encode");
        let (dw, dh, got) = decode_pixels(&body, 3);
        assert_eq!((dw, dh), (w, h));
        assert_eq!(got, src);
    }

    #[test]
    fn single_row_is_one_block_boundary() {
        let body = encode(&[7u8; 1], 1, 1, ColorType::Grayscale, BitDepth::Eight).expect("encode");
        assert_eq!(body.len(), encoded_len(2));
        let (w, h, got) = decode_gray(&body);
        assert_eq!((w, h), (1, 1));
        assert_eq!(got, vec![7u8]);
    }

    #[test]
    fn zero_extent_is_rejected() {
        assert!(encode(&[], 0, 4, ColorType::Grayscale, BitDepth::Eight).is_err());
        assert!(encode(&[], 4, 0, ColorType::Grayscale, BitDepth::Eight).is_err());
    }

    #[test]
    fn adler32_matches_known_vector() {
        // RFC 1950's worked example for the string "Wikipedia".
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn crc32_matches_known_vector() {
        // ISO 3309 check value for the ASCII string "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        // Chaining must be equivalent to one pass over the concatenation.
        assert_eq!(
            crc32_final(crc32_raw(crc32_raw(crc32_seed(), b"12345"), b"6789")),
            crc32(b"123456789")
        );
    }
}
