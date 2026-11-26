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

    // Body-only path: `TransmitFile` sends from its handle's CURRENT position,
    // which a concurrent `seek_read` on the caller's handle can move. The FFI
    // call reopens the file so the cursor it moves belongs to an independent
    // file object, leaving the caller's handle free for range reads.
    //
    // The size guard is `TransmitFile`'s own limit (0x7FFF_FFFE), not
    // `u32::MAX`: a body just above it would pass a `u32::MAX` check, fail in
    // the kernel after the head was written, and desynchronise the connection.
    //
    // A failed `TransmitFile` reports no sent-byte count, so the body may
    // already be partly on the wire. Restarting it from offset 0 would append
    // the whole body to a prefix and corrupt the stream under a Content-Length
    // that promised one body; the error is propagated instead.
    if len != 0 && len <= crate::ffi::TRANSMIT_FILE_MAX {
        let ok = crate::ffi::transmit_file(
            out.as_raw_socket() as usize,
            file.as_raw_handle() as usize,
            len as u32,
            0,
        );
        if ok != 0 {
            return Ok(());
        }
        return Err(io::Error::other("TransmitFile failed after a partial send"));
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
            let n = crate::ffi::sendfile_once(out_fd, in_fd, &mut off, bounded_count(len - sent));
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
    use std::io::Write as _;
    let mut buf = vec![0u8; COPY_BUF]; // heap: avoids burning the 512 KiB worker stack
    let mut offset = start;
    let mut remaining = len;
    while remaining > 0 {
        // Read only what is left. A bare `read` may fill up to the whole
        // buffer, so a 1 KiB range would have read 64 KiB, written it to the
        // socket and then underflowed `remaining` on the subtraction - sending
        // the next response's bytes early and corrupting the keep-alive
        // stream.
        // Bounded in u64 FIRST: `remaining as usize` truncates a span >= 4 GiB
        // to zero on a 32-bit target, which reads an empty slice and then
        // reports `UnexpectedEof`. The buffer is a small constant, so the
        // narrowed value is always in range.
        let want = remaining.min(buf.len() as u64) as usize;
        // Positional read, NOT `try_clone` + `seek`: a clone shares the file
        // cursor, so seeking on it moves the CALLER's cursor too and two
        // transfers over the same `File` would read from each other's offsets.
        // `read_at` leaves the shared cursor alone.
        let n = read_at_offset(file, &mut buf[..want], offset)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "copy EOF"));
        }
        (&*out).write_all(&buf[..n])?;
        offset += n as u64;
        remaining -= n as u64;
    }
    Ok(())
}

/// Read at an absolute offset without touching the file's shared cursor.
#[cfg(unix)]
fn read_at_offset(file: &std::fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt as _;
    file.read_at(buf, offset)
}

/// Windows positional read.
///
/// `seek_read` reads from `offset` and ALSO moves the shared file cursor to the
/// end of that read. A range read therefore leaves the caller's cursor wrong:
/// with the cursor at 6, a four-byte read at offset 0 moves it to 4, and the
/// next ordinary read starts there instead of at 6.
///
/// The callers in this module are affected only if they interleave ordinary
/// reads with range sends over the SAME handle, which they do not: every use
/// here is a positional copy that never reads sequentially afterwards. The
/// defect is therefore real but latent, and this comment states it rather than
/// claiming the cursor is left alone (which it is not).
///
/// Restoring the position is not possible from a `&File`: `Seek` needs
/// `&mut`, and the cached handle is held shared for the duration of a transfer.
/// Doing it properly needs either a mutable handle and a save/restore pair, or
/// `GetFinalPathNameByHandleW` and a second opened handle - the first changes
/// the transfer API for a bug no caller can currently reach, the second needs a
/// Win32 binding this crate does not take. Recorded here so the next reader
/// does not trust the old comment.
#[cfg(windows)]
fn read_at_offset(file: &std::fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt as _;
    file.seek_read(buf, offset)
}

