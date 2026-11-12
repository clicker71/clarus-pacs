// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! HTTP/1.1 request body handling (E-003).
//!
//! Content-Length bodies are a plain byte count read straight off the socket.
//! Chunked bodies are decoded by [`ChunkedReader<R>`], a `Read` adapter that
//! wraps the socket and implements the RFC 9112 Section 7.1 transfer-grammar as a
//! small state machine with a 3-byte pushback. It allocates nothing and keeps
//! no body bytes - the consumer reads decoded bytes as they are produced.
//!
//! ```text
//! size-line "\r\n" ("0" size-line -> trailers "CRLF CRLF") | (data CRLF)*
//! ```
//!
//! Because all framing tokens are <= 4 bytes and the reader only ever needs to
//! look one token ahead, a 3-byte pushback buffer is sufficient to survive any
//! socket boundary without losing a single byte.

use std::io::{self, Read};

use crate::http::conn::CHUNK_LINE;
use crate::http::conn::TRAILER_LIMIT;

/// Body framing chosen from the request head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// Exactly `n` bytes of body.
    Length(u64),
    /// Chunked transfer-encoding.
    Chunked,
}

/// Select framing from an already-validated request head.
///
/// The smuggling gate is executed in `parse::parse_headers`, not here: that is
/// where `Content-Length` and `Transfer-Encoding` are rejected if they appear
/// together or if either is repeated. By the time a head reaches this function
/// at most one of the two is present, so the `transfer_encoding.is_some()`
/// preference below cannot be reached with both set. It is kept as a defensive
/// default - `framing` takes plain options and is also called from tests and
/// from paths that do not go through the header parser, and preferring chunked
/// is the safer answer if that ever changes.
pub fn framing(content_length: Option<u64>, transfer_encoding: Option<&[u8]>) -> Framing {
    if transfer_encoding.is_some() {
        Framing::Chunked
    } else {
        Framing::Length(content_length.unwrap_or(0))
    }
}

/// Read exactly `n` bytes from `r`, passing each chunk to `on_chunk`.
/// Used for Content-Length bodies (STOW streaming). Returns an
/// `UnexpectedEof` if the connection closes early.
pub fn read_length<R: Read, F: FnMut(&[u8]) -> io::Result<()>>(
    r: &mut R,
    mut n: u64,
    on_chunk: &mut F,
) -> io::Result<()> {
    let mut buf = [0u8; crate::http::conn::READ_BUF];
    while n > 0 {
        let want = (n as usize).min(buf.len());
        let got = r.read(&mut buf[..want])?;
        if got == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "premature end of body (Content-Length)",
            ));
        }
        on_chunk(&buf[..got])?;
        n -= got as u64;
    }
    Ok(())
}

/// Pushback-capable byte source with a small internal buffer.
///
/// Generic over the underlying reader so the same logic works over a blocking
/// `TcpStream` in production and over `&[u8]` slices in tests.
#[derive(Debug)]
struct PushbackReader<R: Read> {
    inner: R,
    /// Bytes pushed back (not yet re-served).
    buf: [u8; 3],
    len: usize,
}

impl<R: Read> PushbackReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            buf: [0u8; 3],
            len: 0,
        }
    }

    /// Read exactly `out.len()` bytes (from pushback first, then inner).
    /// Returns `Ok(false)` on EOF before filling.
    fn read_exact_lt(&mut self, out: &mut [u8]) -> io::Result<bool> {
        let mut filled = 0usize;
        while filled < out.len() {
            if self.len > 0 {
                let take = self.len.min(out.len() - filled);
                let start = self.buf.len() - self.len;
                out[filled..filled + take].copy_from_slice(&self.buf[start..start + take]);
                self.len -= take;
                filled += take;
                continue;
            }
            let n = self.inner.read(&mut out[filled..])?;
            if n == 0 {
                return Ok(false);
            }
            filled += n;
        }
        Ok(true)
    }

    /// Read a single byte. `Ok(None)` on clean EOF.
    fn read_byte(&mut self) -> io::Result<Option<u8>> {
        let mut b = [0u8; 1];
        Ok(if self.read_exact_lt(&mut b)? {
            Some(b[0])
        } else {
            None
        })
    }
}

