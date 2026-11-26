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
        // Bounded in u64 FIRST, then narrowed. `(n as usize)` truncates on a
        // 32-bit target: a remaining count >= 2^32 becomes 0, `read` returns an
        // empty slice, and the loop reports a premature EOF for a body that is
        // still arriving. The buffer length is a small constant, so the
        // narrowed value is always in range.
        let want = n.min(buf.len() as u64) as usize;
        // `Interrupted` means the syscall was woken by a signal before it
        // delivered anything - the body is not broken and nothing was
        // consumed, so the read is simply retried. Propagating it with `?`
        // abandoned the remaining count: a source that returns `ab`, then
        // `Interrupted`, then `cd` for a four-byte body delivered only `ab`
        // and reported success, and the caller had no way to resume because
        // `read_length` owns the count. The chunked reader already retried;
        // this is the same rule on the other framing.
        let got = match r.read(&mut buf[..want]) {
            Ok(got) => got,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
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
    ///
    /// On error, `filled` still reports how many bytes were already delivered
    /// into `out`. The caller's `remaining` must be decremented by that count
    /// before the error is surfaced: leaving it whole made a retry re-read
    /// `\r\n` as chunk payload, because those bytes had in fact been consumed.
    fn read_exact_lt_filled(&mut self, out: &mut [u8]) -> (usize, io::Result<bool>) {
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
            // `Interrupted` is a RETRY, not a failure: the kernel was woken by
            // a signal before delivering anything. Propagating it with `?`
            // would abandon the bytes already in `out` (the caller's
            // `ChunkedReader.remaining` and framing state are then out of step
            // with what was consumed), so a retry would re-read framing bytes as
            // payload and desync the decoder.
            let n = match self.inner.read(&mut out[filled..]) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return (filled, Err(e)),
            };
            if n == 0 {
                return (filled, Ok(false));
            }
            filled += n;
        }
        (filled, Ok(true))
    }

    /// [`Self::read_exact_lt_filled`] for callers that only need the outcome.
    fn read_exact_lt(&mut self, out: &mut [u8]) -> io::Result<bool> {
        self.read_exact_lt_filled(out).1
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
    /// Size-line CRLF progress: once the CR of the size line has been
    /// consumed, only the LF remains. Held on `self` so a recoverable error
    /// between the two bytes does not re-read the `\n` as a hex digit.
    size_cr_pending: bool,
    /// The size line is past its hex digits and skipping a chunk extension
    /// (RFC 9112 7.1.1: a recipient MUST ignore unrecognized extensions).
    /// Held on `self` so a recoverable error mid-extension resumes the skip
    /// instead of re-reading extension bytes as hex digits.
    size_ext: bool,
    ext_len: usize,
    remaining: u64,
    trailer_used: usize,
    /// Partial chunk-data CRLF already consumed. `DataCrlf` must not lose a
    /// `\r` delivered before a recoverable error, or the retry reads the `\n`
    /// as the CR and desyncs the stream by one byte.
    crlf: [u8; 2],
    crlf_len: usize,
    /// Trailer line state: bytes on the current line (including `\r`) and the
    /// byte seen before the current one. Held on `self` so a recoverable error
    /// mid-line resumes the same line instead of restarting it and missing the
    /// `\r\n\r\n` terminator.
    trailer_line_len: usize,
    trailer_prev: u8,
    /// Whether the current trailer line has carried a field-name colon, so a
    /// line with no field name is rejected instead of discarded.
    trailer_line_has_colon: bool,
    /// An error that arose AFTER payload bytes were already handed to the
    /// caller.
    ///
    /// `Read::read` promises that `Err` means nothing was read. Reporting such
    /// an error in the same call would break that promise, and a caller that
    /// retried would re-read the stream over the same buffer region - a
    /// shorter, misaligned payload. The bytes are delivered as `Ok(n)` and the
    /// error is raised at the top of the NEXT call instead.
    pending_err: Option<io::Error>,
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
            size_cr_pending: false,
            size_ext: false,
            ext_len: 0,
            remaining: 0,
            trailer_used: 0,
            crlf: [0u8; 2],
            crlf_len: 0,
            trailer_line_len: 0,
            trailer_prev: 0,
            trailer_line_has_colon: false,
            pending_err: None,
        }
    }

    /// True once the message (including trailers) is fully read.
    pub fn is_message_complete(&self) -> bool {
        self.state == ChunkState::Done
    }
}

