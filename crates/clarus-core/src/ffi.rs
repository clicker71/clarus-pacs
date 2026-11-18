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

use crate::idx::preload::SealedIdxRoot;
use std::path::{Path, PathBuf};

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
// Kernel32 process/handle primitives. `HANDLE` is pointer-sized; `BOOL` = `i32`
// (nonzero = success). Declared here rather than pulled from a crate: the calls
// are one flag combination each, and the alternative is a dependency whose
// surface is far larger than the arguments below.
#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetFilePointerEx(
        hFile: isize,
        liDistanceToMove: i64,
        lpNewFilePointer: *mut i64,
        dwMoveMethod: u32,
    ) -> i32;
    fn CloseHandle(hObject: isize) -> i32;
    fn GetFinalPathNameByHandleW(
        hFile: isize,
        lpszFilePath: *mut u16,
        cchFilePath: u32,
        dwFlags: u32,
    ) -> u32;
    fn CreateFileW(
        lpFileName: *const u16,
        dwDesiredAccess: u32,
        dwShareMode: u32,
        lpSecurityAttributes: *mut core::ffi::c_void,
        dwCreationDisposition: u32,
        dwFlagsAndAttributes: u32,
        hTemplateFile: isize,
    ) -> isize;
}

/// `GENERIC_READ` (winnt.h): open for reading only.
#[cfg(windows)]
const GENERIC_READ: u32 = 0x8000_0000;
/// `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`: never block
/// another opener, matching the descriptor the caller already holds.
#[cfg(windows)]
const FILE_SHARE_ALL: u32 = 0x0000_0007;
/// `OPEN_EXISTING`: the file is already there; never create or truncate.
#[cfg(windows)]
const OPEN_EXISTING: u32 = 3;
/// `FILE_ATTRIBUTE_NORMAL`: no special attributes for a plain read.
#[cfg(windows)]
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
/// `FILE_NAME_NORMALIZED | VOLUME_NAME_DOS`: `GetFinalPathNameByHandleW` form
/// without the `\\?\` prefix.
#[cfg(windows)]
const FILE_NAME_NORMALIZED_VOLUME_DOS: u32 = 0x0000_0000;

/// Largest single transfer `TransmitFile` accepts (0x7FFF_FFFE).
///
/// The API rejects anything above this, so the guard has to match the kernel's
/// own limit rather than `u32::MAX`: a body in `(0x7FFF_FFFE, 0xFFFF_FFFF]`
/// would otherwise pass the check, fail in the kernel after the head was
/// already written, and leave the connection desynchronised.
#[cfg(windows)]
pub const TRANSMIT_FILE_MAX: u64 = 0x7FFF_FFFE;

/// Windows `sendfile`-equivalent: one `TransmitFile` call at `offset`, no
/// per-send limit, no transmit buffers. Returns nonzero on success.
///
/// `TransmitFile` sends from its handle's CURRENT position and accepts a null
/// `OVERLAPPED` only on a synchronously opened handle, so naming the span means
/// moving a cursor. The cursor moved here belongs to a SEPARATELY OPENED handle:
/// the file position lives in the file object, so a `DuplicateHandle` copy
/// would share the caller's cursor and a concurrent positional read could move
/// it mid-transfer. Reopening by the resolved final path gives an independent
/// file object, hence an independent position.
///
/// # Caller obligation: the file must be immutable
///
/// Reopening by path has a window between `GetFinalPathNameByHandleW` and
/// `CreateFileW` in which the name could be replaced, so the reopen could open a
/// different file than the handle names. That is not a hazard here because
/// every caller passes a CONTENT-ADDRESSED STORE blob: a CAS object is named by
/// the digest of its bytes and is never renamed, rewritten or replaced, so the
/// resolved path continues to name the same immutable content.
///
/// Callers must not use this transfer for a mutable file. That is the only
/// constraint: `ReOpenFile` would also give an independent file object with its
/// own position, so it is not excluded on cursor grounds - reopening by path is
/// used here because the handle is not always available at this layer, and the
/// resolved path is the one fact both callers already hold.
#[cfg(windows)]
pub fn transmit_file(socket: usize, file: usize, len: u32, offset: u64) -> i32 {
    // `nNumberOfBytesToWrite == 0` is not "write nothing" to TransmitFile: the
    // documented behaviour is to send the WHOLE file from the current
    // position. A caller that computed zero bytes (an empty body, a
    // sub-range that came out empty) would push the entire file down the
    // socket while its Content-Length said zero, desynchronising the
    // connection. Nothing to send is a successful zero-length write.
    if len == 0 {
        return 1;
    }
    // SAFETY: the caller passes a live socket and file handle. `path` is a
    // local buffer written only by `GetFinalPathNameByHandleW`, which is told
    // its capacity; the reopened handle is closed by `OwnedHandle` on every
    // return path, and the raw values are only forwarded to the OS.
    unsafe {
        // The buffer is sized from the API's own contract: a path cannot
        // exceed MAX_PATH without the extended prefix, and the call reports the
        // required capacity so a longer one is detected rather than truncated.
        let mut path = [0u16; 1024];
        let n = GetFinalPathNameByHandleW(
            file as isize,
            path.as_mut_ptr(),
            path.len() as u32,
            FILE_NAME_NORMALIZED_VOLUME_DOS,
        );
        // 0 = failure; > capacity = the path does not fit (never trust a
        // truncated path - it names a different file).
        if n == 0 || n as usize > path.len() {
            return 0;
        }
        // `n` excludes the terminator; `CreateFileW` needs one.
        path[n as usize] = 0;
        let raw = CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_ALL,
            core::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            0,
        );
        // The sentinel must be rejected BEFORE the guard exists: closing
        // INVALID_HANDLE_VALUE raises an invalid-handle exception under a
        // debugger, and `OwnedHandle`'s `Drop` would do exactly that.
        if raw == -1 {
            return 0;
        }
        let reopened = OwnedHandle(raw);
        if SetFilePointerEx(reopened.0, offset as i64, core::ptr::null_mut(), 0) == 0 {
            return 0;
        }
        TransmitFile(
            socket,
            reopened.0 as usize,
            len,
            0, // nNumberOfBytesPerSend: no small-send limit
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            0, // flags
        )
    }
}

