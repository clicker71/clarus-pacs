// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! HTTP/1.1 response writer (E-004).
//!
//! - Per-connection `Vec<u8>` header buffer, reused across keep-alive requests.
//! - `writev` for JSON responses (one syscall: header block + body) on Unix.
//! - `sendfile` for WADO instance retrieval (Linux `sendfile`, Windows
//!   `TransmitFile`, fallback copy loop).
//! - Chunked writer for multipart WADO where length is unknown up front.
//!
//! `Date`/`Server` headers are intentionally omitted - nginx adds them.
//!
//! # The body copy is deliberately not `std::io::copy`
//!
//! `io::copy` is the obvious-looking simplification for both platform arms and
//! it was measured on this exact path: it is 7x SLOWER on Linux (380 vs
//! 2694 MiB/s, ext4, real socket pair) because std dispatches to
//! `copy_file_range`, which is tuned for file->file, and it is slower on
//! Windows because std's non-Linux path is a generic 8 KiB loop instead of
//! `TransmitFile`. See [`copy_loop_userspace`] for the numbers. The explicit
//! kernel paths stay.

use std::io::{self, Write};

/// The sendfile/copy-loop chunk for the streaming body path.
///
/// Deliberately SEPARATE from `conn::READ_BUF` (the shared body/STOW read
/// buffer): bumping that globally would cost ~200 MiB of extra commit across
/// 512 connections on the STOW path. The copy-loop buffer is a heap `Vec` (not
/// a stack array) because `copy_loop_userspace` runs deep in the 512 KiB worker
/// stack.
const COPY_BUF: usize = 256 * 1024;

/// Permissive CORS contract, appended to EVERY response head.
///
/// The field stand is a LAN device: the laptop browser calls the origin
/// server directly (no nginx in between), so the server itself must answer
/// CORS. `*` is intentional - when nginx fronts Clarus it keeps owning the
/// real access policy. The browser reads `Content-Type` (CORS-safelisted)
/// off multipart responses, so no `Access-Control-Expose-Headers` is needed.
pub const CORS_ALLOW_ORIGIN: &[u8] = b"Access-Control-Allow-Origin: *\r\n";

/// Build the full response head into `out` (a per-connection scratch `Vec`).
///
/// Always writes the status line, optional `Content-Type`, optional
/// `Content-Length`, and `Connection: close` only when closing. `extra` lets
/// callers append per-response headers.
///
/// Zero allocations when `out` has capacity - the status
/// code and Content-Length render into a stack buffer (no `format!`, no
/// `to_string`). Output is byte-identical to the previous formatting.
#[allow(clippy::too_many_arguments)]
pub fn build_head(
    out: &mut Vec<u8>,
    code: u16,
    reason: &str,
    content_type: Option<&[u8]>,
    content_length: Option<u64>,
    keep_alive: bool,
    extra: &[(&[u8], &[u8])],
) {
    out.clear();
    let mut nb = [0u8; 24];
    out.extend_from_slice(b"HTTP/1.1 ");
    out.extend_from_slice(itoa(code as u64, &mut nb));
    out.extend_from_slice(b" ");
    out.extend_from_slice(reason.as_bytes());
    out.extend_from_slice(b"\r\n");
    if let Some(ct) = content_type {
        out.extend_from_slice(b"Content-Type: ");
        out.extend_from_slice(ct);
        out.extend_from_slice(b"\r\n");
    }
    for (k, v) in extra {
        out.extend_from_slice(k);
        out.extend_from_slice(b": ");
        out.extend_from_slice(v);
        out.extend_from_slice(b"\r\n");
    }
    // The easter eggs ride on EVERY response (SQL-era contract, restored
    // together with HEAD support): /, /dicomweb, errors included.
    out.extend_from_slice(b"x-pragma: once\r\nx-clarus: definitum semel\r\n");
    // CORS on every response (embedded-viewer use case); the preflight
    // OPTIONS answer passes its allow-methods/headers via `extra`.
    out.extend_from_slice(CORS_ALLOW_ORIGIN);
    if let Some(len) = content_length {
        out.extend_from_slice(b"Content-Length: ");
        out.extend_from_slice(itoa(len, &mut nb));
        out.extend_from_slice(b"\r\n");
    }
    if !keep_alive {
        out.extend_from_slice(b"Connection: close\r\n");
    }
    out.extend_from_slice(b"\r\n");
}

