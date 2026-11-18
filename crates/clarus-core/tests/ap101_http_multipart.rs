// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101 (multipart): boundary + split-between-read parsing.
//!
//! NOTE: `clarus_core::http::multipart` is a *streaming* parser that keeps a
//! small heap `hold`/`hdr` window inside `MultipartParser`
//! (`multipart.rs:88-101`) - it is NOT zero-allocation in its `next_event`
//! path (`stash_hold`/`read_part_headers` extend `Vec`s). `clarus-core/http/*`
//! is frozen, so this test asserts the *correctness* of boundary matching and
//! a boundary split between two `read()`s, plus that the parser reports
//! exactly the parts/bytes of the wire form - not a zero-alloc window, which
//! would be a false claim.

use clarus_core::http::multipart::{MultipartEvent, MultipartParser};
use std::io::{Cursor, Read};

/// A reader that serves at most `N` bytes per `read`, forcing the boundary
/// delimiter to straddle at least one `read` boundary.
struct Chunked {
    inner: Cursor<Vec<u8>>,
    n: usize,
}

impl Read for Chunked {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let want = self.n.min(out.len());
        self.inner.read(&mut out[..want])
    }
}

/// Two parts: `PARTDATA` (8) and `SECOND` (6), then a closing delimiter.
/// First boundary is the preamble form `--boundary42` (no leading CRLF);
/// subsequent parts use the `\r\n--boundary42` separator.
fn sample_body() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"--boundary42\r\n");
    v.extend_from_slice(b"Content-Type: application/dicom\r\n");
    v.extend_from_slice(b"\r\n");
    v.extend_from_slice(b"PARTDATA");
    v.extend_from_slice(b"\r\n--boundary42\r\n");
    v.extend_from_slice(b"Content-Type: application/dicom\r\n");
    v.extend_from_slice(b"\r\n");
    v.extend_from_slice(b"SECOND");
    v.extend_from_slice(b"\r\n--boundary42--\r\n");
    v
}

#[test]
fn multipart_boundary_and_split_read_parses_correctly() {
    const BOUNDARY: &[u8] = b"boundary42";
    let body = sample_body();

    // Serve in 5-byte slices so every boundary is split across read() calls.
    let mut parser = MultipartParser::new(
        Chunked {
            inner: Cursor::new(body),
            n: 5,
        },
        BOUNDARY,
    )
    .expect("parse new");

    let mut parts = 0usize;
    let mut bytes = 0usize;
    loop {
        match parser.next_event().expect("next_event") {
            MultipartEvent::Part(hdr) => {
                assert_eq!(
                    hdr.content_type(),
                    b"application/dicom",
                    "part content-type parsed"
                );
                parts += 1;
            }
            MultipartEvent::Data(chunk) => {
                bytes += chunk.len();
            }
            MultipartEvent::End => break,
        }
    }

    assert_eq!(parts, 2, "two parts parsed");
    assert_eq!(bytes, 8 + 6, "PARTDATA + SECOND across split reads");
}

/// A one-byte source that returns `WouldBlock` once on its `block_at`-th read
/// call, then resumes. A recoverable error must not lose the hold window: the
/// blocked `fill` always has a non-empty `hold`, so draining it before the
/// read (the old shape) dropped the staged tail and fused the parts.
struct Flaky {
    inner: Cursor<Vec<u8>>,
    block_at: usize,
    calls: usize,
    blocked: bool,
}

impl Read for Flaky {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        self.calls += 1;
        if self.calls == self.block_at && !self.blocked {
            self.blocked = true;
            return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock));
        }
        if out.is_empty() {
            return Ok(0);
        }
        // One byte per call, so every boundary straddles read() calls and the
        // hold window is exercised.
        let mut b = [0u8; 1];
        let n = self.inner.read(&mut b)?;
        if n == 0 {
            return Ok(0);
        }
        out[0] = b[0];
        Ok(1)
    }
}

#[test]
fn multipart_recoverable_error_keeps_the_hold_window() {
    const BOUNDARY: &[u8] = b"boundary42";
    let body = sample_body();

    let mut parser = MultipartParser::new(
        Flaky {
            inner: Cursor::new(body),
            block_at: 40, // mid first-part body, where `hold` is non-empty
            calls: 0,
            blocked: false,
        },
        BOUNDARY,
    )
    .expect("parse new");

    let mut parts = 0usize;
    let mut bytes = 0usize;
    loop {
        match parser.next_event() {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => panic!("unexpected error: {e}"),
            Ok(MultipartEvent::Part(hdr)) => {
                assert_eq!(hdr.content_type(), b"application/dicom");
                parts += 1;
            }
            Ok(MultipartEvent::Data(chunk)) => bytes += chunk.len(),
            Ok(MultipartEvent::End) => break,
        }
    }

    assert_eq!(parts, 2, "two parts parsed across a recoverable error");
    assert_eq!(bytes, 8 + 6, "PARTDATA + SECOND survive the retry");
}

/// A rejected boundary candidate must not pin the hold window: a later real
/// delimiter is still reached through a one-byte reader (the shape that made
/// the generous pass rescan the whole window per byte). The parser is
/// documented non-zero-alloc (heap `hold`/`hdr`), so this is a termination +
/// correctness gate, not a 0-alloc window.
#[test]
fn rejected_boundary_candidate_reaches_the_later_delimiter() {
    const BOUNDARY: &[u8] = b"b";
    let mut body = Vec::new();
    body.extend_from_slice(b"--b\r\nContent-Type: application/dicom\r\n\r\n");
    body.extend_from_slice(b"\r\n--bX");
    body.extend_from_slice(&vec![b'z'; 64 * 1024 + 128]);
    body.extend_from_slice(b"\r\n--b\r\nContent-Type: application/dicom\r\n\r\nBBBB\r\n--b--\r\n");

    let mut parser = MultipartParser::new(
        Chunked {
            inner: Cursor::new(body),
            n: 1,
        },
        BOUNDARY,
    )
    .expect("parse new");

    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut data = Vec::new();
    let mut seen_part = false;
    loop {
        match parser.next_event().expect("next_event") {
            MultipartEvent::Part(_) => {
                // A `Part` event precedes its own body bytes: flush the
                // PREVIOUS part's accumulated data, exactly like the unit
                // `collect_reader`.
                if seen_part {
                    out.push(std::mem::take(&mut data));
                }
                seen_part = true;
            }
            MultipartEvent::Data(chunk) => data.extend_from_slice(chunk),
            MultipartEvent::End => {
                out.push(std::mem::take(&mut data));
                break;
            }
        }
    }

    assert_eq!(out.len(), 2, "the later real delimiter must still be found");
    assert_eq!(out[1], b"BBBB", "the second part is intact");
}
