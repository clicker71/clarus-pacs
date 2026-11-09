// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Zero-allocation CBOR codec restricted to the subset required by the
//! CBOR manifest format (RFC 8949).
//!
//! Supported data items (per the plan):
//! - major 0 - unsigned int (`u64`)
//! - major 2 - byte string (BLAKE3 hashes, 32 bytes)
//! - major 3 - text string (UIDs, names)
//! - major 4 - array (series, instances, frames)
//! - major 5 - map (system header, study QIDO keys, DICOM attribute maps)
//!
//! Explicitly rejected: negative ints (major 1), tagged items (major 6), and
//! floats/simple values (major 7). Indefinite-length containers (`0x1f`) are
//! not emitted and rejected on read - lengths are always known up front.
//!
//! # Zero-allocation contract
//!
//! [`CborReader::next`] returns borrowed tokens (no `String`, no `Vec`).
//! [`CborWriter`] writes into a caller-provided buffer and never allocates.
//! The typed manifest layer (`crate::manifest`) is the only place that
//! assembles owned structs, which is not the codec hotpath.

mod encode;
mod parse;

pub use encode::CborWriter;
pub use parse::{CborReader, CborToken, MAX_NESTING_DEPTH};

/// Errors produced by the CBOR codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CborError {
    /// The input ended mid-item.
    Truncated,
    /// A header byte or length prefix exceeds the encoded range.
    Overlong,
    /// A data item type outside the manifest subset was encountered.
    /// Negative ints, tags, floats, and indefinite-length containers.
    Unsupported(u8),
    /// A text string is not valid UTF-8.
    InvalidUtf8,
    /// Encoder ran out of space in the output buffer.
    BufferFull,
    /// Decoder found trailing bytes after the expected item count.
    TrailingData,
}

impl std::fmt::Display for CborError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CborError::Truncated => write!(f, "CBOR input truncated"),
            CborError::Overlong => write!(f, "CBOR length prefix overlong"),
            CborError::Unsupported(major) => write!(f, "CBOR major type {major} unsupported"),
            CborError::InvalidUtf8 => write!(f, "CBOR text string not valid UTF-8"),
            CborError::BufferFull => write!(f, "CBOR output buffer exhausted"),
            CborError::TrailingData => write!(f, "CBOR trailing bytes after item"),
        }
    }
}

impl std::error::Error for CborError {}