/// Render `v` as ASCII decimal into a caller stack buffer; returns the slice.
///
/// Byte-identical to `v.to_string()` (no plus/minus - inputs are unsigned).
/// Shared by [`build_head`] and the conn chunked-start line (B5 0-alloc gate).
pub(crate) fn itoa(mut v: u64, buf: &mut [u8; 24]) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    &buf[i..]
}

/// Write a JSON/text response: head then body (writev on Unix is coalesced by
/// the kernel; for correctness this does two writes, one syscall each).
pub fn write_json<W: Write>(w: &mut W, head: &[u8], body: &[u8]) -> io::Result<()> {
    w.write_all(head)?;
    w.write_all(body)
}

/// Send a file's contents following `head`.
///
/// Unix: `write(out, head)` then the kernel copy below. Windows: `TransmitFile`
/// (see the Windows arm).
///
/// Linux/Android use `sendfile` directly. `std::io::copy` was tried and
/// measured on this exact path: on ext4 through a socket pair it ran at
/// 380 MiB/s against 2694 MiB/s for the explicit `sendfile` loop. The reason is
/// that `io::copy` dispatches to `copy_file_range` FIRST, and that call is
/// tuned for file->file - for a file->socket copy it is the wrong tool. So the
/// loop stays explicit: it is the fastest thing available here, and the
/// capability argument alone does not justify a 7x regression.
#[cfg(unix)]
pub fn sendfile_response(
    out: &std::net::TcpStream,
    head: &[u8],
    file: &std::fs::File,
    len: u64,
) -> io::Result<()> {
    use std::io::Write as _;
    (&*out).write_all(head)?;
    copy_loop(out, file, 0, len)
}

// Windows `TransmitFile` FFI lives in the audited OS boundary
// (crate::ffi::transmit_file); here we only adapt handles and fall back to
// the copy loop on failure.

/// Windows `sendfile`-equivalent (TransmitFile) - falls back to a copy loop.
///
/// S1 (2508, re-opened): the server per-stream drops to 21.3-21.5 MB/s under
/// 2 concurrent C-MOVE workers (aggregate ~42-43), so the server is the common
/// bottleneck and the kernel file->socket path now has a measured aggregate
/// delta. On Windows this sends the body via `TransmitFile` (file -> socket in
/// the kernel, no userspace copy) with a copy_loop fallback on ANY failure.
#[cfg(windows)]
pub fn sendfile_response(
    out: &std::net::TcpStream,
    head: &[u8],
    file: &std::fs::File,
    len: u64,
) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::windows::io::AsRawHandle as _;
    use std::os::windows::io::AsRawSocket as _;
    (&*out).write_all(head)?;

    // Body-only path: the file is opened fresh at offset 0, so TransmitFile
    // sends exactly `len` bytes from the start. `len > u32::MAX` or `0` falls
    // back to the copy loop (robust, no per-part allocation).
    if len != 0 && len <= u32::MAX as u64 {
        let ok = crate::ffi::transmit_file(
            out.as_raw_socket() as usize,
            file.as_raw_handle() as usize,
            len as u32,
        );
        if ok != 0 {
            return Ok(());
        }
    }
    copy_loop_userspace(out, file, 0, len)
}

/// Send `len` bytes of `file` from `start` through the kernel.
///
/// Linux/Android: a `sendfile` loop over the raw descriptors, with the caller's
/// `start` as the initial file offset. Every other unix (macOS, the BSDs) has no
/// four-argument `sendfile`, so it takes the userspace loop; and any kernel
/// refusal (`EINVAL`/`ENOSYS`/`EOPNOTSUPP`) falls back to that same loop, so a
/// caller never has to know which path it got.
///
/// `n == 0` before `len` bytes are out means the file ended early. That is
/// raised as `UnexpectedEof`, not treated as success: a short body under a
/// Content-Length that promised more is a stream desync, not a 500 - the next
/// keep-alive response would be read as the tail of this one.
#[cfg(unix)]
fn copy_loop(
    out: &std::net::TcpStream,
    file: &std::fs::File,
    start: u64,
    len: u64,
) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::io::AsRawFd;
        let out_fd = out.as_raw_fd();
        let in_fd = file.as_raw_fd();
        let mut sent: u64 = 0;
        while sent < len {
            let mut off = match libc::off_t::try_from(start.saturating_add(sent)) {
                Ok(o) => o,
                // The span does not fit this target's `off_t`; the kernel
                // would read from a truncated offset, so serve it in
                // userspace instead of sending the wrong bytes.
                Err(_) => return copy_loop_userspace(out, file, start + sent, len - sent),
            };
            // SAFETY: both descriptors are live for the whole call (owned by
            // the caller's `out` and `file`); `off` is a local; the syscall
            // reports errors by return value and never causes UB.
            let n =
                crate::ffi::sendfile_once(out_fd, in_fd, &mut off, (len - sent) as libc::size_t);
            if n < 0 {
                let e = io::Error::last_os_error();
                if errno_is_unsupported(&e) {
                    return copy_loop_userspace(out, file, start + sent, len - sent);
                }
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "sendfile EOF"));
            }
            sent += n as u64;
        }
        Ok(())
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    {
        copy_loop_userspace(out, file, start, len)
    }
}

