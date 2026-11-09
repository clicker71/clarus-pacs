// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Streaming multipart/related parser (E-007) - the biggest correctness risk.
//!
//! Never buffers a part in full (instances can be 500 MB). The parser owns a
//! single read buffer and a small hold window; `next_event(&mut self)` returns
//! events whose body slices borrow from the parser itself (`&mut self`), so
//! there is no copy of part data.
//!
//! Boundary grammar (RFC 2046 Section 5.1.1): a delimiter is `CRLF -- boundary`, the
//! closing delimiter appends `--`. A boundary starts at a line start, so the
//! hold window keeps enough tail bytes to catch a delimiter split across two
//! `read()`s.

use std::io::{self, Read};

use crate::http::conn::READ_BUF;

/// Maximum essence Content-Type length retained (application/dicom is 18).
const ESSENCE_LEN: usize = 32;
/// Hard cap for the total part-header block.
const HDR_CAP: usize = 4096;

/// A part header block, parsed from the part's header section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PartHeader {
    /// Copied Content-Type of the part (value is a copy - the source buffer
    /// is reused by later reads).
    pub content_type: [u8; ESSENCE_LEN],
    /// Length of the valid prefix of `content_type`.
    pub ct_len: usize,
}

impl PartHeader {
    pub fn content_type(&self) -> &[u8] {
        &self.content_type[..self.ct_len]
    }
}

/// Parser events. `Data` slices borrow from the parser (valid until the next
/// `next_event` call).
#[derive(Debug)]
pub enum MultipartEvent<'a> {
    /// A new part begins, with its parsed headers.
    Part(PartHeader),
    /// A chunk of the current part's body.
    Data(&'a [u8]),
    /// The closing delimiter - no further parts.
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Preamble,
    PartHead,
    Body,
    Done,
}

/// Streaming multipart/related parser over a `Read` source.
pub struct MultipartParser<R: Read> {
    body: R,
    /// The separator line prefix `\r\n--boundary`.
    delim: Vec<u8>,
    state: State,
    /// Deferred delimiter action to apply on the next `next_event` call (the
    /// current call already emitted the body data preceding it).
    pending: Pending,
    /// Hold window for boundary-split detection.
    hold: Vec<u8>,
    /// Accumulated part headers (until the blank line).
    hdr: Vec<u8>,
    /// Pending body data produced by the last fill.
    buf: [u8; READ_BUF],
    /// True once the underlying body reader returned 0 (clean EOF).
    eof: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    None,
    /// The last emitted data was followed by a non-closing boundary: next
    /// iteration reads the following part's headers.
    NextPart,
    /// The last emitted data was followed by the closing boundary: next
    /// iteration reports `End`.
    Closing,
}

impl<R: Read> MultipartParser<R> {
    /// Create a parser for the given boundary (without leading `--` or CRLF).
    pub fn new(body: R, boundary: &[u8]) -> Result<Self, String> {
        if boundary.is_empty() || boundary.len() > 70 {
            return Err(format!(
                "multipart boundary length {} out of range [1..70]",
                boundary.len()
            ));
        }
        let mut delim = Vec::with_capacity(boundary.len() + 2);
        delim.extend_from_slice(b"\r\n--");
        delim.extend_from_slice(boundary);
        Ok(Self {
            body,
            delim,
            state: State::Preamble,
            pending: Pending::None,
            hold: Vec::new(),
            hdr: Vec::new(),
            buf: [0u8; READ_BUF],
            eof: false,
        })
    }