/// Reads a byte prefix (e.g. body bytes prefetched by the head reader in the
/// same TCP segment as the request head) before falling through to the
/// underlying stream. Empty prefixes fall straight through.
#[derive(Debug)]
pub struct PrefixedReader<R: Read> {
    inner: R,
    prefix: Vec<u8>,
    pos: usize,
}

impl<R: Read> PrefixedReader<R> {
    pub fn new(inner: R, prefix: Vec<u8>) -> Self {
        Self {
            inner,
            prefix,
            pos: 0,
        }
    }

    /// Bytes of the prefix not yet handed out.
    pub fn prefix_remaining(&self) -> usize {
        self.prefix.len() - self.pos
    }
}

impl<R: Read> Read for PrefixedReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos < self.prefix.len() {
            let take = (self.prefix.len() - self.pos).min(out.len());
            out[..take].copy_from_slice(&self.prefix[self.pos..self.pos + take]);
            self.pos += take;
            return Ok(take);
        }
        self.inner.read(out)
    }
}

/// Chunked transfer-encoding decoder as a `Read` adapter.
///
/// # State machine
/// - `Size`: read hex digits then `CRLF` -> if size 0 go to `Trailer`, else
///   `Data(n)`.
/// - `Data(n)`: hand out exactly `n` body bytes (only the first `n` of each
///   `read` that spans the boundary are returned; the rest are pushed back).
/// - `DataCrlf`: expect `CRLF` after a data chunk.
/// - `Trailer`: skip lines until `CRLF CRLF` (bounded by [`TRAILER_LIMIT`]).
#[derive(Debug)]
pub struct ChunkedReader<R: Read> {
    src: PushbackReader<R>,
    state: ChunkState,
    hex: [u8; CHUNK_LINE],
    hex_len: usize,
    remaining: u64,
    trailer_used: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    Size,
    Data,
    DataCrlf,
    Trailer,
    Done,
}

impl<R: Read> ChunkedReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            src: PushbackReader::new(inner),
            state: ChunkState::Size,
            hex: [0u8; CHUNK_LINE],
            hex_len: 0,
            remaining: 0,
            trailer_used: 0,
        }
    }

    /// True once the message (including trailers) is fully read.
    pub fn is_message_complete(&self) -> bool {
        self.state == ChunkState::Done
    }
}