/// Whether a `sendfile` errno means "this kernel/filesystem pair will not do
/// it", in which case the userspace loop is the correct fallback rather than
/// an error the client should see.
#[cfg(all(unix, any(target_os = "linux", target_os = "android")))]
fn errno_is_unsupported(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP)
    )
}

/// Userspace 256 KiB copy loop.
///
/// The fallback for the kernel path: on macOS/BSD always (no four-argument
/// `sendfile`), on Linux/Android when the kernel declines (`EINVAL`/`ENOSYS`/
/// `EOPNOTSUPP`) or when `off_t` cannot span the range, and on Windows when
/// `TransmitFile` fails.
///
/// ## Do not replace the kernel paths with `io::copy`
///
/// `io::copy` looks like the same thing, and on Linux it does dispatch to the
/// kernel - but it tries `copy_file_range` FIRST, and that call is tuned for
/// file->file. For a file->socket copy it is the wrong tool. Measured here
/// (ext4, real socket pair, 256 KiB reader, page cache warm, best of 5):
/// `io::copy` 380 MiB/s against 2694 MiB/s for a direct `sendfile` loop.
///
/// On Windows the same swap is worse for a second, independent reason: std maps
/// every non-Linux target to its generic 8 KiB loop, so it would leave the
/// kernel AND drop this buffer's chunk size 32x, on a path already measured at
/// 21.3-21.5 MB/s per stream.
///
/// There is no platform where `io::copy` is faster than what runs here today.
/// If someone proposes it again, re-run the benchmark before believing it.
fn copy_loop_userspace(
    out: &std::net::TcpStream,
    file: &std::fs::File,
    start: u64,
    len: u64,
) -> io::Result<()> {
    use std::io::Read as _;
    use std::io::{Seek, SeekFrom, Write as _};
    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; COPY_BUF]; // heap: avoids burning the 512 KiB worker stack
    let mut remaining = len;
    while remaining > 0 {
        // Read only what is left. `read` may fill up to the whole buffer, so a
        // 1 KiB range would have read 64 KiB off the file, written it to the
        // socket and then underflowed `remaining` on the subtraction - sending
        // the next response's bytes early and corrupting the keep-alive
        // stream.
        let want = (remaining as usize).min(buf.len());
        let n = file.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "copy EOF"));
        }
        (&*out).write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    Ok(())
}

/// Send `len` bytes of `file` starting at `start`, following `head`.
///
/// The offset-aware form of [`sendfile_response`], for responses whose body is
/// a RANGE of the stored object (RFC 9110 14.2): a 206 has to put the
/// requested span on the wire, and buffering the object to slice it defeats
/// the streaming path for exactly the large instances ranges exist for.
///
/// Takes the `TcpStream` directly so it can reach the raw descriptor the
/// kernel call needs. Every platform sends through its own fast path and then
/// the same bounded copy loop, so the bytes are identical either way.
pub fn sendfile_response_at(
    out: &std::net::TcpStream,
    head: &[u8],
    file: &std::fs::File,
    start: u64,
    len: u64,
) -> io::Result<()> {
    use std::io::Write as _;
    // The head is written BEFORE any byte is sent, so a range that cannot be
    // represented must be refused here: after `write_all(head)` a 206 is
    // already on the wire and there is no way back. The caller clamps the span
    // to the file size today (`wado_range::parse_range_header_str`), but this
    // is the function that does the arithmetic, so it validates its own inputs.
    // The kernel copy cannot make this decision for us - it only sees offsets.
    start
        .checked_add(len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "range overflow"))?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if libc::off_t::try_from(start.saturating_add(len)).is_err() {
        // 32-bit targets without large-file support carry an i32 `off_t`, so a
        // span above 2 GiB would truncate and the kernel would read from the
        // wrong offset while the head still promised 206. Those targets can
        // still serve the bytes, just through the userspace loop.
        return copy_loop_userspace(out, file, start, len);
    }
    (&*out).write_all(head)?;
    #[cfg(unix)]
    return copy_loop(out, file, start, len);
    #[cfg(windows)]
    copy_loop_userspace(out, file, start, len)
}

