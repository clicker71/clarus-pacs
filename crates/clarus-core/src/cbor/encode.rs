// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Fixed-buffer CBOR encoder.
//!
//! The writer targets a caller-provided `&mut [u8]` and never allocates.
//! Container headers (arrays, maps) are written with their full element
//! count up front, matching the reader's expectation of known-length
//! containers (no indefinite-length encoding).
//!
//! Lengths use the minimal representation (canonical form): values <= 23
//! embed in the header byte; larger values switch to the smallest of the
//! 1/2/4/8-byte argument forms.

use crate::cbor::CborError;

const MAJOR_UINT: u8 = 0;
const MAJOR_BYTES: u8 = 2;
const MAJOR_TEXT: u8 = 3;
const MAJOR_ARRAY: u8 = 4;
const MAJOR_MAP: u8 = 5;

/// Minimal fixed-buffer CBOR encoder.
#[derive(Debug)]
pub struct CborWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> CborWriter<'a> {
    /// Create a writer over `buf`.
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes written so far.
    pub fn written(&self) -> usize {
        self.pos
    }

    /// Unused capacity remaining in the buffer.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// The slice of `buf` filled so far (for tests and inspection).
    pub fn as_slice(&self) -> &[u8] {
        &self.buf[..self.pos]
    }

    /// Write an unsigned integer (major 0).
    pub fn write_u64(&mut self, value: u64) -> Result<(), CborError> {
        self.write_head(MAJOR_UINT, value)
    }

    /// Write a text string (major 3).
    pub fn write_text(&mut self, s: &str) -> Result<(), CborError> {
        self.write_head(MAJOR_TEXT, s.len() as u64)?;
        self.put_bytes(s.as_bytes())
    }

    /// Write a byte string (major 2).
    pub fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), CborError> {
        self.write_head(MAJOR_BYTES, bytes.len() as u64)?;
        self.put_bytes(bytes)
    }

    /// Write an array header with `len` declared elements (major 4).
    pub fn write_array_header(&mut self, len: u64) -> Result<(), CborError> {
        self.write_head(MAJOR_ARRAY, len)
    }

    /// Write a map header with `len` declared key/value entries (major 5).
    pub fn write_map_header(&mut self, len: u64) -> Result<(), CborError> {
        self.write_head(MAJOR_MAP, len)
    }

    /// Emit a type marker plus its (minimal) argument.
    fn write_head(&mut self, major: u8, value: u64) -> Result<(), CborError> {
        let head = major << 5;
        match value {
            0..=23 => self.put_u8(head | (value as u8)),
            24..=0xff => {
                self.put_u8(head | 24)?;
                self.put_u8(value as u8)
            }
            0x100..=0xffff => {
                self.put_u8(head | 25)?;
                self.put_u8((value >> 8) as u8)?;
                self.put_u8((value & 0xff) as u8)
            }
            0x1_0000..=0xffff_ffff => {
                self.put_u8(head | 26)?;
                self.put_u8((value >> 24) as u8)?;
                self.put_u8((value >> 16) as u8)?;
                self.put_u8((value >> 8) as u8)?;
                self.put_u8((value & 0xff) as u8)
            }
            _ => {
                self.put_u8(head | 27)?;
                for shift in (0..8).rev() {
                    self.put_u8(((value >> (shift * 8)) & 0xff) as u8)?;
                }
                Ok(())
            }
        }
    }

    fn put_u8(&mut self, b: u8) -> Result<(), CborError> {
        let slot = self.buf.get_mut(self.pos).ok_or(CborError::BufferFull)?;
        *slot = b;
        self.pos += 1;
        Ok(())
    }

    fn put_bytes(&mut self, bytes: &[u8]) -> Result<(), CborError> {
        let end = self
            .pos
            .checked_add(bytes.len())
            .ok_or(CborError::BufferFull)?;
        if end > self.buf.len() {
            return Err(CborError::BufferFull);
        }
        self.buf[self.pos..end].copy_from_slice(bytes);
        self.pos = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cbor::CborReader;

    fn encode_all(ops: impl FnOnce(&mut CborWriter<'_>)) -> Vec<u8> {
        let mut buf = [0u8; 256];
        let mut w = CborWriter::new(&mut buf);
        ops(&mut w);
        w.as_slice().to_vec()
    }

    #[test]
    fn writes_embedded_uints() {
        let out = encode_all(|w| {
            w.write_u64(0).unwrap();
            w.write_u64(23).unwrap();
            w.write_u64(24).unwrap();
        });
        // 24 switches to the 1-byte argument form.
        assert_eq!(out, vec![0x00, 0x17, 0x18, 0x18]);
    }

    #[test]
    fn writes_roundtrip_text() {
        let out = encode_all(|w| w.write_text("CT").unwrap());
        let mut r = CborReader::new(&out);
        assert_eq!(r.next().unwrap(), crate::cbor::CborToken::Text("CT"));
    }

    #[test]
    fn writes_roundtrip_bytes() {
        let out = encode_all(|w| w.write_bytes(b"AB").unwrap());
        let mut r = CborReader::new(&out);
        assert_eq!(r.next().unwrap(), crate::cbor::CborToken::Bytes(b"AB"));
    }

    #[test]
    fn writes_map_with_uint_keys() {
        // A map `{1: 0x17, 2: "CT"}` encodes as:
        // 0xa2 (map, 2 entries), 0x01 0x17, 0x02 0x62 'C' 'T'
        let out = encode_all(|w| {
            w.write_map_header(2).unwrap();
            w.write_u64(1).unwrap();
            w.write_u64(0x17).unwrap();
            w.write_u64(2).unwrap();
            w.write_text("CT").unwrap();
        });
        assert_eq!(out, vec![0xa2, 0x01, 0x17, 0x02, 0x62, b'C', b'T']);
    }

    #[test]
    fn buffer_full_is_reported() {
        let mut buf = [0u8; 2];
        let mut w = CborWriter::new(&mut buf);
        w.write_text("abc").unwrap_err();
    }

    #[test]
    fn large_uint_roundtrip() {
        let out = encode_all(|w| w.write_u64(0x1_0000_0000).unwrap());
        let mut r = CborReader::new(&out);
        assert_eq!(
            r.next().unwrap(),
            crate::cbor::CborToken::UInt(0x1_0000_0000)
        );
    }
}
