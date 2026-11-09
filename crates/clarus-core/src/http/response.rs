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

use std::io::{self, Write};

/// The sendfile/copy-loop chunk for the streaming body path.
///
/// Deliberately SEPARATE from `conn::READ_BUF` (the shared body/STOW read
/// buffer): bumping that globally would cost ~200 MiB of extra commit across
/// 512 connections on the STOW path. The copy-loop buffer is a heap `Vec` (not
/// a stack array) because `copy_loop` runs deep in the 512 KiB worker stack.
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

/// Send a file's contents following `head`, via `sendfile` where possible.
///
/// Linux/Android: `write(out, head)` then a `sendfile` loop. Windows:
/// `TransmitFile`. macOS and the BSDs have no four-argument `sendfile`, so they
/// take the generic 64 KB copy loop. The loop also falls back to that copy
/// path when the platform declines the call (e.g. `EINVAL`). Takes the
/// `TcpStream` directly so it can reach the raw descriptor `sendfile` needs.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn sendfile_response(
    out: &std::net::TcpStream,
    head: &[u8],
    file: &std::fs::File,
    len: u64,
) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::io::AsRawFd;
    (&*out).write_all(head)?;
    let out_fd = out.as_raw_fd();
    let in_fd = file.as_raw_fd();
    let mut sent: u64 = 0;
    while sent < len {
        let mut off = sent as libc::off_t;
        // The syscall itself lives in the audited OS boundary (crate::ffi).
        let n = crate::ffi::sendfile_once(out_fd, in_fd, &mut off, (len - sent) as libc::size_t);
        if n < 0 {
            let e = io::Error::last_os_error();
            if errno_is_unsupported(&e) {
                return copy_loop(out, file, sent, len - sent);
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

#[cfg(unix)]
fn errno_is_unsupported(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP)
    )
}

/// Generic unix path where the four-argument `sendfile` does not exist
/// (macOS, the BSDs): head then a plain copy loop.
///
/// Same contract as the sendfile variant - a caller never has to know which
/// one it got - and the copy loop is the same code the sendfile path falls back
/// to, so the bytes on the wire are identical.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
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
    copy_loop(out, file, 0, len)
}

/// Shared bounded copy loop (64 KB chunks) - the cross-platform fallback and
/// the Windows `sendfile` path.
fn copy_loop(
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
}