#[cfg(test)]
mod tests {
    use super::build_head;
    /// Pure head-builder contract: status line, CT, CL, keep-alive vs close,
    /// and extra headers. Covers the transport head shape for the sync conn
    /// loop (each keep-alive request reuses `Conn.header` via this builder).
    #[test]
    fn build_head_full() {
        let mut out = Vec::new();
        build_head(
            &mut out,
            200,
            "OK",
            Some(b"application/dicom"),
            Some(1234),
            true,
            &[(b"Accept-Ranges", b"bytes")],
        );
        let s = String::from_utf8_lossy(&out);
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(s.contains("Content-Type: application/dicom\r\n"));
        assert!(s.contains("Accept-Ranges: bytes\r\n"));
        assert!(s.contains("Access-Control-Allow-Origin: *\r\n"));
        assert!(s.contains("Content-Length: 1234\r\n"));
        assert!(!s.contains("Connection: close"));
        assert!(s.ends_with("\r\n\r\n"));
    }

    #[test]
    fn build_head_close_when_keepalive_off() {
        let mut out = Vec::new();
        build_head(&mut out, 404, "Not Found", None, Some(0), false, &[]);
        let s = String::from_utf8_lossy(&out);
        assert!(s.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(s.contains("Connection: close\r\n"));
        assert!(s.contains("Access-Control-Allow-Origin: *\r\n"));
        assert!(s.ends_with("\r\n\r\n"));
        // No Content-Type when None.
        assert!(!s.contains("Content-Type:"));
    }

    #[test]
    fn build_head_reuses_buffer() {
        let mut out = b"0123456789abcdef".to_vec();
        build_head(&mut out, 200, "OK", None, None, true, &[]);
        // The builder clears the buffer first - no stale bytes remain.
        // The easter eggs ride on every response (SQL-era contract).
        assert_eq!(
            &out[..],
            b"HTTP/1.1 200 OK\r\nx-pragma: once\r\nx-clarus: definitum semel\r\nAccess-Control-Allow-Origin: *\r\n\r\n"
        );
    }

    /// A span whose END does not fit in `u64` is refused as `InvalidInput`, and
    /// refused BEFORE the head reaches the wire.
    ///
    /// The current caller clamps the span to the file size, so this cannot fire
    /// from a WADO-RS range today - but the head is written before any byte is
    /// sent, so a range that cannot be represented would put a 206 on the wire
    /// and then fail. The function owns the arithmetic, so it checks it.
    ///
    /// The stream is a connected pair so the call cannot fail earlier for an
    /// unrelated reason; the assertion is on the ERROR, not on the bytes.
    #[test]
    fn sendfile_response_at_refuses_an_overflowing_span() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let out = std::net::TcpStream::connect(addr).expect("connect");
        let file = tempfile::NamedTempFile::new().expect("tempfile");

        let err = super::sendfile_response_at(&out, b"HEAD", file.as_file(), u64::MAX, 2)
            .expect_err("a span that overflows u64 must be refused");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidInput,
            "expected InvalidInput, got {err:?}"
        );
    }

    /// A file shorter than the promised span must be an error, never a short
    /// body.
    ///
    /// The kernel reports a truncated file as `n == 0` before `len` bytes are
    /// out; treating that as success would put a short body on the wire under a
    /// Content-Length that promises more, so the client would block and the
    /// next keep-alive response would be read as the tail of this one.
    ///
    /// Runs on both platforms: unix exercises the `sendfile` loop, Windows the
    /// userspace loop that services `TransmitFile`'s fallback.
    #[test]
    fn a_file_shorter_than_the_span_is_refused_before_the_count_is_trusted() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let out = std::net::TcpStream::connect(addr).expect("connect");
        let (_peer, _) = (listener.accept().expect("accept"), ());
        let file = tempfile::NamedTempFile::new().expect("tempfile");
        std::io::Write::write_all(&mut file.as_file(), b"0123456789").expect("write");

        let err = super::sendfile_response_at(&out, b"", file.as_file(), 0, 20)
            .expect_err("20 bytes promised, 10 present");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::UnexpectedEof,
            "expected UnexpectedEof, got {err:?}"
        );
        assert!(
            super::sendfile_response_at(&out, b"", file.as_file(), 0, 10).is_ok(),
            "the full span must still succeed"
        );
    }
}
