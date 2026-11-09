// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: request-line + headers parse is zero-allocation.
//!
//! The HTTP header parser must not allocate: it borrows token-by-token from
//! the input slice into a fixed `[ParsedHeader; MAX_HEADERS]` array. This test
//! routes every allocation through `TestAllocator` and asserts the guarded
//! window allocates nothing. Mirrors the A-009 discipline of the other ap101
//! suites.

use clarus_core::http::parse::{Method, RequestHead, parse_headers, parse_request_line};
use ferrite_testkit::heap::TestAllocator;

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
fn request_line_and_fifteen_headers_alloc_zero() {
    // One request line + 15 headers in a single byte buffer. `MAX_HEADERS` is
    // 16 (conn.rs:13), so 15 leaves room for the terminating blank line.
    let mut buf = Vec::new();
    buf.extend_from_slice(b"GET /dicomweb/studies?limit=2 HTTP/1.1\r\n");
    buf.extend_from_slice(b"Host: 127.0.0.1\r\n");
    buf.extend_from_slice(b"Accept: application/dicom+json\r\n");
    buf.extend_from_slice(b"Accept-Encoding: gzip\r\n");
    buf.extend_from_slice(b"Accept-Language: en\r\n");
    buf.extend_from_slice(b"Authorization: Bearer x\r\n");
    buf.extend_from_slice(b"Cache-Control: no-cache\r\n");
    buf.extend_from_slice(b"Connection: keep-alive\r\n");
    buf.extend_from_slice(b"Content-Length: 0\r\n");
    buf.extend_from_slice(b"Content-Type: application/json\r\n");
    buf.extend_from_slice(b"Date: Thu, 01 Jan 2026 00:00:00 GMT\r\n");
    buf.extend_from_slice(b"Origin: https://example.com\r\n");
    buf.extend_from_slice(b"Referer: https://example.com/\r\n");
    buf.extend_from_slice(b"User-Agent: clarus-ap101\r\n");
    buf.extend_from_slice(b"X-Real-IP: 10.0.0.1\r\n");
    buf.extend_from_slice(b"X-Authenticated-User: dr-smith\r\n");
    buf.extend_from_slice(b"\r\n");

    let (zero_alloc, ()) = allocate_env(|| {
        let (mut head, line_end) = parse_request_line(&buf).expect("parse request line");
        assert_eq!(head.method, Method::Get);
        assert_eq!(head.path, b"/dicomweb/studies");
        assert_eq!(head.query, b"limit=2");

        // `parse_headers` takes a slice that starts at the first header line;
        // the request line's CRLF (2 bytes) is past `line_end`. It returns
        // bytes consumed from that trailing slice; all 15 headers + blank
        // line must be consumed.
        let headers_start = line_end + 2;
        let headers_end = parse_headers(&mut head, &buf[headers_start..]).expect("parse headers");
        assert_eq!(
            headers_end,
            buf.len() - headers_start,
            "all 15 headers + terminator consumed"
        );
    });

    assert!(
        zero_alloc,
        "HTTP parse (request line + 15 headers) must be zero-allocation"
    );
}

#[test]
fn framing_smuggling_gates_alloc_zero() {
    // The singleton-header gates (repeat Content-Length, repeat
    // Transfer-Encoding, CL+TE together) run on every request that carries a
    // framing header, so they must not allocate. The seen-flags are a `u8`
    // bitfield; this pins that.
    let rejections = [
        &b"Content-Length: 42\r\nContent-Length: 42\r\n\r\n"[..],
        &b"Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n"[..],
        &b"Content-Length: 42\r\nTransfer-Encoding: chunked\r\n\r\n"[..],
        &b"Transfer-Encoding: chunked\r\nContent-Length: 42\r\n\r\n"[..],
    ];

    for hdr in rejections {
        let (zero_alloc, rejected) = allocate_env(|| {
            let mut head = RequestHead::borrow_empty();
            parse_headers(&mut head, hdr).is_err()
        });
        assert!(rejected, "the gate must reject this header set");
        assert!(zero_alloc, "the framing gate must be zero-allocation");
    }
}
