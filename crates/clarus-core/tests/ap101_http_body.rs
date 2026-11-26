// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: chunked + trailers and content-length reads are zero-alloc.
//!
//! The body readers borrow and buffer inside fixed arrays; the measured window
//! (framing + chunked decode + length read) must not allocate.

use clarus_core::http::body::{ChunkedReader, Framing, framing, read_length};
use ferrite_testkit::heap::TestAllocator;
use std::io::{Cursor, Read};

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

fn allocate_env<F, R>(f: F) -> (bool, R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let result = f();
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (before == after, result)
}

/// A one-byte source that returns `WouldBlock` once, driving the recoverable-
/// error retry paths of the chunked decoder (Size hex_len survival, DataCrlf
/// partial CRLF, Trailer line state). Those paths live in fixed `self` fields,
/// so a retry must be zero-allocation.
struct WouldBlockOnce {
    data: &'static [u8],
    pos: usize,
    block_at: usize,
    calls: usize,
    blocked: bool,
}

impl Read for WouldBlockOnce {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        self.calls += 1;
        if self.calls == self.block_at && !self.blocked {
            self.blocked = true;
            return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock));
        }
        if self.pos >= self.data.len() || out.is_empty() {
            return Ok(0);
        }
        out[0] = self.data[self.pos];
        self.pos += 1;
        Ok(1)
    }
}

#[test]
fn chunked_with_trailers_and_content_length_alloc_zero() {
    // Chunked stream: "4\r\nWiki\r\n5\r\npedia\r\n0\r\nX-Trailer: abc\r\n\r\n"
    let chunked_bytes: &[u8] = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\nX-Trailer: abc\r\n\r\n";
    // Content-Length body: 9 contiguous bytes.
    let length_body: &[u8] = b"012345678";

    let (zero_alloc, ()) = allocate_env(|| {
        // framing is a pure decision (Length vs Chunked) - no alloc.
        assert!(matches!(
            framing(None, Some(&b"chunked"[..])),
            Framing::Chunked
        ));
        assert!(matches!(framing(Some(9), None), Framing::Length(9)));

        // Chunked decode + trailer parse into a fixed stack buffer.
        let mut buf = [0u8; 64];
        let mut cr2 = ChunkedReader::new(Cursor::new(chunked_bytes));
        let mut total = 0usize;
        loop {
            // `expect`, not `unwrap_or(0)`: folding a trailer-decode error into
            // EOF made the loop exit exactly as a clean end does, so a reader
            // that returned all nine bytes and THEN failed on the trailer
            // passed this gate.
            let n = cr2.read(&mut buf).expect("chunked read must not fail");
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(total, 9, "chunked body decodes to 9 bytes");
        // The message is only complete once the terminating chunk AND its
        // trailer section have been consumed.
        assert!(
            cr2.is_message_complete(),
            "the trailer must have been decoded, not silently skipped"
        );

        // Content-Length read over 9 body bytes.
        let mut cl_total = 0usize;
        let mut cursor = Cursor::new(length_body);
        read_length(&mut cursor, 9, &mut |chunk| {
            cl_total += chunk.len();
            Ok(())
        })
        .expect("read_length");
        assert_eq!(cl_total, 9, "content-length read gets 9 bytes");
    });

    assert!(
        zero_alloc,
        "chunked + content-length body reads must be zero-allocation"
    );
}

#[test]
fn chunked_recoverable_error_retry_alloc_zero() {
    // `10\r\n` + 16 payload bytes + terminating chunk. The WouldBlock lands
    // mid size line, so the retry must carry the already-read hex digit and
    // decode 16 bytes - not read `0` as the terminating chunk.
    let body: &[u8] = b"10\r\n0123456789abcdef\r\n0\r\n\r\n";

    let (zero_alloc, ()) = allocate_env(|| {
        let mut cr = ChunkedReader::new(WouldBlockOnce {
            data: body,
            pos: 0,
            block_at: 2, // after the `1`, before the `0`
            calls: 0,
            blocked: false,
        });
        let mut buf = [0u8; 16];
        let mut total = 0usize;
        loop {
            match cr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => panic!("chunked error: {e}"),
            }
        }
        assert_eq!(total, 16, "the 16-byte chunk survives the retry");
    });

    assert!(
        zero_alloc,
        "the chunked recoverable-error retry must be zero-allocation"
    );
}

#[test]
fn chunked_size_line_crlf_retry_alloc_zero() {
    // `10\r\n` + 16 payload bytes. The WouldBlock lands between the CR and LF
    // of the size line, so the retry must resume the LF (not re-read it as a
    // hex digit) and decode 16 bytes - zero-allocation.
    let body: &[u8] = b"10\r\n0123456789abcdef\r\n0\r\n\r\n";

    let (zero_alloc, ()) = allocate_env(|| {
        let mut cr = ChunkedReader::new(WouldBlockOnce {
            data: body,
            pos: 0,
            block_at: 4, // after the `\r`, before the `\n` of the size line
            calls: 0,
            blocked: false,
        });
        let mut buf = [0u8; 16];
        let mut total = 0usize;
        loop {
            match cr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => panic!("chunked error: {e}"),
            }
        }
        assert_eq!(total, 16, "the 16-byte chunk survives the CR-LF retry");
    });

    assert!(
        zero_alloc,
        "the chunked size-line CRLF retry must be zero-allocation"
    );
}

#[test]
fn chunked_extension_skip_alloc_zero() {
    // RFC 9112 7.1.1: `3;foo=bar` is a valid size line; only the hex prefix
    // is the size. The extension skip is a bounded in-place scan, no heap.
    let chunked_bytes: &[u8] = b"3;foo=bar\r\nABC\r\n0\r\n\r\n";

    let (zero_alloc, ()) = allocate_env(|| {
        let mut cr = ChunkedReader::new(Cursor::new(chunked_bytes));
        let mut buf = [0u8; 16];
        let mut total = 0usize;
        loop {
            let n = cr.read(&mut buf).expect("chunked read");
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(
            total, 3,
            "the extension is skipped, the 3-byte chunk decodes"
        );
    });

    assert!(
        zero_alloc,
        "the chunk extension skip must be zero-allocation"
    );
}