/// A handle this module opened, closed on drop so no return path leaks it.
#[cfg(windows)]
struct OwnedHandle(isize);

#[cfg(windows)]
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from `CreateFileW` and is closed once, here.
        // INVALID_HANDLE_VALUE is filtered by the caller before construction.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// A crate-owned manifest directory: every `<study>.manifest` in it was
/// published by `stow/manifest.rs` via tmp-file + rename and is never
/// rewritten or truncated in place.
///
/// This is the proof [`crate::manifest::read_manifest`] and
/// [`crate::manifest::read_manifest_study_only`] require, and it is a
/// property of the DIRECTORY, not of any particular path the caller happens
/// to hold. A bare `&Path` cannot express it - a caller could name a
/// directory some other program rewrites - so the mappable cache entry point
/// [`crate::manifest_cache::get_or_load`] takes this type instead of a path.
/// Readers of any path they cannot vouch for use
/// [`crate::manifest::read_manifest_copy`].
///
/// # Construction
///
/// [`TrustedManifestDir::new_unchecked`] is `unsafe`: only code that can vouch
/// for the publication protocol may build one. The audited safe voucher is the
/// server's configured `btree.manifest_dir` (see `clarus/src/ffi.rs`); the
/// mirror of `idx/preload.rs`'s `SealedIdxRoot::from_writer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedManifestDir {
    dir: PathBuf,
}

impl TrustedManifestDir {
    /// Wrap `dir` as the crate-owned manifest directory.
    ///
    /// # Safety
    ///
    /// `dir` must be the directory `stow/manifest.rs` publishes into
    /// (tmp-file + rename), and no writer other than that one may rewrite or
    /// truncate any `<study>.manifest` under it for as long as a mapping
    /// produced from this root is alive. Only the server's audited boundary
    /// (`clarus/src/ffi.rs`) and test fixtures that publish their own files
    /// satisfy this; safe code must obtain the root through that voucher,
    /// never this constructor.
    pub unsafe fn new_unchecked(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }
}

/// Test-only safe voucher: a temp dir the test itself publishes into.
///
/// Mirrors the audited boundary without widening it: test fixtures write their
/// manifests through `write_manifest` (tmp-file + rename) and never truncate
/// them in place, so the publication invariant holds for the duration of the
/// test. Production code never sees this (it is `#[cfg(test)]` and
/// `pub(crate)`).
#[cfg(test)]
pub(crate) fn trusted_manifest_dir_for_test(dir: impl Into<PathBuf>) -> TrustedManifestDir {
    TrustedManifestDir { dir: dir.into() }
}

/// Map a file and hold it open for as long as the mapping is readable.
///
/// The `File` travels with the mapping, so the descriptor cannot be closed
/// (and the name reused) while bytes are still mapped. The remaining
/// obligation - that no in-place writer mutates or truncates the file - is
/// met by the two crate-internal constructors below ([`map_manifest_dir`],
/// [`map_sealed_day`]), each of which states its own file-class argument.
pub struct MappedFile {
    mmap: memmap2::Mmap,
    /// Kept solely to pin the descriptor for the mapping's lifetime.
    _file: std::fs::File,
}

