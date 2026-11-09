// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Clarus adapters layer - infrastructure implementations.
//!
//! This crate contains concrete implementations of the port interfaces:
//! - Content-addressed file storage (CAS)
//! - DICOM streaming parser (dicom-rs)
//! - DICOM transcoding codecs (G-05)

#![forbid(unsafe_code)]

pub mod storage;
pub use storage::ContentAddressedStorage;

pub mod dicom_rs;
pub use dicom_rs::DicomRsParser;

pub mod bit_depth;
pub mod codec;
pub mod transcode;

// -- Re-exports for clarus-server (NGX-003: deduplicate dependencies) --
// `base64` (own RFC4648 encoder), `bytes`/`futures`/`async-trait` re-exports
// removed on 22.08 diet: base64 -> local encoder, bytes -> under `s3`,
// futures/async-trait -> dead deps (0 consumers).
pub use blake3;
// `jpeg_encoder` is unconditional (C1 22.08): the raw-path thumbnail encoder
// in the default build. The DECODERS stay behind `feature = "transcode"`.
pub use jpeg_encoder;
// `png` is a local module (1310): the /rendered route's PNG encoder for RGB8
// and Gray8 previews. Hand-rolled and dependency-free - STORED DEFLATE blocks
// mean there is no compressor and no decoder, so the former `png` crate's
// fdeflate/flate2/miniz_oxide/crc32fast/simd-adler32 chain is gone entirely.
pub mod png;
// `gif` is a local module (1310, closes D1): the /rendered route's Gray8
// GIF89a encoder. GIF has no uncompressed mode, so this one carries a small
// LZW compressor plus a flat dictionary - still 0 dependencies, and no
// decoder, because the server only ever writes previews.
pub mod gif;
// `sha2` is used only by the S3 backend (SigV4 HMAC-SHA256 / etag). It is
// not part of the default build (CAS hashing is BLAKE3).
#[cfg(feature = "s3")]
pub use sha2;

// -- Unique ID generator (replaces `uuid` crate - KISS) ----------------

use std::sync::atomic::{AtomicU64, Ordering};

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a unique hex string (replaces `Uuid::new_v4()`).
///
/// The result is a 64-bit value, and it is not a hash: the fields are
/// concatenated, never XORed. `ts ^ cnt` was a collision generator - two calls
/// whose timestamp fields differ only in the bits the counter happens to flip
/// produce the same number, so a temp file from one request could land on top
/// of another's.
///
/// Layout, most significant first: 24 bits of timestamp seconds, 16 bits of
/// pid, 24 bits of counter. The fields are packed into the LOW bits of a `u64`
/// rather than assuming each one has its full width there: seconds alone would
/// take 32 of the 64, leaving no room for a pid wider than eight bits.
/// Truncating the seconds keeps the low bits, which is what changes second to
/// second; the wrap period is 2^24 s (about 194 days), and the counter - not
/// the clock - is what separates two ids issued in the same second.
///
/// The pid field is the low 16 bits, not the full process id. Eight bits was
/// the original width and is too narrow: two processes whose ids differ by a
/// multiple of 256 get the same field, which on a host that reassigns pids (or
/// a namespace that counts from 1) is ordinary rather than exotic. Sixteen bits
/// moves that to a multiple of 65536 and costs no extra width here.
///
/// The remaining collision case is therefore exact: two processes whose pids
/// differ by a multiple of 65536, in the same second, with equal counter
/// values. Inside one process the counter alone forbids it. A collision lands
/// on a scratch path, so the worst outcome is one failed temp operation, not
/// lost data - which is why the field is not widened further.
pub fn unique_id() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let cnt = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed) & 0x00FF_FFFF;
    let pid = u64::from(std::process::id() & 0xFFFF);

    // 24 bits seconds | 16 bits pid | 24 bits counter = 64.
    let id = ((secs & 0x00FF_FFFF) << 40) | ((pid & 0xFFFF) << 24) | cnt;
    format!("{id:016x}")
}

/// Generate a 32-char hex boundary for MIME multipart (replaces short boundary).
///
/// Two concatenated `unique_id()` calls. The entropy is NOT 128 random bits:
/// the ids are counter- and clock-derived, so the collision domain is the
/// number of ids issued, not $2^{128}$. In practice the counter makes repeats
/// impossible within a process, and the pid separates processes, which is what
/// a boundary actually needs - it must not equal any prefix of the part body.
pub fn boundary_id() -> String {
    format!("{}{}", unique_id(), unique_id())
}

// -- Hex encoding (KISS - replaces `hex` crate) -----------------------

/// Hex-encode bytes (replaces `hex` crate - KISS).
const HEX: &[u8; 16] = b"0123456789abcdef";

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_encode() {
        assert_eq!(hex_encode(b""), "");
        assert_eq!(hex_encode(&[0x00, 0xff, 0xab]), "00ffab");
    }
}