    /// Fetch the next event. Blocks on the underlying `Read`.
    pub fn next_event(&mut self) -> io::Result<MultipartEvent<'_>> {
        // Apply a deferred delimiter action from the previous call.
        match self.pending {
            Pending::Closing => {
                self.pending = Pending::None;
                self.state = State::Done;
                return Ok(MultipartEvent::End);
            }
            Pending::NextPart => {
                self.pending = Pending::None;
                self.state = State::PartHead;
                self.hdr.clear();
            }
            Pending::None => {}
        }
        loop {
            match self.state {
                State::Done => return Ok(MultipartEvent::End),
                State::Preamble => {
                    let n = self.fill()?;
                    if n == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "multipart: EOF before first boundary",
                        ));
                    }
                    if let Some(idx) = self.find_first_boundary(&self.buf[..n]) {
                        // The first boundary is `--boundary` (no CRLF prefix);
                        // it ends at idx + 2 + boundary.len().
                        let boundary_len = self.delim.len() - 4;
                        let after = idx + 2 + boundary_len;
                        self.state = State::PartHead;
                        self.consume_tail_into(n, after);
                        continue;
                    }
                    // Every available byte is now in scope; with EOF no more
                    // bytes will ever form a boundary, so stop instead of
                    // re-scanning the same tail forever.
                    if self.eof {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "multipart: EOF before first boundary",
                        ));
                    }
                    self.stash_hold(n, self.delim.len() + 2);
                }
                State::PartHead => {
                    if let Some(h) = self.read_part_headers()? {
                        return Ok(MultipartEvent::Part(h));
                    }
                }
                State::Body => {
                    let n = self.fill()?;
                    if n == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "multipart: EOF before closing delimiter",
                        ));
                    }
                    match self.find_delimiter(&self.buf[..n]) {
                        Some((end_idx, closing)) => {
                            if end_idx == 0 {
                                // Delimiter at the very start: no data to emit.
                                if closing {
                                    self.state = State::Done;
                                    return Ok(MultipartEvent::End);
                                }
                                self.state = State::PartHead;
                                self.hdr.clear();
                                self.consume_tail_into(n, self.delim.len());
                                continue;
                            }
                            // Emit the data before the delimiter; defer the
                            // delimiter action to the next call. Keep whatever
                            // follows the delimiter (boundary CRLF + next part
                            // headers) in the hold window. For a closing
                            // delimiter the trailing bytes are irrelevant.
                            self.consume_tail_into(n, end_idx + self.delim.len());
                            self.pending = if closing {
                                Pending::Closing
                            } else {
                                Pending::NextPart
                            };
                            return Ok(MultipartEvent::Data(&self.buf[..end_idx]));
                        }
                        None => {
                            // Hold back the delimiter plus its 2-byte closing/
                            // separator lookahead so a boundary that straddles
                            // a read boundary is never split or misclassified.
                            let carry = self.delim.len() + 2;
                            let emit = n.saturating_sub(carry);
                            if emit > 0 {
                                self.stash_hold(n, carry);
                                return Ok(MultipartEvent::Data(&self.buf[..emit]));
                            }
                            // emit == 0: everything is potential boundary
                            // prefix. Terminate on EOF instead of looping.
                            if self.eof {
                                return Err(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "multipart: EOF before closing delimiter",
                                ));
                            }
                            self.stash_hold(n, carry);
                        }
                    }
                }
            }
        }
    }

    /// Fill `self.buf`: put held tail bytes first, then top up from the body
    /// so scanning always makes forward progress. Returns the number of bytes
    /// available (0 only when there is nothing left and no new body data).
    fn fill(&mut self) -> io::Result<usize> {
        let mut used = 0usize;
        if !self.hold.is_empty() {
            let n = self.hold.len().min(self.buf.len());
            self.buf[..n].copy_from_slice(&self.hold[..n]);
            self.hold.drain(..n);
            used = n;
        }
        if used < self.buf.len() && !self.eof {
            let n = self.body.read(&mut self.buf[used..])?;
            if n == 0 {
                self.eof = true;
            } else {
                used += n;
            }
        }
        Ok(used)
    }

    /// Keep the last `keep` bytes of `self.buf[..n]` in the hold window so a
    /// boundary that straddles a read is never lost.
    fn stash_hold(&mut self, n: usize, keep: usize) {
        let keep = keep.min(n);
        self.hold.clear();
        if keep > 0 {
            let start = n - keep;
            self.hold.extend_from_slice(&self.buf[start..n]);
        }
    }

    /// After consuming a delimiter, keep the bytes that follow it for the
    /// header/body reader.
    fn consume_tail_into(&mut self, n: usize, after: usize) {
        let buf = &self.buf[..n];
        self.hold.clear();
        if after < buf.len() {
            self.hold.extend_from_slice(&buf[after..]);
        }
    }

    /// Find the first `--boundary` (preamble variant, no CRLF prefix).
    fn find_first_boundary(&self, buf: &[u8]) -> Option<usize> {
        // delim = \r\n -- boundary ; the boundary text is delim[4..].
        let boundary = &self.delim[4..];
        let mut i = 0usize;
        while i + 2 + boundary.len() <= buf.len() {
            if &buf[i..i + 2] == b"--" && &buf[i + 2..i + 2 + boundary.len()] == boundary {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Find the full delimiter in `buf` (line-start `\r\n--boundary`).
    /// Returns `(index_of_CR, closing)`. `closing` true when `--boundary--`.
    ///
    /// A match is only reported when enough lookahead is available to decide
    /// between a closing `--` and a part-separating CRLF. Without it (the
    /// delimiter at the very end of the buffer) we defer: the caller's hold
    /// window re-examines it once more bytes arrive.
    fn find_delimiter(&self, buf: &[u8]) -> Option<(usize, bool)> {
        let d = &self.delim[..];
        if buf.len() < d.len() {
            return None;
        }
        let mut i = 0usize;
        while i + d.len() <= buf.len() {
            if &buf[i..i + d.len()] == d {
                let bstart = i + d.len();
                // Need at least 2 bytes of lookahead to judge closing `--`
                // vs. the separator CRLF.
                if buf.len() < bstart + 2 {
                    // Defer: we cannot tell yet. The caller keeps the window
                    // and re-examines it when more bytes arrive.
                    return None;
                }
                let closing = buf[bstart..].starts_with(b"--");
                // A real delimiter is followed by `--` or `\r\n`; anything
                // else is not a delimiter (could be body bytes that merely
                // begin with the boundary prefix).
                let sep = buf[bstart..].starts_with(b"\r\n");
                if closing || sep {
                    return Some((i, closing));
                }
                // A boundary-prefixed run INSIDE the body: keep scanning. This
                // used to `return None`, which told the caller the delimiter
                // had not arrived yet - so the real delimiter further on was
                // never found and the following part merged into this one (or
                // the part was truncated) whenever the payload happened to
                // contain `\r\n--<boundary>` followed by anything else.
                i += d.len();
                continue;
            }
            i += 1;
        }
        None
    }

    /// Read part headers until the blank line, returning the parsed header.
    fn read_part_headers(&mut self) -> io::Result<Option<PartHeader>> {
        loop {
            if let Some(p) = self.hdr.windows(4).position(|w| w == b"\r\n\r\n") {
                let after = p + 4;
                let body_tail = self.hdr.split_off(after);
                self.hold.clear();
                self.hold.extend_from_slice(&body_tail);
                self.hdr.truncate(p);
                let ct = parse_essence_content_type(&self.hdr);
                self.state = State::Body;
                let mu = ct.len().min(ESSENCE_LEN);
                let mut content_type = [0u8; ESSENCE_LEN];
                content_type[..mu].copy_from_slice(&ct[..mu]);
                return Ok(Some(PartHeader {
                    content_type,
                    ct_len: mu,
                }));
            }
            if self.hdr.len() > HDR_CAP {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "multipart part headers too large",
                ));
            }
            // Copy EVERYTHING `fill` made available. Capping the copy at a
            // fixed scratch size would silently drop body bytes that arrived
            // in the same read as the part headers (a streaming-only bug).
            // The size guard is `HDR_CAP` above, checked before appending.
            let n = self.fill()?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "multipart: EOF in part headers",
                ));
            }
            self.hdr.extend_from_slice(&self.buf[..n]);
        }
    }
}

