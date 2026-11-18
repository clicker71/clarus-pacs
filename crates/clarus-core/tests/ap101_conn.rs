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

/// A2 + A7 + B5: the connection-loop hotpaths, measured SEQUENTIALLY in ONE
/// `#[test]`.
///
/// The four windows below share the process-wide `#[global_allocator]` ALLOC
/// (one per binary). libtest runs separate `#[test]` fns on PARALLEL threads,
/// so one test's `ALLOC.reset()`/`set_enabled()` used to land inside another
/// test's window - the zero-alloc gate was nondeterministic (architectural
/// class E: race on shared mutable state). One function runs the windows on a
/// single thread, back to back, so no sibling can pollute a measurement.
#[test]
fn conn_hotpaths_are_allocation_free() {
    // -- A2: read_head (caller stack buffer) + parse ---------------------
    {
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

    // -- A7 (head half): WADO single-object head into the reused header ----
    //
    // The window covers HEAD construction and the head write ONLY. The body
    // transfer has its own budget below: on macOS/BSD `sendfile_response`
    // falls back to `copy_loop_userspace`, which allocates its bounded buffer
    // by design, so a single window could only assert strict zero on platforms
    // that never take that path - the gate would be wrong or empty elsewhere.
    {
        let (server, _client) = socket_pair();
        let mut conn = Conn::new(server, b"127.0.0.1".to_vec());
        let dir = tempfile::tempdir().expect("tempdir");
        let fpath = dir.path().join("sop.dcm");
        std::fs::write(&fpath, b"\x01\x02\x03\x04\x05\x06\x07\x08").expect("write file");
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
            conn.header = head;
        });
        assert!(
            zero,
            "building the WADO single-object head into the reused conn.header must be zero-allocation (A7)"
        );
    }

    // -- B5: chunked framing -------------------------------------------------
    {
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

    // -- A7 (body half): the BODY transfer's allocation budget --------------
    //
    // The peer stays ALIVE and is drained by a helper thread: closing it makes
    // every body send fail, which would measure the error path rather than the
    // transfer. The drain reads into a FIXED stack buffer, so its own growth
    // can no longer land allocations inside the sender's measured window (a
    // growing `Vec` sink did).
    {
        let (server, client) = socket_pair();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let drain = std::thread::spawn(move || {
            use std::io::Read as _;
            let mut sink = [0u8; 8192];
            // Signal readiness once the reader is positioned at its first
            // read, so the measured send below runs against a live drain.
            ready_tx.send(()).expect("signal");
            loop {
                match (&client).read(&mut sink) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
        ready_rx.recv().expect("receiver ready");

        let conn = Conn::new(server, b"127.0.0.1".to_vec());
        let dir = tempfile::tempdir().expect("tempdir");
        let fpath = dir.path().join("sop.dcm");
        std::fs::write(&fpath, b"\x01\x02\x03\x04\x05\x06\x07\x08").expect("write file");
        let file = std::fs::File::open(&fpath).expect("open file");
        const LEN: u64 = 8;

        // Warm the path once so one-time state (lazy statics, resolver setup)
        // is outside the measured window.
        sendfile_response(conn.stream(), &[], &file, LEN).expect("warm sendfile");

        // Measure the ALLOCATION COUNT, not "exactly zero": the userspace path
        // is allowed one buffer, so the gate has to state a budget rather than
        // a boolean.
        ALLOC.reset();
        ALLOC.set_enabled(true);
        let before = ALLOC.snapshot();
        sendfile_response(conn.stream(), &[], &file, LEN).expect("sendfile");
        let after = ALLOC.snapshot();
        ALLOC.set_enabled(false);
        let allocations = after.0.saturating_sub(before.0);

        // Linux/Android use the kernel copy; everything else uses the
        // userspace loop with its ONE bounded buffer.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(
            allocations, 0,
            "the sendfile body path must be zero-allocation on Linux/Android"
        );
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        assert!(
            allocations <= 1,
            "the userspace body path must stay within its ONE bounded buffer \\
             allocation, got {allocations} (a per-iteration allocation is the \\
             regression this catches)"
        );
        drop(conn);
        let _ = drain.join();
    }
}