impl MappedFile {
    /// Map an already-open handle, taking ownership of it.
    ///
    /// Private: the soundness of a mapping depends on how the inode is
    /// published, so the decision to map one cannot be delegated through a
    /// public safe signature that accepts any `File`. The two crate-internal
    /// callers below make their file-class argument before calling this.
    fn from_pinned(file: std::fs::File) -> std::io::Result<Self> {
        // SAFETY: the file-class argument is made by the two callers
        // (`map_manifest_dir` and `map_sealed_day`) - a temp+rename published
        // manifest and a sealed (rolled-over) idx day - and the descriptor is
        // kept alive for as long as the mapping is readable, so the name
        // cannot be reused underneath us.
        let mmap = unsafe { memmap2::Mmap::map(&file) }?;
        Ok(Self { mmap, _file: file })
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

/// Map a published manifest file (crate-internal mmap entry point).
///
/// `file_name` is validated as a single path component so `dir.join` cannot
/// escape `dir`.
pub(crate) fn map_manifest_dir(dir: &Path, file_name: &str) -> std::io::Result<MappedFile> {
    let name = Path::new(file_name);
    if file_name.is_empty() || file_name.starts_with('.') || name.components().count() != 1 {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    let file = std::fs::File::open(dir.join(file_name))?;
    // SAFETY: pub(crate) only. The two callers name a manifest the writer
    // published via tmp-file + rename and never truncates in place, so the
    // inode is immutable for the mapping's life; `file_name` is a single
    // component.
    MappedFile::from_pinned(file)
}

/// Map a sealed index day file (crate-internal mmap entry point for preload).
///
/// Sound because it accepts only a sealed day under one of the writer's own
/// day directories, so the mapped inode is never appended to or truncated
/// again.
pub(crate) fn map_sealed_day(
    sealed: &SealedIdxRoot,
    dir: &Path,
    day: &[u8; 8],
) -> std::io::Result<MappedFile> {
    let is_day_dir =
        dir == sealed.dates_dir() || dir.parent() == Some(sealed.modality_root().as_path());
    if !is_day_dir || !sealed.is_sealed(day) || !day.iter().all(|b| b.is_ascii_digit()) {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    let name = format!(
        "{}.idx",
        std::str::from_utf8(day).expect("ascii digits checked")
    );
    let file = std::fs::File::open(dir.join(name))?;
    // SAFETY: `sealed.is_sealed(day)` holds (rolled-over, never written again)
    // and `dir` is one of the writer's day directories, so the inode is
    // immutable.
    MappedFile::from_pinned(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::idx::preload::SealedIdxRoot;
    use crate::idx::rollover::DailyIndexSet;

    /// `map_manifest_dir` maps a published manifest and refuses path escapes.
    ///
    /// The name must be a single path component: an absolute name would
    /// replace the directory in `dir.join`, and `..` would climb out of it.
    /// Both must fail before `File::open` is reached; the missing-file case is
    /// left to the open itself.
    #[test]
    fn map_manifest_dir_maps_and_refuses_escapes() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // `Cargo.toml` is a build input nothing rewrites while the test runs,
        // and the mapping lives only for this function.
        let mapped = map_manifest_dir(dir, "Cargo.toml").expect("map Cargo.toml");
        assert!(!mapped.as_slice().is_empty());

        assert!(matches!(
            map_manifest_dir(dir, "../Cargo.toml"),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(matches!(
            map_manifest_dir(dir, "no-such-manifest.manifest"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound
        ));
    }

    /// `map_sealed_day` maps a sealed day and refuses the current day, a
    /// foreign directory, the root itself and a non-digit day.
    #[test]
    fn map_sealed_day_maps_sealed_and_refuses_others() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut set = DailyIndexSet::new(dir.path().to_path_buf(), *b"20240115");
        // Roll the writer forward: 20240115 becomes sealed, 20240116 current.
        set.rollover_if_needed(*b"20240116");
        let root = SealedIdxRoot::from_writer(&set);
        let dates = root.dates_dir();
        std::fs::create_dir_all(&dates).expect("dates dir");
        std::fs::write(dates.join("20240115.idx"), b"sealed-day\n").expect("write day");
        // Files at every REJECTED path, so each refusal is falsifiable:
        // neutering the matching guard must map the file and flip the
        // assertion, not merely turn an InvalidInput into a NotFound.
        std::fs::write(dates.join("20240116.idx"), b"current\n").expect("write current");
        std::fs::write(dir.path().join("20240115.idx"), b"at-root\n").expect("write at root");
        let foreign = tempfile::tempdir().expect("foreign dir");
        std::fs::write(foreign.path().join("20240115.idx"), b"foreign\n").expect("write foreign");

        assert!(map_sealed_day(&root, &dates, b"20240115").is_ok());
        // The writer's current day is mutable and must not be mapped.
        assert!(map_sealed_day(&root, &dates, b"20240116").is_err());
        // A directory outside the sealed tree is refused, not mapped on trust.
        assert!(map_sealed_day(&root, foreign.path(), b"20240115").is_err());
        // The root itself is not a day directory.
        assert!(map_sealed_day(&root, root.root(), b"20240115").is_err());
        // A day key that is not `YYYYMMDD` digits is refused.
        assert!(map_sealed_day(&root, &dates, b"2024011x").is_err());
    }
}
