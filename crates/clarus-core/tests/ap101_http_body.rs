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
            let n = cr2.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(total, 9, "chunked body decodes to 9 bytes");

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
