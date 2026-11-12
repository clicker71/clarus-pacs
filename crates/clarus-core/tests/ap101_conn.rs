// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: the connection-loop head path is zero-alloc.
//!
//! Two gates, both pinned against the ap101 TestAllocator (isolated binary:
//! one `#[global_allocator]`, inline window, fixtures built with the
//! allocator disabled):
//! - A2: `read_head` into a caller stack buffer + request-line/header parse
//!   must not allocate (pins the no-to_vec head path).
//! - A7: building the WADO single-object response head into the REUSED
//!   `conn.header` + the take/write/re-assign dance must not allocate.

use clarus_core::http::conn::{Conn, HEADER_BUF_SIZE};
use clarus_core::http::parse::{parse_headers, parse_request_line};
use clarus_core::http::response::{build_head, sendfile_response};
use ferrite_testkit::heap::TestAllocator;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

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

/// A connected socket pair: (server-side stream, live client).
fn socket_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    let client = TcpStream::connect(addr).expect("connect");
    let (server, _peer) = listener.accept().expect("accept");
    (server, client)
}

/// A2: read_head (caller stack buffer) + parse are allocation-free.
#[test]
fn read_head_and_parse_alloc_zero() {
    // Fixtures OUTSIDE the window (Conn::new preallocates its header Vec).
    let (server, mut client) = socket_pair();
    client
        .write_all(
            b"GET /dicomweb/studies HTTP/1.1\r\nHost: x\r\nAccept: application/dicom+json\r\n\r\n",
        )
        .expect("write request");
    let mut conn = Conn::new(server, b"127.0.0.1".to_vec());

    let (zero, n) = allocate_env(|| {
        let mut head_buf = [0u8; HEADER_BUF_SIZE];
        let n = conn
            .read_head(&mut head_buf, Duration::from_secs(5))
            .expect("read head");
        let (mut head, off) = parse_request_line(&head_buf[..n]).expect("line");
        parse_headers(&mut head, &head_buf[off + 2..n]).expect("hdrs");
        n
    });
    assert!(n > 0);
    assert!(
        zero,
        "read_head + request-line/header parse must be zero-allocation (A2)"
    );
}

/// A7: the WADO single-object response head built into the reused
/// `conn.header` (take -> write -> sendfile -> re-assign) is allocation-free.
#[test]
fn wado_single_object_head_reuse_alloc_zero() {
    // Fixtures OUTSIDE the window.
    let (server, _client) = socket_pair();
    let mut conn = Conn::new(server, b"127.0.0.1".to_vec());
    let dir = tempfile::tempdir().expect("tempdir");
    let fpath = dir.path().join("sop.dcm");
    std::fs::write(&fpath, b"\x01\x02\x03\x04\x05\x06\x07\x08").expect("write file");
    let file = std::fs::File::open(&fpath).expect("open file");
    const LEN: u64 = 8;

    let (zero, ()) = allocate_env(|| {
        build_head(
            &mut conn.header,
            200,
            "OK",
            Some(b"application/dicom"),
            Some(LEN),
            true,
            &[],
        );
        let head = std::mem::take(&mut conn.header);
        conn.write_all_configured(&head).expect("write head");
        sendfile_response(conn.stream(), &[], &file, LEN).expect("sendfile");
        conn.header = head;
    });
    assert!(
        zero,
        "building the WADO single-object head into the reused conn.header must be zero-allocation (A7)"
    );
}

/// B5: the chunked framing (chunked_start + chunked_data + chunked_end)
/// is allocation-free. Post-revert state: chunked_data writes 3 x write_all
/// (the write_vectored iovec was reverted after measuring it); the
/// gate is green on both versions and stays.
#[test]
fn chunked_framing_alloc_zero() {
    // Fixtures OUTSIDE the window (Conn::new preallocates its header Vec).
    let (server, _client) = socket_pair();
    let mut conn = Conn::new(server, b"127.0.0.1".to_vec());

    let (zero, ()) = allocate_env(|| {
        conn.chunked_start(200, "OK", b"application/dicom", true)
            .expect("chunked start");
        conn.chunked_data(b"0123456789abcdef")
            .expect("chunked data");
        conn.chunked_end().expect("chunked end");
    });
    assert!(
        zero,
        "chunked_start + chunked_data + chunked_end must be zero-allocation (B5)"
    );
}
