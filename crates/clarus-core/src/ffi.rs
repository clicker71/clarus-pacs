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
//! `http/response.rs` (sendfile/TransmitFile), `idx/preload.rs` and
//! `manifest/read.rs` (mmap). Socket timeouts go through std
//! (`TcpStream::set_read_timeout` / `set_write_timeout`), so they need no
//! boundary here.

#![allow(unsafe_code)]

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

/// One `sendfile` call: returns the kernel result (`>= 0` = bytes sent,
/// `< 0` = errno). The caller owns the loop, fallback and error mapping.
///
/// Linux/Android ONLY. The four-argument `sendfile(out, in, *off, count)`
/// signature is a Linux extension; the POSIX/BSD one is
/// `(fd, sock, off, len, *hdtr, *sbytes)` and does not accept a descriptor
/// pair, so this declaration does not even link there. macOS/BSD take the
/// generic copy path instead of failing to build.
///
/// # Why this stays instead of `std::io::copy`
///
/// `io::copy` looks like a drop-in replacement, and on Linux it really does
/// dispatch to the kernel - but it tries `copy_file_range` FIRST, and that call
/// is tuned for file->file. Measured on ext4 through a real socket pair:
/// `io::copy` 380 MiB/s against 2694 MiB/s for this direct `sendfile` loop.
/// The capability list says `io::copy` can do more; the clock says it does it
/// 7x slower, so the explicit call is what ships.
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
/// # Safety
///
/// The caller must guarantee that nothing mutates or truncates the underlying
/// file for as long as the returned mapping lives. Two concrete ways that
/// guarantee is broken, both of which memmap2 documents as undefined
/// behaviour:
///
/// - an in-place writer (anything other than temp-file + rename) touching the
///   mapped range, and
/// - the file being truncated or replaced while the mapping is still readable.
///
/// Keeping the `File` open for the mapping's lifetime is NOT sufficient by
/// itself on every platform, which is why this stays `unsafe` and the
/// obligation is spelled out at each call site rather than hidden behind a
/// safe signature.
pub unsafe fn map_readonly(file: &std::fs::File) -> std::io::Result<memmap2::Mmap> {
    // SAFETY: the caller upholds the immutability contract documented above;
    // this block only forwards that obligation to memmap2. The mapping is
    // dropped before `file`.
    unsafe { memmap2::Mmap::map(file) }
}

/// Map a file and hold it open for as long as the mapping is readable.
///
/// Safe counterpart to [`map_readonly`]: the `File` travels with the mapping,
/// so the descriptor cannot be closed (and the name reused) while bytes are
/// still mapped. The remaining obligation - that no in-place writer mutates or
/// truncates the file - is the one the callers below meet by construction, and
/// it is documented per call site.
pub struct MappedFile {
    mmap: memmap2::Mmap,
    /// Kept solely to pin the descriptor for the mapping's lifetime.
    _file: std::fs::File,
}

impl MappedFile {
    /// Map an already-open read-only handle.
    ///
    /// Safe by construction for the file classes this crate maps. The
    /// immutability obligation is NOT delegated to the caller: every file
    /// mapped here is published by temp-file + rename and is never written in
    /// place afterwards -
    ///
    /// - manifests: `stow/manifest.rs` writes `<study>.manifest.tmp` then
    ///   renames, so a reader either sees the previous inode or the new one;
    /// - rolled-over idx days: the writer only ever appends to today's file, and
    ///   a day is sealed before it is preloaded.
    ///
    /// The handle is kept alive for the mapping's lifetime, so the descriptor
    /// cannot be closed and the name reused underneath us.
    pub fn from_file(file: std::fs::File) -> std::io::Result<Self> {
        // SAFETY: see the file-class argument above. memmap2's documented UB
        // conditions are a concurrent in-place writer and a truncation below
        // the mapped range; neither can happen for a published manifest or a
        // sealed index day.
        let mmap = unsafe { memmap2::Mmap::map(&file) }?;
        Ok(Self { mmap, _file: file })
    }

    /// Open `path` read-only and map it, keeping the handle pinned.
    ///
    /// Same file-class argument as [`MappedFile::from_file`].
    pub fn open_readonly(path: &std::path::Path) -> std::io::Result<Self> {
        let file = std::fs::File::open(path)?;
        Self::from_file(file)
    }

    /// The mapped bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.mmap
    }
}

impl std::ops::Deref for MappedFile {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.mmap
    }
}
