// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Criterion benchmark for the HTTP/1.1 request-head parser.
//!
//! Measures the full head parse (request line + headers) on a ~200-byte
//! request - the same shape the connection loop runs per request. The
//! acceptance threshold is < 5 us on a 200-byte request.
//!
//! Run: `cargo bench --bench parse_bench`

use clarus_core::http::parse::{parse_headers, parse_request_line};
use criterion::{Criterion, black_box, criterion_group, criterion_main};

/// A ~200-byte QIDO `GET` request head (request line + 6 headers).
/// Built with `Vec` outside the measured iteration; the parser borrows only.
fn make_request_200() -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"GET /dicomweb/studies?limit=50&offset=0 HTTP/1.1\r\n");
    buf.extend_from_slice(b"Host: 127.0.0.1:8024\r\n");
    buf.extend_from_slice(b"Accept: application/dicom+json\r\n");
    buf.extend_from_slice(b"Accept-Encoding: identity\r\n");
    buf.extend_from_slice(b"Content-Length: 0\r\n");
    buf.extend_from_slice(b"Content-Type: application/json\r\n");
    buf.extend_from_slice(b"Connection: keep-alive\r\n");
    buf.extend_from_slice(b"\r\n");
    buf
}

fn bench_head_parse(c: &mut Criterion) {
    let req = make_request_200();
    assert!(
        (180..=260).contains(&req.len()),
        "request head should be ~200 bytes (got {})",
        req.len()
    );

    c.bench_function("http_head_parse/200b", |b| {
        b.iter(|| {
            let (mut head, line_end) = parse_request_line(black_box(&req)).expect("request line");
            // `head` is populated by `parse_headers`, but only its byte count
            // was ever handed to `black_box`. With cross-crate inlining the
            // compiler is then free to treat the field stores as dead and
            // delete the very work this benchmark exists to measure, so the
            // head itself is observed too.
            let consumed =
                parse_headers(&mut head, black_box(&req[line_end + 2..])).expect("headers");
            black_box((&head, consumed));
        });
    });
}

criterion_group!(benches, bench_head_parse);
criterion_main!(benches);
