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
                        Some((end_idx, closing, pad)) => {
                            if end_idx == 0 {
                                // Delimiter at the very start: no data to emit.
                                if closing {
                                    self.state = State::Done;
                                    return Ok(MultipartEvent::End);
                                }
                                self.state = State::PartHead;
                                self.hdr.clear();
                                self.consume_tail_into(n, self.delim.len() + pad);
                                continue;
                            }
                            // Emit the data before the delimiter; defer the
                            // delimiter action to the next call. Keep whatever
                            // follows the delimiter (boundary CRLF + next part
                            // headers) in the hold window. For a closing
                            // delimiter the trailing bytes are irrelevant. The
                            // transport padding is skipped here too, or it
                            // would become the start of the next part's
                            // headers.
                            self.consume_tail_into(n, end_idx + self.delim.len() + pad);
                            self.pending = if closing {
                                Pending::Closing
                            } else {
                                Pending::NextPart
                            };
                            return Ok(MultipartEvent::Data(&self.buf[..end_idx]));
                        }
                        None => {
                            // No delimiter resolved in this window. Hold back
                            // from the last byte that could still START one,
                            // together with whatever transport padding already
                            // followed it: RFC 2046 Section 5.1.1 allows any
                            // amount of linear whitespace between the boundary
                            // text and the CRLF, so a fixed `delim.len() + 2`
                            // carry dropped padding that crossed the read and
                            // left the parts fused.
                            let carry = self.unresolved_delimiter_carry(&self.buf[..n]);
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
                            // The whole (short) window is still a live
                            // candidate. That is normal while the window is
                            // shorter than the lookahead (a one-byte reader,
                            // before the head has arrived); it is only a real
                            // stall once the window is FULL, because then no
                            // read can add the bytes that would resolve it.
                            if n < self.buf.len() {
                                self.stash_hold(n, carry);
                                continue;
                            }
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "multipart: boundary candidate exceeds the read buffer",
                            ));
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
        // Stage the held tail WITHOUT draining it: the drain is the commit
        // point and must not run before the body read has a definite outcome.
        // Draining first lost a boundary split across two reads when the
        // second read failed recoverably (Interrupted/WouldBlock/TimedOut):
        // the next fill saw an empty hold and overwrote the staged bytes.
        let staged = self.hold.len().min(self.buf.len());
        if staged > 0 {
            self.buf[..staged].copy_from_slice(&self.hold[..staged]);
        }
        let mut used = staged;
        if staged < self.buf.len() && !self.eof {
            // `Interrupted` is a retry signal, not a failure (the kernel was
            // woken by a signal before delivering anything), so retry it in
            // place. WouldBlock/TimedOut surface to the caller with `hold`
            // still intact, so the retry re-stages the same tail.
            let n = loop {
                match self.body.read(&mut self.buf[staged..]) {
                    Ok(n) => break n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            };
            if n == 0 {
                self.eof = true;
            } else {
                used += n;
            }
        }
        self.hold.drain(..staged);
        Ok(used)
    }

    /// How many trailing bytes of `buf` must survive the next read.
    ///
    /// Two things can still turn into a delimiter: a partial one that begins in
    /// the final `delim.len() - 1` bytes, and a COMPLETE boundary whose CRLF
    /// has not arrived yet. In the second case the transport padding after the
    /// boundary text belongs to the delimiter - RFC 2046 Section 5.1.1 allows
    /// any amount - so it must be held too. Skipping that left a fixed two-byte
    /// lookahead, which emitted the padding as body and fused the parts.
    ///
    /// A candidate whose suffix is already known-INVALID (`--boundaryX...`,
    /// suffix neither `--` nor `\r\n`) can never become a delimiter, so it is
    /// never retained: retaining it pinned the carry to the whole window and
    /// forced a full rescan on every one-byte read until the window filled.
    fn unresolved_delimiter_carry(&self, buf: &[u8]) -> usize {
        let len = buf.len();
        // A delimiter needs its `--`, its boundary text, and at least the two
        // bytes that decide closing `--` vs the separator CRLF.
        let lookahead = self.delim.len().saturating_add(2);
        let mut carry = len.min(lookahead);
        for i in 0..len {
            if !buf[i..].starts_with(&self.delim) {
                continue;
            }
            let after = i + self.delim.len();
            let pad = Self::transport_padding_len(buf, after);
            let bstart = after + pad;
            // A rejected suffix (`--boundaryX...`) is body, not a delimiter,
            // so it must not pin the window open.
            let decidable = buf.len() >= bstart + 2;
            let rejectable = decidable
                && !buf[bstart..].starts_with(b"--")
                && !buf[bstart..].starts_with(b"\r\n");
            if rejectable {
                continue;
            }
            carry = carry.max(len - i);
        }
        carry
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
    ///
    /// RFC 2046 Section 5.1.1 defines a delimiter as a line that STARTS with
    /// `--boundary`, so the preamble's own text is allowed to contain the
    /// boundary bytes anywhere except at a line start. Matching them mid-line
    /// split a preamble of, say, `note--b` into a spurious empty part and
    /// shifted every following part by one.
    fn find_first_boundary(&self, buf: &[u8]) -> Option<usize> {
        // delim = \r\n -- boundary ; the boundary text is delim[4..].
        let boundary = &self.delim[4..];
        let mut i = 0usize;
        while i + 2 + boundary.len() <= buf.len() {
            // Only input start or right after CRLF is a line start.
            let at_line_start = i == 0 || (i >= 2 && &buf[i - 2..i] == b"\r\n");
            if at_line_start
                && &buf[i..i + 2] == b"--"
                && &buf[i + 2..i + 2 + boundary.len()] == boundary
            {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Length of the transport padding that may follow a boundary.
    ///
    /// RFC 2046 Section 5.1.1 allows linear whitespace after the boundary
    /// text and before the terminating CRLF. Without skipping it a separator
    /// written as `\r\n--b \t\r\n` was not recognised, and the two parts
    /// around it fused into one.
    fn transport_padding_len(buf: &[u8], from: usize) -> usize {
        let mut n = 0usize;
        while let Some(b) = buf.get(from + n) {
            if *b == b' ' || *b == b'\t' {
                n += 1;
            } else {
                break;
            }
        }
        n
    }

    /// Find the full delimiter in `buf` (line-start `\r\n--boundary`).
    /// Returns `(index_of_CR, closing, padding)`. `closing` true when
    /// `--boundary--`; `padding` is the transport whitespace after the
    /// boundary text.
    ///
    /// A match is only reported when enough lookahead is available to decide
    /// between a closing `--` and a part-separating CRLF. Without it (the
    /// delimiter at the very end of the buffer) we defer: the caller's hold
    /// window re-examines it once more bytes arrive.
    ///
    /// Transport padding (spaces/tabs) is permitted between the boundary text
    /// and whatever follows it (RFC 2046 Section 5.1.1), so it is skipped
    /// before that decision rather than being read as body content. Its LENGTH
    /// is returned so the caller can skip those bytes too when it resumes at
    /// the next part's headers.
    fn find_delimiter(&self, buf: &[u8]) -> Option<(usize, bool, usize)> {
        let d = &self.delim[..];
        if buf.len() < d.len() {
            return None;
        }
        let mut i = 0usize;
        while i + d.len() <= buf.len() {
            if &buf[i..i + d.len()] == d {
                let pad = Self::transport_padding_len(buf, i + d.len());
                let bstart = i + d.len() + pad;
                // Need at least 2 bytes of lookahead to judge closing `--`
                // vs. the separator CRLF.
                if buf.len() < bstart + 2 {
                    // Defer: we cannot tell yet. The caller keeps the window
                    // and re-examines it when more bytes arrive; the padding
                    // already seen is reported separately by
                    // `deferred_delimiter_span` so it is not emitted as body.
                    return None;
                }
                let closing = buf[bstart..].starts_with(b"--");
                // A real delimiter is followed by `--` or `\r\n`; anything
                // else is not a delimiter (could be body bytes that merely
                // begin with the boundary prefix).
                let sep = buf[bstart..].starts_with(b"\r\n");
                if closing {
                    // RFC 2046 5.1.1: after the closing `--` comes transport
                    // padding and then CRLF (or the end of the message). The
                    // two hyphens alone are a PREFIX of a longer line such as
                    // `--boundary--X`, which used to end the part and discard
                    // everything after it: a body byte immediately after `--`
                    // silently truncated the message.
                    let cpad = Self::transport_padding_len(buf, bstart + 2);
                    let cend = bstart + 2 + cpad;
                    if buf.len() <= cend {
                        // Cannot tell yet whether CRLF or junk follows.
                        return None;
                    }
                    if !buf[cend..].starts_with(b"\r\n") {
                        i += d.len();
                        continue;
                    }
                    return Some((i, true, pad));
                }
                if sep {
                    return Some((i, false, pad));
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
                // The cap must be checked HERE too, before the split.
                //
                // The `len() > HDR_CAP` guard below only runs when the
                // terminator has not arrived yet, so 5000 header bytes plus the
                // blank line in ONE fill slipped past it and were accepted -
                // while the same block split across two reads was rejected.
                // The header span is everything before the terminator; body
                // bytes that arrived in the same fill are not counted.
                if p > HDR_CAP {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "multipart part headers too large",
                    ));
                }
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

    /// Boundary bytes mid-line in the preamble are NOT a delimiter.
    ///
    /// RFC 2046 Section 5.1.1 defines a delimiter as a line STARTING with
    /// `--boundary`, which is exactly what lets a preamble carry arbitrary
    /// text. A mid-line match makes the preamble's REMAINDER the first part's
    /// header block, so the parse starts from the wrong offset and the parts
    /// that follow are reached with the wrong headers - which is how a real
    /// preamble (`This is a multi-part message--b in the middle`) turned into
    /// an extra part followed by a truncated one.
    #[test]
    fn a_boundary_in_the_middle_of_a_preamble_line_is_not_a_delimiter() {
        // `--b` appears mid-line here; the real first boundary is the next line.
        let body = b"preamble--b\r\n--b\r\nContent-Type: application/dicom\r\n\r\nAAAA\r\n--b\r\nContent-Type: application/dicom\r\n\r\nBBBB\r\n--b--\r\n";
        let out = collect(body, b"b");
        assert_eq!(
            out.len(),
            2,
            "both real parts must be parsed, with no spurious one"
        );
        assert_eq!(out[0].1, b"AAAA");
        assert_eq!(out[1].1, b"BBBB");
    }

    /// Transport padding after a boundary must still split the parts.
    ///
    /// RFC 2046 Section 5.1.1 permits linear whitespace between the boundary
    /// text and the terminating CRLF. A separator written as `\r\n--b \t\r\n`
    /// was not recognised, so the two parts fused into one.
    #[test]
    fn a_padded_delimiter_still_splits_parts() {
        // Transport padding is not limited to two bytes (RFC 2046 Section
        // 5.1.1 allows any amount of linear whitespace). Driving the parse
        // through a one-byte reader makes the hold window matter: the padding
        // crosses a read boundary, so a fixed 2-byte carry loses it.
        let body = b"--b\r\nContent-Type: application/dicom\r\n\r\nAAAA\r\n--b \t  \t  \r\nContent-Type: application/dicom\r\n\r\nBBBB\r\n--b--\r\n";
        let out = collect_reader(ByteReader(body), b"b");
        assert_eq!(out.len(), 2, "a padded separator is still a separator");
        assert_eq!(out[0].1, b"AAAA");
        assert_eq!(out[1].1, b"BBBB", "the second part must not fuse");
    }

    #[test]
    fn a_whole_body_read_also_splits_on_long_padding() {
        let body = b"--b\r\nContent-Type: application/dicom\r\n\r\nAAAA\r\n--b    \r\nContent-Type: application/dicom\r\n\r\nBBBB\r\n--b--\r\n";
        let out = collect(body, b"b");
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].1, b"BBBB");
    }

    #[test]
    fn boundary_split_across_reads() {
        let full = b"--b\r\nContent-Type: application/dicom\r\n\r\nDATA\r\n--b--\r\n";
        let out = collect_reader(ByteReader(full), b"b");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, b"DATA");
    }

    /// A one-byte source that returns `WouldBlock` once on its `block_at`-th
    /// read call, then resumes. Forces the hold window to matter: the blocked
    /// read is always a `fill` whose `hold` is non-empty (a one-byte reader
    /// stashes a tail after every fill).
    struct FlakyReader<'a> {
        data: &'a [u8],
        pos: usize,
        block_at: usize,
        calls: usize,
        blocked: bool,
    }
    impl Read for FlakyReader<'_> {
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

    /// A recoverable error must not lose the hold window.
    ///
    /// `fill` used to copy the held tail into `buf` and DRAIN it before
    /// reading from the body, so a `WouldBlock` on the read left an empty hold
    /// and the next call overwrote the staged bytes. A boundary that straddled
    /// the two reads was then lost and the parts fused. The hold window exists
    /// precisely to catch a split boundary - losing it defeats the window.
    #[test]
    fn a_recoverable_error_does_not_lose_the_hold_window() {
        let body = b"--b\r\nContent-Type: application/dicom\r\n\r\nAAAABBBB\r\n--b\r\nContent-Type: application/octet-stream\r\n\r\nCCCC\r\n--b--\r\n";

        let mut p = MultipartParser::new(
            FlakyReader {
                data: body,
                pos: 0,
                block_at: 45,
                calls: 0,
                blocked: false,
            },
            b"b",
        )
        .expect("parser");

        let mut out = Vec::new();
        let mut cur: Option<PartHeader> = None;
        let mut data = Vec::new();
        loop {
            match p.next_event() {
                // WouldBlock is recoverable: the caller retries the SAME
                // parser, so the hold window must have survived the error.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => panic!("unexpected error: {e}"),
                Ok(MultipartEvent::Part(h)) => {
                    if let Some(prev) = cur.take() {
                        out.push((prev.content_type().to_vec(), std::mem::take(&mut data)));
                    }
                    cur = Some(h);
                }
                Ok(MultipartEvent::Data(d)) => data.extend_from_slice(d),
                Ok(MultipartEvent::End) => break,
            }
        }
        if let Some(prev) = cur.take() {
            out.push((prev.content_type().to_vec(), std::mem::take(&mut data)));
        }

        assert_eq!(out.len(), 2, "a recoverable error must not fuse the parts");
        assert_eq!(out[0].1, b"AAAABBBB");
        assert_eq!(out[1].1, b"CCCC");
    }

    /// A5(a): padding longer than the read buffer must not hang the parser.
    ///
    /// The window can fill with a single delimiter candidate whose padding has
    /// not ended yet. Retaining all of it left `fill` with no room, so the
    /// event loop spun forever on untrusted input. The parser now either
    /// parses or reports the size limit - it must RETURN either way.
    #[test]
    fn padding_longer_than_the_read_buffer_does_not_hang() {
        let mut body = Vec::new();
        body.extend_from_slice(b"--b\r\nContent-Type: application/dicom\r\n\r\nAAAA\r\n--b");
        body.extend_from_slice(&vec![b' '; READ_BUF + 64]);
        body.extend_from_slice(b"\r\nContent-Type: application/dicom\r\n\r\nBBBB\r\n--b--\r\n");

        // A bounded number of events: a hang would never return at all, and an
        // infinite `Data` stream would trip the cap.
        let mut p = MultipartParser::new(&body[..], b"b").expect("parser");
        let mut events = 0usize;
        let outcome = loop {
            events += 1;
            assert!(events < 100_000, "the parser must terminate");
            match p.next_event() {
                Ok(MultipartEvent::End) => break Ok(()),
                Ok(_) => {}
                Err(e) => break Err(e),
            }
        };
        if let Err(e) = outcome {
            assert_eq!(
                e.kind(),
                io::ErrorKind::InvalidData,
                "a stall must surface as a defined size limit, got {e}"
            );
        }
    }

    /// A5(b): a rejected `--boundaryX` prefix must not pin the window open.
    ///
    /// Its suffix is already known-invalid, so it is body - but keeping it
    /// made the carry equal the whole window, `emit` zero, and the loop spin
    /// before the later REAL delimiter could be reached.
    #[test]
    fn a_rejected_boundary_prefix_does_not_stall_the_parser() {
        let mut body = Vec::new();
        body.extend_from_slice(b"--b\r\nContent-Type: application/dicom\r\n\r\n");
        // A boundary-looking run with an invalid suffix, then more than one
        // buffer of body, then the real separator.
        body.extend_from_slice(b"\r\n--bX");
        body.extend_from_slice(&vec![b'z'; READ_BUF + 128]);
        body.extend_from_slice(
            b"\r\n--b\r\nContent-Type: application/dicom\r\n\r\nBBBB\r\n--b--\r\n",
        );

        let out = collect_reader(ByteReader(&body), b"b");
        assert_eq!(out.len(), 2, "the later real delimiter must still be found");
        assert_eq!(out[1].1, b"BBBB");
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

    /// A boundary-looking run whose suffix is already known-invalid
    /// (`--boundaryX...`) is body, not a delimiter, so the carry window must
    /// not retain it. The generous guard kept it, returning a carry equal to
    /// the whole buffer and forcing a rescan per byte until the window filled.
    #[test]
    fn a_rejected_candidate_is_not_retained_in_the_carry_window() {
        let p = MultipartParser::new(&b""[..], b"boundary").expect("parser");
        let buf = b"\r\n--boundaryXrest";
        let carry = p.unresolved_delimiter_carry(buf);
        assert!(
            carry < buf.len(),
            "a rejectable candidate is body and must not pin the window; carry={carry}, len={}",
            buf.len()
        );
    }

    /// A rejected candidate must not pin the window, so a later real
    /// delimiter is still reached through a one-byte reader (the shape that
    /// made the generous path rescan the whole window per byte).
    #[test]
    fn a_rejected_candidate_still_reaches_the_later_delimiter() {
        let mut body = Vec::new();
        body.extend_from_slice(b"--b\r\nContent-Type: application/dicom\r\n\r\n");
        body.extend_from_slice(b"\r\n--bX");
        body.extend_from_slice(&vec![b'z'; READ_BUF + 128]);
        body.extend_from_slice(
            b"\r\n--b\r\nContent-Type: application/dicom\r\n\r\nBBBB\r\n--b--\r\n",
        );
        let out = collect_reader(ByteReader(&body), b"b");
        assert_eq!(out.len(), 2, "the later real delimiter must still be found");
        assert_eq!(out[1].1, b"BBBB");
    }

    /// RFC 2046 5.1.1: the closing delimiter is `--boundary--` followed by
    /// optional transport padding and a CRLF (or the end of the message). The
    /// two hyphens alone are a PREFIX of a longer line, so `--boundary--X`
    /// used to end the part and discard everything after it.
    #[test]
    fn a_closing_delimiter_with_trailing_junk_is_not_closing() {
        let body =
            b"--b\r\nContent-Type: application/dicom\r\n\r\nA\r\n--b--X still body\r\n--b--\r\n";
        let out = collect(body, b"b");
        assert_eq!(out.len(), 1, "the junk line is body, not the end");
        assert_eq!(
            out[0].1, b"A\r\n--b--X still body",
            "everything up to the REAL closing delimiter is payload"
        );

        // A closing delimiter with padding and CRLF still terminates.
        let body = b"--b\r\nContent-Type: application/dicom\r\n\r\nA\r\n--b--\t \r\n";
        let out = collect(body, b"b");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, b"A");
    }

    /// `HDR_CAP` must hold regardless of how the bytes were split across
    /// reads. Checking it only when the terminator was absent let a header
    /// block that arrived complete in ONE fill slip through.
    #[test]
    fn oversized_part_headers_are_rejected_even_in_one_fill() {
        let mut body = Vec::new();
        body.extend_from_slice(b"--b\r\n");
        body.extend_from_slice(&vec![b'x'; HDR_CAP + 1024]);
        body.extend_from_slice(b"\r\n\r\nA\r\n--b--\r\n");
        let mut p = MultipartParser::new(&body[..], b"b").expect("parser");
        let mut err = None;
        for _ in 0..64 {
            match p.next_event() {
                Ok(MultipartEvent::End) => break,
                Ok(_) => {}
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        assert!(
            err.is_some(),
            "an oversized header block must be rejected, not accepted"
        );
    }
}