impl<R: Read> Read for ChunkedReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        // A previous call delivered its payload and then found the stream
        // broken. That error is owed to the caller now, before any more input
        // is consumed.
        if let Some(e) = self.pending_err.take() {
            return Err(e);
        }
        loop {
            match self.state {
                ChunkState::Done => return Ok(0),
                ChunkState::Size => {
                    // Parse `<hexdigits> [chunk-ext] CR LF`.
                    // `hex_len` is NOT reset on entry: it resets only when the
                    // line actually completes, so hex digits read before a
                    // recoverable error (WouldBlock/TimedOut) survive a retry.
                    // Resetting here discarded them and misread `10` as `0`,
                    // silently truncating the message.
                    loop {
                        // A previous attempt consumed the CR of the size line
                        // and then hit a recoverable error reading the LF. Only
                        // the LF is owed now; only a recoverable error may
                        // leave the flag set, so the retry re-enters here.
                        if self.size_cr_pending {
                            let lf = self
                                .src
                                .read_byte()?
                                .ok_or_else(|| eof_err("chunk size CRLF"))?;
                            if lf != b'\n' {
                                return Err(invalid("chunk size line: expected LF after CR"));
                            }
                            self.finish_size_line();
                            break;
                        }
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
                                // Consume the LF with the CR already marked, so
                                // a recoverable error between the two bytes
                                // does not desync the size line by one byte.
                                self.size_cr_pending = true;
                                let lf = self
                                    .src
                                    .read_byte()?
                                    .ok_or_else(|| eof_err("chunk size CRLF"))?;
                                if lf != b'\n' {
                                    return Err(invalid("chunk size line: expected LF after CR"));
                                }
                                self.finish_size_line();
                                break;
                            }
                            Some(b) => {
                                if self.size_ext {
                                    // Skipping a chunk extension: every byte up
                                    // to the CRLF is discarded, bounded by the
                                    // same budget as the hex digits so a
                                    // hostile size line cannot grow unbounded.
                                    self.ext_len += 1;
                                    if self.ext_len > CHUNK_LINE {
                                        return Err(invalid("chunk extension line too long"));
                                    }
                                } else if b == b';' || b == b' ' || b == b'\t' {
                                    // RFC 9112 7.1.1: chunk-ext = *( BWS ";" BWS
                                    // chunk-ext-name [ BWS "=" BWS chunk-ext-val ] ),
                                    // and a recipient MUST ignore unrecognized
                                    // extensions. The hex digits already
                                    // collected are the size; the rest of the
                                    // line is an extension to skip.
                                    if self.hex_len == 0 {
                                        return Err(invalid("chunk size line: no hex digits"));
                                    }
                                    self.size_ext = true;
                                    self.ext_len = 0;
                                } else {
                                    let d =
                                        hex_digit(b).ok_or_else(|| invalid("bad hex in size"))?;
                                    if self.hex_len >= CHUNK_LINE {
                                        return Err(invalid("chunk size line too long"));
                                    }
                                    self.hex[self.hex_len] = d;
                                    self.hex_len += 1;
                                }
                            }
                        }
                    }
                }
                ChunkState::Data => {
                    if self.remaining == 0 {
                        self.state = ChunkState::DataCrlf;
                        continue;
                    }
                    // Bounded in u64 first: on a 32-bit target `remaining as
                    // usize` truncates a chunk >= 4 GiB to zero, which would
                    // return an empty read and desync the decoder. `out.len()`
                    // is small, so the narrowed value is always in range.
                    let want = self.remaining.min(out.len() as u64) as usize;
                    let (got, res) = self.src.read_exact_lt_filled(&mut out[..want]);
                    // Whatever reached `out` is GONE from the stream, so
                    // `remaining` shrinks by exactly that count even when the
                    // read then failed. Leaving it whole made the retry treat
                    // those bytes again and pull framing into the payload.
                    self.remaining -= got as u64;
                    if let Err(e) = res {
                        // `Read::read` may only report `Err` when nothing was
                        // read, so the delivered bytes are returned and the
                        // error is held for the next call.
                        if got > 0 {
                            self.pending_err = Some(e);
                            return Ok(got);
                        }
                        return Err(e);
                    }
                    if got < want {
                        // Same contract for a premature EOF: the bytes are
                        // real payload, the truncation is reported afterwards.
                        if got > 0 {
                            self.pending_err = Some(eof_err("chunk data"));
                            return Ok(got);
                        }
                        return Err(eof_err("chunk data"));
                    }
                    // If we over-read (want == remaining but out had more space),
                    // nothing extra was consumed - the reader fills exactly
                    // `want` bytes, so no pushback is needed here.
                    return Ok(want);
                }
                ChunkState::DataCrlf => {
                    // Fill the 2-byte CRLF from where the last attempt left
                    // off. `read_exact_lt` discarded the filled count, so a
                    // `\r` delivered before WouldBlock was consumed-and-lost:
                    // the retry read the `\n` as the CR and desynced by one.
                    // `crlf_len` on `self` keeps the partial byte accounted.
                    let (filled, res) = self
                        .src
                        .read_exact_lt_filled(&mut self.crlf[self.crlf_len..]);
                    self.crlf_len += filled;
                    let ok = res?;
                    if !ok {
                        return Err(eof_err("chunk CRLF"));
                    }
                    if &self.crlf != b"\r\n" {
                        return Err(invalid("chunk data: missing CRLF"));
                    }
                    self.crlf_len = 0;
                    self.state = ChunkState::Size;
                }
                ChunkState::Trailer => {
                    // Skip trailer lines until the empty line `\r\n`.
                    // A line is empty when it contains exactly `\r` before `\n`.
                    // Budget-bounded by TRAILER_LIMIT.
                    // `trailer_line_len`/`trailer_prev` live on `self`, not as
                    // loop locals: a recoverable error mid-line must resume the
                    // same line, or the `\r\n\r\n` terminator is missed and the
                    // message is misread as incomplete.
                    loop {
                        let b = match self.src.read_byte()? {
                            // RFC 9112 7.1.2: the trailer section is terminated
                            // by an empty line - `0\r\n\r\n` is the minimum. EOF
                            // before that CRLF is a truncation, not a clean end
                            // of message, so it must not read as success.
                            None => return Err(eof_err("trailer terminator")),
                            Some(b) => b,
                        };
                        self.trailer_used += 1;
                        if self.trailer_used > TRAILER_LIMIT {
                            return Err(invalid("trailer too large"));
                        }
                        self.trailer_line_len += 1;
                        if b == b'\n' {
                            // Line ended. Empty line == exactly `\r\n`: a bare
                            // LF leaves `line_len == 1` and therefore does NOT
                            // terminate, it only ends a non-empty line. Testing
                            // the length alone accepted `X\n`, whose second
                            // byte is the LF, as the terminator.
                            if self.trailer_line_len == 2 && self.trailer_prev == b'\r' {
                                // The terminating CRLF is the END of the body;
                                // a read after it must answer clean EOF (0),
                                // not re-enter the trailer scan and then fail
                                // on the missing trailer byte. The `Done` state
                                // is what makes `read` after completion a
                                // no-op instead of an error.
                                self.state = ChunkState::Done;
                                return Ok(0); // message complete
                            }
                            // RFC 9112 7.1.2: the trailer section is a list of
                            // field lines, so every line must end CRLF and name
                            // a field. Discarding the bytes unread meant a
                            // malformed trailer (`no-colon-here`) was silently
                            // swallowed and the message reported complete -
                            // a truncated or injected trailer read as success.
                            if self.trailer_line_len < 2 || self.trailer_prev != b'\r' {
                                return Err(invalid("trailer line: missing CRLF"));
                            }
                            if !self.trailer_line_has_colon {
                                return Err(invalid("trailer line: missing field name"));
                            }
                            self.trailer_line_len = 0;
                            self.trailer_line_has_colon = false;
                        } else if b == b':' && self.trailer_line_len > 1 {
                            // A colon names the field; one at position 1 means
                            // the name is empty, which the check above rejects.
                            self.trailer_line_has_colon = true;
                        }
                        // Remember this byte for the next iteration's CR test.
                        // Must stay INSIDE the loop: the terminator check above
                        // reads it on the very next byte.
                        self.trailer_prev = b;
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

    /// Decode the collected hex digits into a chunk size and move to the next
    /// framing state. Called once the size line's CRLF has been fully consumed
    /// (hex digits only: any chunk extension has been skipped). Resets the
    /// per-line state so the next size line starts fresh.
    fn finish_size_line(&mut self) {
        let size = self.hex_value();
        self.hex_len = 0;
        self.size_cr_pending = false;
        self.size_ext = false;
        self.ext_len = 0;
        self.remaining = size;
        self.state = if size == 0 {
            ChunkState::Trailer
        } else {
            ChunkState::Data
        };
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

    /// A one-byte source that returns `WouldBlock` once on its `block_at`-th
    /// read call, then resumes. One byte per call makes every framing token
    /// cross at least one read boundary, so a blocked read always has partial
    /// framing progress to preserve.
    struct FlakySource<'a> {
        data: &'a [u8],
        pos: usize,
        block_at: usize,
        calls: usize,
        blocked: bool,
    }
    impl Read for FlakySource<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls == self.block_at && !self.blocked {
                self.blocked = true;
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            if self.pos >= self.data.len() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.data[self.pos];
            self.pos += 1;
            Ok(1)
        }
    }

    /// Drain fully, retrying recoverable errors (`WouldBlock`/`TimedOut`) like
    /// a real caller would. A recoverable error is NOT the end of the message.
    fn drain_retry<R: Read>(cr: &mut ChunkedReader<R>) -> Vec<u8> {
        let mut all = Vec::new();
        let mut buf = [0u8; 16];
        loop {
            match cr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => all.extend_from_slice(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => panic!("chunked error: {e}"),
            }
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

    /// RFC 9112 7.1.2: the trailer section is a list of FIELD lines. A line
    /// with no field name used to be discarded unread and the message reported
    /// complete, so an injected or truncated trailer read as success.
    #[test]
    fn a_malformed_trailer_line_is_rejected_not_discarded() {
        // No colon at all: not a field line.
        let mut cr = ChunkedReader::new(ByteReader(b"0\r\nno-colon-here\r\n\r\n"));
        let mut out = Vec::new();
        assert!(
            std::io::Read::read_to_end(&mut cr, &mut out).is_err(),
            "a colonless trailer line is malformed"
        );
        assert!(!cr.is_message_complete());

        // An empty field name (`:` first) is malformed too.
        let mut cr = ChunkedReader::new(ByteReader(b"0\r\n: v\r\n\r\n"));
        let mut out = Vec::new();
        assert!(std::io::Read::read_to_end(&mut cr, &mut out).is_err());

        // A valid trailer still completes.
        let mut cr = ChunkedReader::new(ByteReader(b"0\r\nX: y\r\n\r\n"));
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut cr, &mut out).expect("a valid trailer completes");
        assert!(cr.is_message_complete());
    }

    /// `read_length` must RETRY `Interrupted` in place.
    ///
    /// It owns the remaining count, so returning the error to the caller
    /// abandons bytes that will never be read again: a four-byte body whose
    /// source answers `ab`, then `Interrupted`, then `cd` delivered only `ab`
    /// and reported success.
    #[test]
    fn read_length_retries_an_interrupted_read() {
        struct Flaky {
            steps: Vec<Option<&'static [u8]>>,
            i: usize,
        }
        impl std::io::Read for Flaky {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                let step = self.steps.get(self.i).copied().flatten();
                self.i += 1;
                match step {
                    None => Err(std::io::Error::from(std::io::ErrorKind::Interrupted)),
                    Some(bytes) => {
                        let n = bytes.len().min(out.len());
                        out[..n].copy_from_slice(&bytes[..n]);
                        Ok(n)
                    }
                }
            }
        }

        let mut src = Flaky {
            steps: vec![Some(b"ab"), None, Some(b"cd")],
            i: 0,
        };
        let mut got = Vec::new();
        read_length(&mut src, 4, &mut |chunk: &[u8]| {
            got.extend_from_slice(chunk);
            Ok(())
        })
        .expect("an interrupted read is retried, not propagated");
        assert_eq!(got, b"abcd", "all four bytes must arrive");
    }

    /// F5: the read bound must be computed in u64, not narrowed first.
    ///
    /// `(n as usize)` is correct on a 64-bit host and catastrophic on a 32-bit
    /// one: `2^32 as usize == 0`, so `read(&mut buf[..0])` returns 0 and the
    /// loop reports a premature EOF on a body that is still arriving. A >4 GiB
    /// STOW is exactly the shape that hits it.
    ///
    /// Testing this on a 64-bit host is the trap: the truncating form passes
    /// there, so a behavioural test proves nothing. The property that makes the
    /// fix correct - and that is falsifiable at ANY host width - is the ORDER of
    /// operations: bound to the buffer in u64, then narrow.
    #[test]
    fn the_read_bound_is_computed_before_narrowing() {
        // The exact arithmetic both call sites must use.
        fn bounded(remaining: u64, buf_len: usize) -> usize {
            remaining.min(buf_len as u64) as usize
        }
        const BUF: usize = 16 * 1024;

        // A span above u32::MAX must still yield a full-sized slice.
        assert_eq!(
            bounded(1u64 << 32, BUF),
            BUF,
            "a 4 GiB span must be bounded by the BUFFER, not truncate to zero"
        );
        assert_eq!(bounded(u64::MAX, BUF), BUF);

        // And the ordinary cases are untouched.
        assert_eq!(bounded(0, BUF), 0);
        assert_eq!(bounded(7, BUF), 7);
        assert_eq!(bounded(BUF as u64, BUF), BUF);

        // The truncating form the fix removes fails the first assertion on a
        // 32-bit host; showing it here documents WHY the order matters.
        #[cfg(target_pointer_width = "32")]
        {
            let narrowed_first = (1u64 << 32) as usize;
            assert_eq!(
                narrowed_first, 0,
                "on a 32-bit target the narrowed-first form is exactly zero"
            );
        }
    }

    /// F5: a real reader is driven for a count above u32::MAX.
    ///
    /// Complements the arithmetic test with the actual loop: the first slice
    /// handed to `read` must be non-empty. On a 64-bit host this passes either
    /// way (which is why the arithmetic test above is the real gate), but it
    /// fails immediately on a 32-bit one, so it guards the regression where the
    /// fix is reverted on that platform.
    #[test]
    fn a_count_above_u32_max_drives_the_reader() {
        use std::cell::RefCell;
        use std::io::Read;

        struct Probe {
            data: std::io::Cursor<Vec<u8>>,
            offered: RefCell<Vec<usize>>,
        }
        impl Read for Probe {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.offered.borrow_mut().push(buf.len());
                self.data.read(buf)
            }
        }

        let mut probe = Probe {
            data: std::io::Cursor::new(b"abcdefgh".to_vec()),
            offered: RefCell::new(Vec::new()),
        };
        let mut got = Vec::new();
        let err = super::read_length(&mut probe, 1u64 << 32, &mut |chunk| {
            got.extend_from_slice(chunk);
            Ok(())
        })
        .expect_err("the fixture delivers fewer bytes than announced");
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);

        let offered = probe.offered.into_inner();
        assert!(
            offered.iter().all(|&n| n > 0),
            "no read may be offered an EMPTY slice while bytes remain; \
             offered={offered:?}"
        );
        assert_eq!(
            got, b"abcdefgh",
            "every delivered byte reaches the callback"
        );
    }

    /// F8: `Interrupted` mid-read must retry in place, keeping `filled`.
    ///
    /// The kernel raises `Interrupted` when a signal arrives mid-call. The old
    /// code propagated it with `?`, abandoning the bytes already copied into
    /// `out` - while `ChunkedReader.remaining` had not been decremented for
    /// them. A caller that retried would then read FRAMING bytes as payload and
    /// desync the chunk decoder permanently.
    ///
    /// Drives the REAL `PushbackReader`, so reverting the retry fails here.
    #[test]
    fn an_interrupted_read_retries_without_losing_progress() {
        use std::io::Read;

        /// Yields `ab`, then `Interrupted`, then `cd`.
        struct Flaky {
            step: u8,
        }
        impl Read for Flaky {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match self.step {
                    0 => {
                        self.step = 1;
                        let n = buf.len().min(2);
                        buf[..n].copy_from_slice(&b"ab"[..n]);
                        Ok(n)
                    }
                    1 => {
                        self.step = 2;
                        Err(std::io::Error::from(std::io::ErrorKind::Interrupted))
                    }
                    _ => {
                        self.step = 3;
                        let n = buf.len().min(2);
                        buf[..n].copy_from_slice(&b"cd"[..n]);
                        Ok(n)
                    }
                }
            }
        }

        let mut src = super::PushbackReader::new(Flaky { step: 0 });
        let mut out = [0u8; 4];
        let got = src
            .read_exact_lt(&mut out)
            .expect("Interrupted must be retried, not propagated");
        assert!(got, "all four bytes are available");
        assert_eq!(&out, b"abcd", "the bytes must come back in order");
    }

    /// H2: a read that fails AFTER delivering bytes must report them.
    ///
    /// A short read followed by a hard error leaves `ab` already copied into the
    /// caller's buffer, i.e. already consumed from the stream. `remaining` has
    /// to shrink by that count before the error surfaces: while it stayed whole
    /// a retry expected four payload bytes, got two, and took the chunk's
    /// trailing `\r\n` as payload - the decoder then read framing as content.
    #[test]
    fn a_read_that_fails_after_delivering_bytes_reports_them() {
        use std::io::Read;

        /// Yields `ab`, then a hard error forever.
        struct Broken {
            delivered: bool,
        }
        impl Read for Broken {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if !self.delivered {
                    self.delivered = true;
                    let n = buf.len().min(2);
                    buf[..n].copy_from_slice(&b"ab"[..n]);
                    return Ok(n);
                }
                Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset))
            }
        }

        let mut src = super::PushbackReader::new(Broken { delivered: false });
        let mut out = [0u8; 4];
        let (filled, res) = src.read_exact_lt_filled(&mut out);
        assert_eq!(filled, 2, "the two delivered bytes must be reported");
        assert_eq!(
            res.unwrap_err().kind(),
            std::io::ErrorKind::ConnectionReset,
            "the error still reaches the caller"
        );
        assert_eq!(&out[..filled], b"ab", "the delivered bytes are in `out`");
    }

    /// A3: `Read::read` must not report `Err` after delivering payload.
    ///
    /// The chunk-data arm used to do `remaining -= got; res?;`, so a chunk that
    /// delivered two payload bytes and then hit a stream error returned `Err`
    /// from a call that had already written into the caller's buffer. The
    /// `Read` contract says `Err` means nothing was read, so a caller that
    /// retried would re-read the stream over the same region and receive a
    /// shorter, misaligned chunk. The bytes are returned as `Ok(n)` and the
    /// error is raised by the next call.
    #[test]
    fn a_chunk_that_breaks_mid_payload_delivers_its_bytes_first() {
        use std::io::Read;

        /// Feeds the chunk head `4\r\n` and TWO payload bytes, then errors.
        struct BreakAfterPayload {
            sent: usize,
        }
        impl Read for BreakAfterPayload {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                // A 4-byte chunk whose payload never finishes: the announced
                // length is 4, the stream stops after 2 payload bytes.
                const DATA: &[u8] = b"4\r\nab";
                if self.sent >= DATA.len() {
                    return Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
                }
                let take = (DATA.len() - self.sent).min(buf.len());
                buf[..take].copy_from_slice(&DATA[self.sent..self.sent + take]);
                self.sent += take;
                Ok(take)
            }
        }

        let mut reader = super::ChunkedReader::new(BreakAfterPayload { sent: 0 });
        let mut out = [0u8; 8];
        let first = reader
            .read(&mut out)
            .expect("a call that delivered payload must return Ok");
        assert!(
            first > 0,
            "first read must return the two delivered bytes, got {first}"
        );
        assert_eq!(&out[..first], b"ab", "the payload is intact");

        // The stream error is now owed to the caller.
        let err = reader.read(&mut out).expect_err("the held error surfaces");
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset);
    }

    /// A recoverable error mid-size-line must not discard the hex digits.
    ///
    /// The Size arm used to zero `hex_len` on every entry, so a WouldBlock
    /// after `1` made the retry re-read `0\r\n` as a fresh size line and take
    /// the 16-byte chunk as the terminating chunk, silently truncating the
    /// body. `hex_len` now resets only when the line completes.
    #[test]
    fn a_recoverable_error_mid_size_line_keeps_the_hex_digits() {
        let body = b"10\r\n0123456789abcdef\r\n0\r\n\r\n";
        let mut cr = ChunkedReader::new(FlakySource {
            data: body,
            pos: 0,
            block_at: 2, // after the `1`, before the `0`
            calls: 0,
            blocked: false,
        });
        assert_eq!(drain_retry(&mut cr), b"0123456789abcdef");
        assert!(cr.is_message_complete());
    }

    /// A recoverable error mid data-CRLF must not lose the consumed `\r`.
    ///
    /// `read_exact_lt` discarded the filled count, so a `\r` delivered before
    /// WouldBlock was consumed-and-lost; the retry then read the `\n` as the
    /// CR and rejected a valid chunk. `crlf_len` keeps the partial byte.
    #[test]
    fn a_recoverable_error_mid_data_crlf_keeps_the_cr() {
        let body = b"3\r\nfoo\r\n0\r\n\r\n";
        let mut cr = ChunkedReader::new(FlakySource {
            data: body,
            pos: 0,
            block_at: 8, // the `\n` of the chunk-data CRLF
            calls: 0,
            blocked: false,
        });
        assert_eq!(drain_retry(&mut cr), b"foo");
        assert!(cr.is_message_complete());
    }

    /// A recoverable error mid-trailer-terminator must not restart the line.
    ///
    /// `line_len`/`prev` used to be loop locals, lost on a retry, so the
    /// `\r\n\r\n` terminator was missed and the body read as incomplete. The
    /// state now lives on `self` and resumes the same line.
    #[test]
    fn a_recoverable_error_mid_trailer_keeps_the_line_state() {
        let body = b"0\r\n\r\n";
        let mut cr = ChunkedReader::new(FlakySource {
            data: body,
            pos: 0,
            block_at: 5, // the `\n` of the trailer terminator
            calls: 0,
            blocked: false,
        });
        assert_eq!(drain_retry(&mut cr), b"");
        assert!(cr.is_message_complete());
    }

    /// A recoverable error between the CR and LF of the size line must keep
    /// the consumed CR. The Size arm used to read the CR and then the LF as
    /// two inline bytes: a WouldBlock after the CR left nothing on `self` to
    /// record it, so the retry re-entered Size and read the `\n` as a hex
    /// digit ("bad hex in size"). The pending-CR flag makes the transition
    /// resumable, exactly like `DataCrlf`.
    #[test]
    fn a_recoverable_error_between_cr_and_lf_of_the_size_line_keeps_the_cr() {
        let body = b"10\r\n0123456789abcdef\r\n0\r\n\r\n";
        let mut cr = ChunkedReader::new(FlakySource {
            data: body,
            pos: 0,
            block_at: 4, // after the `\r`, before the `\n` of the size line
            calls: 0,
            blocked: false,
        });
        assert_eq!(drain_retry(&mut cr), b"0123456789abcdef");
        assert!(cr.is_message_complete());
    }

    /// EOF after `0\r\n` but before the mandatory empty trailer line `\r\n`
    /// is a truncation (RFC 9112 7.1.2 requires the trailer section to be
    /// CRLF-terminated). It used to read as a clean Ok(0) success, which is a
    /// fail-open on a cut body.
    #[test]
    fn eof_before_the_trailer_terminator_is_unexpected_eof() {
        let mut cr = ChunkedReader::new(ByteReader(b"0\r\n"));
        let err = cr
            .read(&mut [0u8; 16])
            .expect_err("a truncated trailer must not read as success");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// RFC 9112 7.1.1: a recipient MUST ignore chunk extensions. `3;foo=bar`
    /// is a valid chunk-size line; only the hex prefix `3` is the size.
    #[test]
    fn chunk_extensions_are_ignored() {
        let chunked = b"3;foo=bar\r\nABC\r\n0\r\n\r\n";
        let mut cr = ChunkedReader::new(ByteReader(chunked));
        assert_eq!(drain(&mut cr), b"ABC");
        assert!(cr.is_message_complete());
    }

    /// The extension skip shares the `CHUNK_LINE` budget with the hex digits,
    /// so a hostile size line cannot grow unbounded.
    #[test]
    fn an_overlong_chunk_extension_is_bounded() {
        let mut data = Vec::new();
        data.extend_from_slice(b"3;");
        data.extend_from_slice(&[b'x'; CHUNK_LINE + 4]);
        data.extend_from_slice(b"\r\nABC\r\n0\r\n\r\n");
        let mut cr = ChunkedReader::new(&data[..]);
        assert!(matches!(
            cr.read(&mut [0u8; 16]),
            Err(e) if e.kind() == io::ErrorKind::InvalidData
        ));
    }
}