/// Whether the bytes at `pos` start a `Content-Type` field followed by a colon.
///
/// Two details decide correctness, and a plain byte scan gets both wrong:
///
/// - The field name is CASE-INSENSITIVE (RFC 9110 5.1). A MIME part whose
///   header was spelled `content-type:` produced an empty essence, so the
///   part was treated as having no media type at all.
/// - The match must start a FIELD: at the beginning of the block, or right
///   after a CRLF. Scanning for the bytes anywhere let a header VALUE that
///   merely contained `Content-Type:` - a `Content-Disposition` filename, an
///   `X-Note` - supply the essence for the part.
///
/// Returns the offset just past the colon.
fn content_type_at(hdr: &[u8], pos: usize) -> Option<usize> {
    const NAME: &[u8] = b"content-type";
    if pos != 0 && !hdr[..pos].ends_with(b"\r\n") {
        return None;
    }
    let after = pos + NAME.len();
    if after > hdr.len() || !hdr[pos..after].eq_ignore_ascii_case(NAME) {
        return None;
    }
    (hdr.get(after) == Some(&b':')).then_some(after + 1)
}

/// Extract the essence Content-Type value from a header block (copied out -
/// the caller keeps a copy because `hdr` is reused).
fn parse_essence_content_type(hdr: &[u8]) -> Vec<u8> {
    let mut i = 0usize;
    while i < hdr.len() {
        if let Some(vstart) = content_type_at(hdr, i) {
            let mut vend = vstart;
            while vend < hdr.len() && hdr[vend] != b'\r' {
                vend += 1;
            }
            let mut v = &hdr[vstart..vend];
            while v.first() == Some(&b' ') || v.first() == Some(&b'\t') {
                v = &v[1..];
            }
            while v.last() == Some(&b' ') || v.last() == Some(&b'\t') {
                v = &v[..v.len() - 1];
            }
            let essence = match v.iter().position(|&b| b == b';') {
                Some(se) => &v[..se],
                None => v,
            };
            return essence.trim_ascii().to_vec();
        }
        i += 1;
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The part's `Content-Type` field name is case-insensitive (RFC 9110 5.1)
    /// and must start a field. The old byte scan got both wrong: a lowercase
    /// spelling produced an EMPTY essence (the part looked like it had no media
    /// type), and a `Content-Type:` inside another header's VALUE supplied the
    /// essence for the part.
    #[test]
    fn the_part_content_type_is_matched_case_insensitively_and_at_field_start() {
        let essence =
            |hdr: &[u8]| String::from_utf8_lossy(&parse_essence_content_type(hdr)).into_owned();

        // Lowercase and mixed-case field names are still the field.
        assert_eq!(essence(b"content-type: image/jp2"), "image/jp2");
        assert_eq!(essence(b"CONTENT-TYPE: image/jp2"), "image/jp2");
        assert_eq!(essence(b"Content-Type:image/jp2"), "image/jp2");
        // Parameters are dropped, surrounding whitespace is trimmed.
        assert_eq!(
            essence(b"Content-Type: application/octet-stream; transfer-syntax=1.2.3"),
            "application/octet-stream"
        );
        // A match inside a header VALUE is not the field.
        assert_eq!(
            essence(b"Content-Disposition: form-data; filename=\"Content-Type: fake/type\"\r\n"),
            ""
        );
        // ...while a real field after other headers is found.
        assert_eq!(
            essence(b"Content-Disposition: form-data\r\nContent-Type: image/jpeg\r\n"),
            "image/jpeg"
        );
        // A prefix that is not the field name does not match.
        assert_eq!(essence(b"X-Content-Type-Options: nosniff"), "");
    }

    fn collect(body: &[u8], boundary: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut p = MultipartParser::new(body, boundary).expect("parser");
        let mut out = Vec::new();
        let mut cur: Option<PartHeader> = None;
        let mut data = Vec::new();
        loop {
            match p.next_event().expect("event") {
                MultipartEvent::Part(h) => {
                    if let Some(prev) = cur.take() {
                        out.push((prev.content_type().to_vec(), std::mem::take(&mut data)));
                    }
                    cur = Some(h);
                }
                MultipartEvent::Data(d) => data.extend_from_slice(d),
                MultipartEvent::End => break,
            }
        }
        if let Some(prev) = cur.take() {
            out.push((prev.content_type().to_vec(), std::mem::take(&mut data)));
        }
        out
    }

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

    /// A reader that serves data in large chunks (like a socket with a full
    /// kernel buffer), up to `max` bytes per call.
    struct ChunkReader<'a> {
        data: &'a [u8],
        max: usize,
    }
    impl Read for ChunkReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.data.is_empty() {
                return Ok(0);
            }
            let take = self.data.len().min(buf.len()).min(self.max);
            buf[..take].copy_from_slice(&self.data[..take]);
            self.data = &self.data[take..];
            Ok(take)
        }
    }

    #[test]
    fn parses_single_part() {
        let body = b"--b\r\nContent-Type: application/dicom\r\n\r\nABCD\r\n--b--\r\n";
        let out = collect(body, b"b");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, b"application/dicom");
        assert_eq!(out[0].1, b"ABCD");
    }

    #[test]
    fn boundary_split_across_reads() {
        let full = b"--b\r\nContent-Type: application/dicom\r\n\r\nDATA\r\n--b--\r\n";
        let out = collect_reader(ByteReader(full), b"b");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, b"DATA");
    }

    #[test]
    fn part_body_larger_than_header_scratch_survives_chunky_reads() {
        // Regression: `read_part_headers` used to cap the bytes it moved out
        // of `fill()` at 256 (the scratch size), silently dropping part-body
        // bytes that arrived in the same read as the part headers. A chunked
        // reader exposes this: fill() returns many KB at once.
        let mut body = Vec::new();
        body.extend_from_slice(b"--b\r\nContent-Type: application/dicom\r\n\r\n");
        body.extend_from_slice(&vec![0x5Au8; 4096]);
        body.extend_from_slice(b"\r\n--b--\r\n");
        let out = collect_reader(
            ChunkReader {
                data: &body,
                max: 4096,
            },
            b"b",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.len(), 4096, "part body must not be truncated");
        assert!(out[0].1.iter().all(|&b| b == 0x5A));
    }

    #[test]
    fn exact_byte_roundtrip_with_big_reads() {
        // Mirrors the live STOW wire layout: 5-char boundary, ~8.8 KB part
        // body bracketed by the closing delimiter, delivered in ~8 KB chunks.
        // The collected part body must be byte-exact (a live diagnosis showed
        // the temp file gaining one stray byte at each end).
        let mut body = Vec::new();
        body.extend_from_slice(b"--BOUND\r\nContent-Type: application/dicom\r\n\r\n");
        body.extend_from_slice(&vec![0x00u8; 8830]);
        body.extend_from_slice(b"\r\n--BOUND--\r\n");
        let out = collect_reader(
            ChunkReader {
                data: &body,
                max: 8192,
            },
            b"BOUND",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.len(), 8830, "byte-exact roundtrip");
        assert!(out[0].1.iter().all(|&b| b == 0x00));
    }

    fn collect_reader(body: impl Read, boundary: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut p = MultipartParser::new(body, boundary).expect("parser");
        let mut out = Vec::new();
        let mut cur: Option<PartHeader> = None;
        let mut data = Vec::new();
        loop {
            match p.next_event().expect("event") {
                MultipartEvent::Part(h) => {
                    if let Some(prev) = cur.take() {
                        out.push((prev.content_type().to_vec(), std::mem::take(&mut data)));
                    }
                    cur = Some(h);
                }
                MultipartEvent::Data(d) => data.extend_from_slice(d),
                MultipartEvent::End => break,
            }
        }
        if let Some(prev) = cur.take() {
            out.push((prev.content_type().to_vec(), std::mem::take(&mut data)));
        }
        out
    }

    #[test]
    fn rejects_oversize_boundary() {
        assert!(MultipartParser::new(&b""[..], &[b'x'; 80]).is_err());
    }

    #[test]
    fn multiple_parts() {
        let body = b"--b\r\nContent-Type: application/dicom\r\n\r\nA\r\n--b\r\nContent-Type: application/octet-stream\r\n\r\nB\r\n--b--\r\n";
        let out = collect(body, b"b");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].1, b"A");
        assert_eq!(out[1].1, b"B");
        assert_eq!(out[1].0, b"application/octet-stream");
    }
}
