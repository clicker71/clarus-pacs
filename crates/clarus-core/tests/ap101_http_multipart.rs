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