impl<R: Read> Read for ChunkedReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.state {
                ChunkState::Done => return Ok(0),
                ChunkState::Size => {
                    // Parse `<hexdigits> CR LF`.
                    self.hex_len = 0;
                    loop {
                        match self.src.read_byte()? {
                            None => return Err(eof_err("chunk size")),
                            Some(b'\r') => {
                                // RFC 9112 7.1: chunk-size = 1*HEXDIG. A bare
                                // CRLF is NOT a zero-sized chunk, it is an
                                // empty chunk-size, and `hex_value` over zero
                                // digits returns 0 - so a bare CRLF was read as
                                // the terminating chunk and the rest of the
                                // body was silently discarded.
                                if self.hex_len == 0 {
                                    return Err(invalid("chunk size line: no hex digits"));
                                }
                                let lf = self
                                    .src
                                    .read_byte()?
                                    .ok_or_else(|| eof_err("chunk size CRLF"))?;
                                if lf != b'\n' {
                                    return Err(invalid("chunk size line: expected LF after CR"));
                                }
                                let size = self.hex_value();
                                self.remaining = size;
                                self.state = if size == 0 {
                                    ChunkState::Trailer
                                } else {
                                    ChunkState::Data
                                };
                                break;
                            }
                            Some(b' ') | Some(b'\t') => {
                                // Strict: extensions are rejected. A space after
                                // digits is the start of an extension.
                                return Err(invalid("chunk extensions not allowed"));
                            }
                            Some(b) => {
                                let d = hex_digit(b).ok_or_else(|| invalid("bad hex in size"))?;
                                if self.hex_len >= CHUNK_LINE {
                                    return Err(invalid("chunk size line too long"));
                                }
                                self.hex[self.hex_len] = d;
                                self.hex_len += 1;
                            }
                        }
                    }
                }
                ChunkState::Data => {
                    if self.remaining == 0 {
                        self.state = ChunkState::DataCrlf;
                        continue;
                    }
                    let want = (self.remaining as usize).min(out.len());
                    let got = self.src.read_exact_lt(&mut out[..want])?;
                    if !got {
                        return Err(eof_err("chunk data"));
                    }
                    self.remaining -= want as u64;
                    // If we over-read (want == remaining but out had more space),
                    // nothing extra was consumed - `read_exact_lt` reads exactly
                    // `want` bytes, so no pushback is needed here.
                    return Ok(want);
                }
                ChunkState::DataCrlf => {
                    let mut crlf = [0u8; 2];
                    let ok = self.src.read_exact_lt(&mut crlf)?;
                    if !ok {
                        return Err(eof_err("chunk CRLF"));
                    }
                    if &crlf != b"\r\n" {
                        return Err(invalid("chunk data: missing CRLF"));
                    }
                    self.state = ChunkState::Size;
                }
                ChunkState::Trailer => {
                    // Skip trailer lines until the empty line `\r\n`.
                    // A line is empty when it contains exactly `\r` before `\n`.
                    // Budget-bounded by TRAILER_LIMIT.
                    let mut line_len = 0usize; // bytes on the current line (incl `\r`)
                    let mut prev = 0u8; // byte seen before the current one
                    loop {
                        let b = match self.src.read_byte()? {
                            None => {
                                // Clean EOF mid-trailer with nothing on line is
                                // tolerated only if no trailer bytes were seen.
                                if self.trailer_used == 0 && line_len == 0 {
                                    return Ok(0);
                                }
                                return Err(eof_err("trailer"));
                            }
                            Some(b) => b,
                        };
                        self.trailer_used += 1;
                        if self.trailer_used > TRAILER_LIMIT {
                            return Err(invalid("trailer too large"));
                        }
                        line_len += 1;
                        if b == b'\n' {
                            // Line ended. Empty line == exactly `\r\n`: a bare
                            // LF leaves `line_len == 1` and therefore does NOT
                            // terminate, it only ends a non-empty line. Testing
                            // the length alone accepted `X\n`, whose second
                            // byte is the LF, as the terminator.
                            if line_len == 2 && prev == b'\r' {
                                // The terminating CRLF is the END of the body;
                                // a read after it must answer clean EOF (0),
                                // not re-enter the trailer scan and then fail
                                // on the missing trailer byte. The `Done` state
                                // is what makes `read` after completion a
                                // no-op instead of an error.
                                self.state = ChunkState::Done;
                                return Ok(0); // message complete
                            }
                            line_len = 0;
                        }
                        // Remember this byte for the next iteration's CR test.
                        // Must stay INSIDE the loop: the terminator check above
                        // reads it on the very next byte.
                        prev = b;
                    }
                }
            }
        }
    }
}