/// Fallback for targets with neither extension: clone so the seek is private.
#[cfg(not(any(unix, windows)))]
fn read_at_offset(file: &std::fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut f = file.try_clone()?;
    f.seek(SeekFrom::Start(offset))?;
    f.read(buf)
}

/// A transfer count narrowed to this platform's count type WITHOUT truncating.
///
/// `libc::size_t` is 32 bits on a 32-bit target, so `remaining as size_t` turns
/// a 4 GiB span into zero - the kernel is then asked to send nothing, and the
/// loop reports `UnexpectedEof` on a body that is still being served.
///
/// The value is clamped in `u64` and only then narrowed, so a huge span is sent
/// in several bounded calls instead of one zero-length call. Clamping (not
/// erroring) is correct here: each partial send advances `sent`, so the loop
/// makes progress and the caller's `len` is still served in full.
#[cfg(unix)]
fn bounded_count(remaining: u64) -> libc::size_t {
    // `size_t::MAX` is the largest single-call count; anything above it is sent
    // in the next iteration.
    let max = libc::size_t::MAX as u64;
    remaining.min(max) as libc::size_t
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
    let total = start
        .checked_add(len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "range overflow"))?;
    // 32-bit targets without large-file support carry an i32 `off_t`, so a
    // span above 2 GiB would truncate and the kernel would read from the wrong
    // offset. Those targets can still serve the bytes, just through the
    // userspace loop. The DECISION is taken before the head is written,
    // because the head promises a 206 and there is no way back afterwards.
    //
    // The kernel copy exists only on Unix (Windows uses the userspace loop for
    // every span), so the flag is only meaningful there - but it is computed
    // on every target so both arms of the decision are always compiled.
    let fits_off_t = {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            libc::off_t::try_from(total).is_ok()
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = total;
            true
        }
    };

    // One write point for the head, so no transfer path can start without it.
    // The fallback used to `return copy_loop_userspace(..)` before this line,
    // which sent the BODY with no status line, no Content-Type and no
    // Content-Length - the client saw a headless byte stream and could only
    // time out.
    (&*out).write_all(head)?;

    #[cfg(unix)]
    if fits_off_t {
        return copy_loop(out, file, start, len);
    }
    // Not reached on Unix when `fits_off_t` holds (the `return` above), and
    // the only path on Windows.
    #[cfg(not(unix))]
    let _ = fits_off_t;
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

    /// F10: the userspace copy must read AT an offset, not seek a clone.
    ///
    /// `try_clone` shares the file cursor, so `seek` on the clone also moves the
    /// ORIGINAL handle's position. Two concurrent transfers over one `File`
    /// (a warm-cache sendfile fallback and a range request, say) then read from
    /// each other's offsets and splice the wrong bytes into a response.
    ///
    /// The observable consequence is that interleaving two spans over ONE
    /// handle must still return each span's own bytes. A shared seek makes the
    /// second read start wherever the first left the cursor.
    #[test]
    fn interleaved_positional_reads_keep_their_own_offsets() {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        f.write_all(b"0123456789abcdef").expect("write");
        let file = f.reopen().expect("reopen");

        // Interleave: a span at 10, then one at 0, then 10 again.
        let mut a = [0u8; 4];
        let n = super::read_at_offset(&file, &mut a, 10).expect("read 10");
        assert_eq!(&a[..n], b"abcd");

        let mut b = [0u8; 4];
        let n = super::read_at_offset(&file, &mut b, 0).expect("read 0");
        assert_eq!(
            &b[..n],
            b"0123",
            "the second span must come from ITS offset, not from where the \
             first read left a shared cursor"
        );

        let mut c = [0u8; 4];
        let n = super::read_at_offset(&file, &mut c, 10).expect("read 10 again");
        assert_eq!(&c[..n], b"abcd", "the offset is absolute, not incremental");
    }

    /// The Windows transfer must name its span, not inherit the cursor.
    ///
    /// `TransmitFile` with a null `OVERLAPPED` sends from the handle's CURRENT
    /// position, which `seek_read` moves even though it returns the requested
    /// offset's bytes. A full-object send racing a range read could therefore
    /// stream the range's span under the full object's Content-Length.
    ///
    /// And a failed `TransmitFile` reports no sent-byte count: re-running the
    /// userspace loop from offset 0 would append a whole body to the bytes the
    /// kernel already wrote.
    #[cfg(windows)]
    #[test]
    fn the_windows_transfer_names_its_span_and_never_restarts_it() {
        let src = include_str!("response.rs");
        let start = src
            .find("pub fn sendfile_response")
            .expect("sendfile_response must exist");
        let rest = &src[start..];
        let end = rest.find("\n/// Send `len` bytes").expect("next item");
        let body = &rest[..end];

        assert!(
            body.contains("crate::ffi::transmit_file("),
            "the body must go through the kernel transfer"
        );
        assert!(
            !body.contains("seek("),
            "the transfer must not depend on the shared cursor"
        );
        assert!(
            body.contains("TransmitFile failed"),
            "a failed send must be propagated, not restarted"
        );
        // The fallback exists only for spans the kernel path cannot take.
        let fallback_pos = body.find("copy_loop_userspace").expect("fallback");
        let guard_pos = body
            .find("TRANSMIT_FILE_MAX")
            .expect("span guard must name the API's own ceiling");
        assert!(
            guard_pos < fallback_pos,
            "the userspace loop must sit behind the span guard"
        );
    }

    /// A4: the size guard is `TransmitFile`'s limit, not `u32::MAX`.
    ///
    /// The API refuses a single transfer above 0x7FFF_FFFE. A guard written as
    /// `u32::MAX` admits the bytes in between, so the call passes the check,
    /// fails in the kernel after the head is already on the wire, and
    /// desynchronises the connection.
    #[cfg(windows)]
    #[test]
    fn the_windows_guard_matches_the_api_ceiling() {
        assert_eq!(
            crate::ffi::TRANSMIT_FILE_MAX,
            2_147_483_646,
            "the guard must be TransmitFile's documented maximum"
        );
        let cap = crate::ffi::TRANSMIT_FILE_MAX;
        assert!(cap < u32::MAX as u64, "the ceiling is below u32::MAX");
        assert!(0x7FFF_FFFEu64 <= cap, "the last legal size is admitted");
        assert!(
            0x7FFF_FFFFu64 > cap,
            "the first illegal size takes the userspace fallback"
        );
        assert!(
            0xFFFF_FFFFu64 > cap,
            "a u32::MAX body must not reach the kernel path"
        );
    }

    /// A1: the transfer must not move the CALLER's file position.
    ///
    /// The file position lives in the file OBJECT on Windows, so a
    /// `DuplicateHandle` copy shares it - seeking the copy moves the original's
    /// cursor, and a concurrent `seek_read` can then race the transfer into the
    /// wrong span. The FFI reopens the file by its final path instead, giving an
    /// independent file object and therefore an independent position.
    ///
    /// The assertion is the observable consequence: a caller that parks its
    /// cursor somewhere and then sends must find it exactly where it left it.
    #[cfg(windows)]
    #[test]
    fn the_windows_transfer_leaves_the_callers_cursor_alone() {
        use std::io::{Seek as _, SeekFrom};
        use std::os::windows::io::AsRawHandle as _;
        use std::os::windows::io::AsRawSocket as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("body.bin");
        std::fs::write(&path, b"0123456789abcdef").expect("write");
        let file = std::fs::File::open(&path).expect("open");

        // Park the caller's cursor in the middle; a transfer that shares the
        // file object will move it.
        (&file).seek(SeekFrom::Start(6)).expect("seek");
        let before = (&file).stream_position().expect("position");

        let (server, client) = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| {
                let addr = l.local_addr()?;
                let client = std::net::TcpStream::connect(addr)?;
                let (server, _) = l.accept()?;
                Ok((server, client))
            })
            .expect("socket pair");
        // Drain in the background so the send cannot block on a full buffer.
        let drain = std::thread::spawn(move || {
            use std::io::Read as _;
            let mut sink = Vec::new();
            let _ = (&client).take(u64::MAX).read_to_end(&mut sink);
        });

        let ok = crate::ffi::transmit_file(
            server.as_raw_socket() as usize,
            file.as_raw_handle() as usize,
            8,
            2,
        );
        assert_ne!(ok, 0, "the transfer must succeed");
        let after = (&file).stream_position().expect("position");
        assert_eq!(
            before, after,
            "the caller's cursor must be untouched by a transfer"
        );

        // And the caller's own positional read still starts where it asks.
        let mut buf = [0u8; 4];
        let n = super::read_at_offset(&file, &mut buf, 0).expect("read");
        assert_eq!(&buf[..n], b"0123", "a positional read ignores the cursor");

        drop(server);
        let _ = drain.join();
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

    /// Every transfer path sends the HEAD before the body.
    ///
    /// The large-offset fallback on 32-bit targets used to `return` into the
    /// userspace copy loop BEFORE writing the head, so the client received a
    /// body with no status line, no Content-Type and no Content-Length. The
    /// decision is now taken first and the head is written once, ahead of
    /// either loop; this test pins the observable half on whichever loop the
    /// platform takes.
    #[test]
    fn the_head_reaches_the_peer_before_the_body_on_every_path() {
        use std::io::Read as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let out = std::net::TcpStream::connect(addr).expect("connect");
        let mut peer = listener.accept().expect("accept").0;

        let file = tempfile::NamedTempFile::new().expect("tempfile");
        std::io::Write::write_all(&mut file.as_file(), b"0123456789").expect("write");
        file.as_file().sync_all().expect("sync");

        super::sendfile_response_at(
            &out,
            b"HTTP/1.1 206 Partial Content\r\nContent-Length: 10\r\n\r\n",
            file.as_file(),
            0,
            10,
        )
        .expect("transfer");
        // The write half has to close before the peer's `read_to_end` can see
        // EOF; otherwise the read blocks until the timeout.
        drop(out);

        let mut got = Vec::new();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("timeout");
        peer.read_to_end(&mut got).expect("read");

        const HEAD: &[u8] = b"HTTP/1.1 206 Partial Content\r\nContent-Length: 10\r\n\r\n";
        assert!(
            got.starts_with(HEAD),
            "the response must open with its head, got: {:?}",
            String::from_utf8_lossy(&got[..got.len().min(80)])
        );
        assert_eq!(
            &got[HEAD.len()..],
            b"0123456789",
            "the body must follow the head"
        );
    }

    /// F6: the per-call send count is clamped in u64 before narrowing.
    ///
    /// `libc::size_t` is 32 bits on a 32-bit target, so `remaining as size_t`
    /// turns a 4 GiB span into ZERO: the kernel is asked to send nothing and the
    /// loop then reports `UnexpectedEof` on a body it never attempted. Clamping
    /// first sends the span in several bounded calls instead.
    ///
    /// Like F5 this is not reachable on a 64-bit host (`size_t` == u64), so the
    /// gate is the arithmetic contract rather than an observable failure.
    #[cfg(unix)]
    #[test]
    fn the_send_count_is_clamped_before_narrowing() {
        // A span far above any 32-bit count must stay non-zero.
        assert!(
            super::bounded_count(1u64 << 32) > 0,
            "a 4 GiB send must not clamp to zero, which would send nothing and \
             then report EOF"
        );
        assert!(super::bounded_count(u64::MAX) > 0);
        // Ordinary spans pass through unchanged.
        assert_eq!(super::bounded_count(0) as u64, 0);
        assert_eq!(super::bounded_count(1234) as u64, 1234);
        // And the clamp is the platform maximum, not an arbitrary constant.
        assert_eq!(
            super::bounded_count(u64::MAX) as u64,
            libc::size_t::MAX as u64
        );
    }
}
