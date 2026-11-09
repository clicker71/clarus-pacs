// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Streaming CBOR token reader over a borrowed byte slice.
//!
//! The reader is deliberately low-level: each call to [`CborReader::next`]
//! yields one CBOR data item without allocating. Nesting of arrays and maps
//! is exposed to the caller via [`CborToken::ArrayStart`] / [`CborToken::MapStart`]
//! carrying the element/entry count; the caller tracks depth explicitly.
//!
//! # Slicing without allocation
//!
//! Text and byte strings are returned as slices of the input buffer. This is
//! what makes the parser zero-allocation: no heap buffer is ever touched.
//! UTF-8 validation for text strings is performed in place by
//! `std::str::from_utf8`, which borrows and never allocates.

use crate::cbor::CborError;

/// A single CBOR data item, borrowed from the input slice.
///
/// Array and map starts should be consumed in the order deposited by the
/// encoder. The value is the declared length (elements for arrays, key/value
/// entry pairs for maps).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CborToken<'a> {
    /// Major 0 - unsigned integer.
    UInt(u64),
    /// Major 3 - UTF-8 text string.
    Text(&'a str),
    /// Major 2 - byte string (e.g. a 32-byte BLAKE3 hash).
    Bytes(&'a [u8]),
    /// Major 4 - array of `u64` elements.
    ArrayStart(u64),
    /// Major 5 - map of `u64` key/value entry pairs.
    MapStart(u64),
}

/// Minimal streaming CBOR reader.
#[derive(Debug)]
pub struct CborReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

const MAJOR_UINT: u8 = 0;
const MAJOR_BYTES: u8 = 2;
const MAJOR_TEXT: u8 = 3;
const MAJOR_ARRAY: u8 = 4;
const MAJOR_MAP: u8 = 5;

/// Deepest container nesting this reader will walk.
///
/// Bounds the recursion in [`CborReader::skip_value`] and in the manifest's
/// nested attribute decoder. The structures this codebase writes nest at most
/// four deep (envelope -> study map -> attribute array -> element), so 32 is
/// generous for legitimate data and shallow enough that a crafted stream of
/// `0x81` bytes cannot exhaust a worker's stack.
pub const MAX_NESTING_DEPTH: usize = 32;

impl<'a> CborReader<'a> {
    /// Create a reader positioned at the start of `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Number of bytes consumed so far.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Number of unread bytes remaining.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// True if the whole buffer has been consumed.
    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// Read the next CBOR data item.
    ///
    /// Returns `CborError::Truncated` when the buffer ends mid-item and
    /// `CborError::Unsupported` for majors outside the manifest subset.
    // The name deliberately mirrors a streaming token source, not
    // `std::iter::Iterator` (this type is not an iterator and does not
    // implement `Iterator`).
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<CborToken<'a>, CborError> {
        let initial = self.pos;
        let head = self.read_u8()?;
        let major = head >> 5;
        let info = head & 0x1f;

        // Indefinite-length containers are not part of the manifest subset.
        if info == 31 {
            return Err(CborError::Unsupported(major));
        }

        match major {
            MAJOR_UINT => {
                let value = self.read_length(info)?;
                Ok(CborToken::UInt(value))
            }
            MAJOR_BYTES => {
                let len = self.read_length(info)? as usize;
                let slice = self.take(len)?;
                Ok(CborToken::Bytes(slice))
            }
            MAJOR_TEXT => {
                let len = self.read_length(info)? as usize;
                let slice = self.take(len)?;
                // Borrowed UTF-8 validation - zero allocation.
                let text = std::str::from_utf8(slice).map_err(|_| {
                    self.pos = initial;
                    CborError::InvalidUtf8
                })?;
                Ok(CborToken::Text(text))
            }
            MAJOR_ARRAY => {
                let len = self.read_length(info)?;
                Ok(CborToken::ArrayStart(len))
            }
            MAJOR_MAP => {
                let len = self.read_length(info)?;
                Ok(CborToken::MapStart(len))
            }
            _ => Err(CborError::Unsupported(major)),
        }
    }

    /// Skip past `n` physical bytes without interpreting them.
    fn take(&mut self, n: usize) -> Result<&'a [u8], CborError> {
        let end = self.pos.checked_add(n).ok_or(CborError::Overlong)?;
        if end > self.buf.len() {
            return Err(CborError::Truncated);
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// Skip past one complete CBOR data item, including all nested
    /// containers (arrays and maps). This lets shallow-decode paths
    /// cheaply jump over sections they do not need (e.g. `KEY_SERIES`
    /// when only study-level QIDO attributes are wanted).
    ///
    /// The nesting depth is BOUNDED. A stream of `0x81` bytes ("array of one")
    /// is a legal CBOR item at any depth, so an unbounded walk recursed until
    /// the stack overflowed and the process aborted - from one small uploaded
    /// manifest. [`MAX_NESTING_DEPTH`] is far deeper than any structure this
    /// codebase writes (envelope -> attrs -> array -> element) and shallow
    /// enough that the recursion cannot exhaust a worker stack.
    pub fn skip_value(&mut self) -> Result<(), CborError> {
        self.skip_value_at(0)
    }

    fn skip_value_at(&mut self, depth: usize) -> Result<(), CborError> {
        if depth > MAX_NESTING_DEPTH {
            return Err(CborError::Unsupported(7));
        }
        let head = self.read_u8()?;
        let major = head >> 5;
        let info = head & 0x1f;

        if info == 31 {
            return Err(CborError::Unsupported(major));
        }

        match major {
            MAJOR_UINT => {
                self.read_length(info)?;
                Ok(())
            }
            MAJOR_BYTES | MAJOR_TEXT => {
                let len = self.read_length(info)? as usize;
                self.take(len)?;
                Ok(())
            }
            MAJOR_ARRAY => {
                let len = self.read_length(info)?;
                for _ in 0..len {
                    self.skip_value_at(depth + 1)?;
                }
                Ok(())
            }
            MAJOR_MAP => {
                let len = self.read_length(info)?;
                for _ in 0..len {
                    self.skip_value_at(depth + 1)?; // key
                    self.skip_value_at(depth + 1)?; // value
                }
                Ok(())
            }
            _ => Err(CborError::Unsupported(major)),
        }
    }

    fn read_u8(&mut self) -> Result<u8, CborError> {
        let b = *self.buf.get(self.pos).ok_or(CborError::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    /// Decode the argument-vs-embedded-value length for a header byte.
    fn read_length(&mut self, info: u8) -> Result<u64, CborError> {
        match info {
            0..=23 => Ok(info as u64),
            24 => {
                let b = self.read_u8()?;
                Ok(b as u64)
            }
            25 => {
                let hi = self.read_u8()?;
                let lo = self.read_u8()?;
                Ok(((hi as u64) << 8) | (lo as u64))
            }
            26 => {
                let a = self.read_u8()?;
                let b = self.read_u8()?;
                let c = self.read_u8()?;
                let d = self.read_u8()?;
                Ok(((a as u64) << 24) | ((b as u64) << 16) | ((c as u64) << 8) | (d as u64))
            }
            27 => {
                let mut value: u64 = 0;
                for _ in 0..8 {
                    value = (value << 8) | (self.read_u8()? as u64);
                }
                Ok(value)
            }
            _ => Err(CborError::Unsupported(31)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deeply nested stream must fail, not overflow the stack.
    ///
    /// `0x81` is "array of one", legal CBOR at ANY depth. Walking it without a
    /// bound recursed until the stack ran out and the process aborted - one
    /// small uploaded manifest was enough.
    #[test]
    fn skip_value_refuses_excessive_nesting() {
        // One array wrapper per level, then a single uint leaf.
        let deep = vec![0x81u8; MAX_NESTING_DEPTH + 8];
        let mut r = CborReader::new(&deep);
        assert!(
            r.skip_value().is_err(),
            "a stream deeper than MAX_NESTING_DEPTH must be refused"
        );
    }

    /// Legitimate nesting (a few levels) still skips cleanly.
    #[test]
    fn skip_value_accepts_legitimate_nesting() {
        // [ [ [ 1 ] ] ] - three levels, the deepest shape this codebase writes.
        let nested = [0x81u8, 0x81, 0x81, 0x01];
        let mut r = CborReader::new(&nested);
        r.skip_value().expect("legitimate nesting skips");
        assert!(r.is_empty());
    }

    #[test]
    fn reads_embedded_uints() {
        let mut r = CborReader::new(&[0x00, 0x17, 0x18, 0x2a]);
        assert_eq!(r.next().unwrap(), CborToken::UInt(0));
        assert_eq!(r.next().unwrap(), CborToken::UInt(23));
        assert_eq!(r.next().unwrap(), CborToken::UInt(42));
        assert!(r.is_empty());
    }

    #[test]
    fn reads_multibyte_uints() {
        // 0x19 0x01 0x00 => 256 (two-byte argument)
        // 0x1a 0x00 0x00 0x01 0x00 => 256 (four-byte argument)
        let mut r = CborReader::new(&[0x19, 0x01, 0x00, 0x1a, 0x00, 0x00, 0x01, 0x00]);
        assert_eq!(r.next().unwrap(), CborToken::UInt(256));
        assert_eq!(r.next().unwrap(), CborToken::UInt(256));
    }

    #[test]
    fn reads_byte_and_text_strings() {
        // 0x42 'A' 'B'  -> bytes "AB"
        // 0x61 'x'      -> text "x"
        let mut r = CborReader::new(&[0x42, b'A', b'B', 0x61, b'x']);
        assert_eq!(r.next().unwrap(), CborToken::Bytes(b"AB"));
        assert_eq!(r.next().unwrap(), CborToken::Text("x"));
    }

    #[test]
    fn reads_array_and_map_starts() {
        // empty array 0x80, empty map 0xa0
        let mut r = CborReader::new(&[0x80, 0xa0, 0x83, 0xa1]);
        assert_eq!(r.next().unwrap(), CborToken::ArrayStart(0));
        assert_eq!(r.next().unwrap(), CborToken::MapStart(0));
        assert_eq!(r.next().unwrap(), CborToken::ArrayStart(3));
        assert_eq!(r.next().unwrap(), CborToken::MapStart(1));
    }

    #[test]
    fn rejects_unsupported_major_types() {
        // 0x20 (major 1, negative int), 0x60/0x61...
        let mut r = CborReader::new(&[0x20]);
        assert_eq!(r.next().unwrap_err(), CborError::Unsupported(1));

        // float 0xf9 0x3c 0x00 (major 7)
        let mut r = CborReader::new(&[0xf9, 0x3c, 0x00]);
        assert_eq!(r.next().unwrap_err(), CborError::Unsupported(7));
    }

    #[test]
    fn rejects_indefinite_length() {
        let mut r = CborReader::new(&[0x9f]); // indefinite array
        assert_eq!(r.next().unwrap_err(), CborError::Unsupported(4));
    }

    #[test]
    fn truncated_item_errors() {
        // Declares a 2-byte text string but only provides one byte.
        let mut r = CborReader::new(&[0x62, b'a']);
        assert_eq!(r.next().unwrap_err(), CborError::Truncated);
    }

    #[test]
    fn invalid_utf8_text_errors() {
        let mut r = CborReader::new(&[0x61, 0xff]);
        assert_eq!(r.next().unwrap_err(), CborError::InvalidUtf8);
    }
}
