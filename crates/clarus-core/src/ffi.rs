// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Audited OS boundary for `clarus-core`, see `docs/unsafe-audit.md`.
//!
//! This is the ONLY module in the crate where `unsafe` may appear: the crate
//! root carries `#![deny(unsafe_code)]` and this module overrides it with
//! `#![allow(unsafe_code)]` (the established pattern; `forbid` at the root
//! cannot be overridden - Rust E0453 - so the root stays on `deny`). Every
//! site below is a direct OS call (libc / WinAPI / memmap2) with a
//! `// SAFETY:` justification. Public API is all safe functions.
//!
//! Every libc / WinAPI / memmap2 call in the crate is gathered here so the
//! `unsafe` surface is auditable in one file: `time.rs` (localtime),
//! `http/conn.rs` (setsockopt), `http/response.rs` (sendfile/TransmitFile),
//! `idx/preload.rs` and `manifest/read.rs` (mmap).

#![allow(unsafe_code)]

#[cfg(unix)]
use std::time::Duration;

/// Fill a `libc::tm` from the OS local-time conversion for an epoch offset.
///
/// Returns `None` on conversion failure (callers fall back to the epoch).
#[cfg(unix)]
pub fn localtime_civil(epoch_secs: libc::time_t) -> Option<libc::tm> {
    // SAFETY: `tm` is a valid stack buffer that `localtime_r` writes fully
    // and returns a pointer into (NULL only on failure - the same failure
    // class the caller already handles). We hold no aliases to `tm` and the
    // call is the thread-local _r variant, so no shared global state is
    // touched. `epoch_secs` is a `time_t` (i64) read by value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::localtime_r(&epoch_secs, &mut tm) };
    if ok.is_null() { None } else { Some(tm) }
}

/// Windows variant: `localtime_s` returns 0 on success (C11 errno convention).
#[cfg(windows)]
pub fn localtime_civil(epoch_secs: libc::time_t) -> Option<libc::tm> {
    // SAFETY: `tm` is a valid stack buffer written by `localtime_s` on
    // success; the call is the thread-local _s variant. `epoch_secs` is a
    // `time_t` (i64) read by value; the pointer lives for the call only.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::localtime_s(&mut tm, &epoch_secs) };
    if rc != 0 { None } else { Some(tm) }
}

/// Set SO_RCVTIMEO / SO_SNDTIMEO on a socket (the Unix half of
/// `conn::set_timeouts`; Windows uses the std `TcpStream` setters).
#[cfg(unix)]
pub fn set_socket_timeouts(
    stream: &std::net::TcpStream,
    recv: Duration,
    send: Duration,
) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let rv = to_timeval(recv);
    let sv = to_timeval(send);
    // SAFETY: the option pointers are derived from references to stack
    // `timeval`s (`&rv as *const _ as *const c_void`) valid for the duration
    // of each call; `fd` is a live descriptor from `AsRawFd`; the level /
    // option / length constants are the POSIX definitions for SO_RCVTIMEO
    // and SO_SNDTIMEO. Failures are ignored by the caller's contract
    // (best-effort timeouts).
    unsafe {
        let _ = libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &rv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
        let _ = libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            &sv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
    }
    Ok(())
}

#[cfg(unix)]
fn to_timeval(d: Duration) -> libc::timeval {
    libc::timeval {
        tv_sec: d.as_secs() as libc::time_t,
        tv_usec: d.subsec_micros() as libc::suseconds_t,
    }
}

/// One `sendfile` call: returns the kernel result (`>= 0` = bytes sent,
/// `< 0` = errno). The caller owns the loop, fallback and error mapping.
///
/// Linux/Android ONLY. The four-argument `sendfile(out, in, *off, count)`
/// signature is a Linux extension; the POSIX/BSD one is
/// `(fd, sock, off, len, *hdtr, *sbytes)` and does not accept a descriptor
/// pair, so this declaration does not even link there. macOS/BSD take the
/// generic copy path instead of failing to build.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn sendfile_once(
    out_fd: std::os::unix::io::RawFd,
    in_fd: std::os::unix::io::RawFd,
    off: &mut libc::off_t,
    count: libc::size_t,
) -> libc::ssize_t {
    // SAFETY: both descriptors come from `AsRawFd` on live objects held by
    // the caller for the whole call; `off` points at the caller's local
    // `libc::off_t`; the kernel only reads/writes inside those contracts.
    // Errors are reported by the return value, never UB.
    unsafe { libc::sendfile(out_fd, in_fd, off, count) }
}

// Windows `TransmitFile` FFI (mswsock.dll; `extern "system"` = stdcall on
// x86, C on x64/ARM). `SOCKET` and `HANDLE` are both pointer-sized on
// Windows, so they map to `usize`; `BOOL` = `i32` (nonzero = success).
#[cfg(windows)]
#[link(name = "mswsock")]
unsafe extern "system" {
    fn TransmitFile(
        hSocket: usize,
        hFile: usize,
        nNumberOfBytesToWrite: u32,
        nNumberOfBytesPerSend: u32,
        lpOverlapped: *mut core::ffi::c_void,
        lpTransmitBuffers: *mut core::ffi::c_void,
        dwFlags: u32,
    ) -> i32;
}

/// Windows `sendfile`-equivalent: one `TransmitFile` call, file offset 0,
/// no per-send limit, no OVERLAPPED/buffers. Returns nonzero on success.
#[cfg(windows)]
pub fn transmit_file(socket: usize, file: usize, len: u32) -> i32 {
    // `nNumberOfBytesToWrite == 0` is not "write nothing" to TransmitFile: the
    // documented behaviour is to send the WHOLE file from the current
    // position. A caller that computed zero bytes (an empty body, a
    // sub-range that came out empty) would push the entire file down the
    // socket while its Content-Length said zero, desynchronising the
    // connection. Nothing to send is a successful zero-length write.
    if len == 0 {
        return 1;
    }
    // SAFETY: the handles are raw OS handles produced by
    // `AsRawSocket`/`AsRawHandle` on a live stream / file held by the caller.
    // The call is synchronous on the caller's blocking socket with null
    // OVERLAPPED/buffers; it never dereferences Rust data, so this block is
    // sound for valid handles. Failure is a clean zero return -> the caller
    // falls back to a copy loop, never UB.
    unsafe {
        TransmitFile(
            socket,
            file,
            len,
            0, // nNumberOfBytesPerSend: no small-send limit
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            0, // flags
        )
    }
}

/// Map a read-only file into memory (memmap2).
///
/// The caller must open `file` read-only and guarantee no concurrent writer
/// mutates or truncates the underlying file while the mapping lives
/// (manifests are written temp+rename; rolled-over idx files are immutable).
pub fn map_readonly(file: &std::fs::File) -> std::io::Result<memmap2::Mmap> {
    // SAFETY: memmap2 duplicates the handle so the returned `Mmap` is
    // self-contained; the UB preconditions (read-only map, no concurrent
    // writes, no file shrink below the mapped range) are guaranteed by the
    // caller's contract above. The mapping is dropped before `file`.
    unsafe { memmap2::Mmap::map(file) }
}