impl<R: Read> ChunkedReader<R> {
    fn hex_value(&self) -> u64 {
        let mut n: u64 = 0;
        for i in 0..self.hex_len {
            n = n * 16 + self.hex[i] as u64;
        }
        n
    }
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn eof_err(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        format!("unexpected EOF in {what}"),
    )
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ByteReader<'a>(&'a [u8]);
    impl<'a> Read for ByteReader<'a> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            buf[0] = self.0[0];
            self.0 = &self.0[1..];
            Ok(1)
        }
    }

    /// Drain a `ChunkedReader` fully, stopping at EOF (Ok(0)).
    fn drain<R: Read>(cr: &mut ChunkedReader<R>) -> Vec<u8> {
        let mut all = Vec::new();
        let mut buf = [0u8; 16];
        loop {
            let n = match cr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("chunked error: {e}"),
            };
            all.extend_from_slice(&buf[..n]);
        }
        all
    }

    #[test]
    fn chunked_body_decodes_single_chunk() {
        let chunked = b"5\r\nhello\r\n0\r\n\r\n";
        let mut cr = ChunkedReader::new(ByteReader(chunked));
        assert_eq!(drain(&mut cr), b"hello");
    }

    #[test]
    fn chunked_body_decodes_multiple_chunks() {
        let chunked = b"3\r\nfoo\r\n3\r\nbar\r\n0\r\n\r\n";
        let mut cr = ChunkedReader::new(ByteReader(chunked));
        assert_eq!(drain(&mut cr), b"foobar");
    }

    #[test]
    fn chunked_with_trailer_fields() {
        // Trailer section `X-Checksum: x\r\n\r\n` after the 0-chunk.
        let chunked = b"2\r\nhi\r\n0\r\nX-Trailer: abc\r\n\r\n";
        let mut cr = ChunkedReader::new(ByteReader(chunked));
        assert_eq!(drain(&mut cr), b"hi");
    }

    /// After the terminating CRLF the body is DONE: a further read must answer
    /// clean EOF (Ok(0)) and the message must report complete. Without the
    /// `Done` state a post-completion read re-entered the trailer scan, hit the
    /// empty source and returned EOF-mid-trailer (`is_message_complete` never
    /// true, because `trailer_used == 0` could not hold after the scan).
    #[test]
    fn a_completed_body_reads_eof_and_reports_complete() {
        let chunked = b"2\r\nhi\r\n0\r\n\r\n";
        let mut cr = ChunkedReader::new(ByteReader(chunked));
        assert_eq!(drain(&mut cr), b"hi");
        assert!(cr.is_message_complete());
        assert_eq!(cr.read(&mut [0u8; 4]).expect("eof"), 0);
    }

    #[test]
    fn chunked_bad_hex_rejected() {
        let chunked = b"zz\r\nhi\r\n0\r\n\r\n";
        let mut cr = ChunkedReader::new(ByteReader(chunked));
        assert!(matches!(
            cr.read(&mut [0u8; 4]),
            Err(e) if e.kind() == io::ErrorKind::InvalidData
        ));
    }

    /// RFC 9112 7.1: chunk-size = 1*HEXDIG. A bare CRLF has ZERO hex digits,
    /// which used to sum to a size of 0 and be read as the terminating chunk -
    /// so the rest of a body after an empty size line was silently dropped.
    #[test]
    fn an_empty_chunk_size_line_is_rejected() {
        let chunked = b"\r\nhello\r\n0\r\n\r\n";
        let mut cr = ChunkedReader::new(ByteReader(chunked));
        assert!(
            matches!(
                cr.read(&mut [0u8; 4]),
                Err(e) if e.kind() == io::ErrorKind::InvalidData
            ),
            "a bare CRLF is not a zero-sized chunk"
        );
        // The legitimate terminator still ends the body.
        let mut cr = ChunkedReader::new(ByteReader(b"0\r\n\r\n"));
        assert!(drain(&mut cr).is_empty());
    }

    #[test]
    fn framing_prefers_transfer_encoding() {
        assert_eq!(framing(Some(5), Some(b"chunked")), Framing::Chunked);
        assert_eq!(framing(Some(5), None), Framing::Length(5));
        assert_eq!(framing(None, None), Framing::Length(0));
    }

    /// RFC 9112 7.1: the last-chunk is followed by `trailer-section CRLF`, and
    /// the trailer section ends with an EMPTY line, i.e. exactly `CRLF`. A line
    /// of length two measures the same whether or not its first byte was `CR`,
    /// so `X\n` was read as the terminator and the body reported complete.
    #[test]
    fn a_bare_lf_trailer_terminator_is_not_completion() {
        let mut cr = ChunkedReader::new(ByteReader(b"0\r\nX\n"));
        let mut buf = [0u8; 16];
        // Drive to exhaustion: the only honest outcomes are an error. Without
        // the fix the LF at index 4 was read as the terminator and this read
        // returned Ok(0) from a Done state, claiming a properly ended body.
        loop {
            match cr.read(&mut buf) {
                Ok(0) => panic!("a bare LF must not complete the body"),
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(
            !cr.is_message_complete(),
            "b\"0\\r\\nX\\n\" must never report a complete body"
        );
    }

    /// A trailer line that happens to be two bytes long is NOT the terminator.
    #[test]
    fn a_two_byte_trailer_line_is_not_completion() {
        let chunked = b"0\r\nX: y\r\n\r\n";
        let mut cr = ChunkedReader::new(ByteReader(chunked));
        assert_eq!(drain(&mut cr), b"");
        assert!(cr.is_message_complete());
    }

    /// The strict terminator still completes and the post-completion read is a
    /// no-op (the `Done` state's reason for existing).
    #[test]
    fn the_crlf_terminator_still_completes() {
        let mut cr = ChunkedReader::new(ByteReader(b"0\r\n\r\n"));
        assert_eq!(cr.read(&mut [0u8; 16]).expect("complete"), 0);
        assert!(cr.is_message_complete());
        assert_eq!(cr.read(&mut [0u8; 16]).expect("done no-op"), 0);
    }
}
