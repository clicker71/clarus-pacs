// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Audited OS boundary for `clarus-server`, see `docs/unsafe-audit.md`.
//!
//! This is the ONLY module in the crate where `unsafe` may appear: the crate
//! root carries `#![deny(unsafe_code)]` and this module overrides it with
//! `#![allow(unsafe_code)]` (the established pattern; `forbid` at the root
//! cannot be overridden - Rust E0453 - so the root stays on `deny`). Every
//! site below is a direct OS call (libc / WinAPI) with a `// SAFETY:`
//! justification. Public API is all safe functions.
//!
//! Every unsafe OS call in the server is gathered here. Previous homes:
//! `disk_utils.rs` (statvfs / GetDiskFreeSpaceExW) and `main.rs`
//! (console mode, getrlimit).

#![allow(unsafe_code)]

use std::path::Path;

const MB: f64 = 1024.0 * 1024.0;

/// Whether a pid names a live process.
///
/// `kill(pid, 0)` performs the permission and existence checks without
/// sending a signal. Two outcomes prove the process exists: `rc == 0`, and
/// `EPERM` - the kernel refused because the process belongs to another user.
/// Reporting `EPERM` as "dead" is the dangerous reading: a live instance
/// running under a different uid would be treated as gone, the second process
/// would take the data-directory guard, and two writers would share the
/// manifests and the CAS. `ESRCH` (and anything else) is the only "no such
/// process" answer.
///
/// A direct syscall rather than `kill(1)`: the status of a subprocess cannot
/// distinguish the two cases, and it costs a fork per startup probe.
#[cfg(unix)]
pub fn process_exists(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 only runs the checks - it delivers nothing
    // and dereferences no pointer.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// `statvfs` at `path`: `(total_mb, available_mb)`. `None` on any failure
/// (invalid path, OS denial, syscall error) - disk stats are best-effort.
#[cfg(unix)]
pub fn statvfs_mb(path: &Path) -> Option<(f64, f64)> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;

    let c_path = path.to_str().and_then(|s| CString::new(s).ok())?;

    // `MaybeUninit` instead of `zeroed()`: all-zeroes is not a valid value
    // for every C struct (Rust >= 1.53 UB rules).
    let mut stat = MaybeUninit::<libc::statvfs>::uninit();

    // SAFETY: `c_path` is a valid null-terminated C string alive for the
    // call; `stat.as_mut_ptr()` points at an uninitialized `statvfs` the
    // kernel fills. The struct is only read after rc == 0 below.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: `libc::statvfs` returned 0 - the kernel filled the struct.
    let stat = unsafe { stat.assume_init() };

    let block_size = stat.f_frsize as f64;
    Some((
        stat.f_blocks as f64 * block_size / MB,
        stat.f_bavail as f64 * block_size / MB,
    ))
}

/// `GetDiskFreeSpaceExW` at `path`: `(total_mb, available_mb)`. `None` on
/// any failure - disk stats are best-effort.
#[cfg(windows)]
pub fn disk_free_mb(path: &Path) -> Option<(f64, f64)> {
    use std::os::windows::ffi::OsStrExt;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut free_bytes: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut total_free: u64 = 0;

    // SAFETY: `wide` is a valid null-terminated UTF-16 string alive for the
    // call; the three pointers address stack `u64`s the API fills. BOOL == 0
    // (failure) is checked before the outputs are read.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_bytes as *mut u64,
            &mut total_bytes as *mut u64,
            &mut total_free as *mut u64,
        )
    };
    if ok == 0 {
        return None;
    }

    Some((total_bytes as f64 / MB, free_bytes as f64 / MB))
}

// FFI declaration - kernel32.dll, Windows API.
// `extern "system"` = `stdcall` on x86 Windows, `C` on x64/ARM.
// BOOL = i32: TRUE=nonzero, FALSE=0.
#[cfg(windows)]
unsafe extern "system" {
    fn GetDiskFreeSpaceExW(
        lpDirectoryName: *const u16,
        lpFreeBytesAvailableToCaller: *mut u64,
        lpTotalNumberOfBytes: *mut u64,
        lpTotalNumberOfFreeBytes: *mut u64,
    ) -> i32;
}

/// Windows console setup: disable Quick Edit on stdin (click-to-freeze
/// footgun) and enable ANSI escape processing on stdout/stderr for the log
/// console mirror. Best-effort: no-op when there is no console.
#[cfg(windows)]
pub fn configure_windows_console() {
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    const ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
    const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6u32; // -10
    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5u32; // -11
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4u32; // -12
    unsafe extern "system" {
        fn GetStdHandle(nStdHandle: u32) -> isize;
        fn GetConsoleMode(hConsoleHandle: isize, lpMode: *mut u32) -> i32;
        fn SetConsoleMode(hConsoleHandle: isize, dwMode: u32) -> i32;
    }
    // SAFETY: the handles are standard device pseudo-handles returned by
    // GetStdHandle (-10/-11/-12), valid as console handles without
    // CloseHandle; mode pointers address stack `u32`s written by
    // GetConsoleMode before use. SetConsoleMode takes the mode bits by
    // value. All calls are best-effort (return codes ignored) and no Rust
    // data is aliased.
    unsafe {
        // Disable Quick Edit on stdin - prevents click-to-freeze
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode: u32 = 0;
        if GetConsoleMode(handle, &mut mode) != 0 {
            SetConsoleMode(
                handle,
                mode & !(ENABLE_QUICK_EDIT_MODE | ENABLE_EXTENDED_FLAGS),
            );
        }
        // Enable ANSI escape code processing on stdout
        let stdout_handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut stdout_mode: u32 = 0;
        if GetConsoleMode(stdout_handle, &mut stdout_mode) != 0 {
            SetConsoleMode(
                stdout_handle,
                stdout_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
            );
        }
        // Enable ANSI escape code processing on stderr: the log console
        // mirror paints the [LEVEL] token when stderr is a terminal.
        let stderr_handle = GetStdHandle(STD_ERROR_HANDLE);
        let mut stderr_mode: u32 = 0;
        if GetConsoleMode(stderr_handle, &mut stderr_mode) != 0 {
            SetConsoleMode(
                stderr_handle,
                stderr_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
            );
        }
    }
}

/// Warn when the open-file soft limit is below what the connection pool
/// needs (Linux only). Advisory: the server still starts on a
/// low limit.
///
/// Need ~= `max_connections * 4 + 256` (listener + read/write fds + margin).
#[cfg(target_os = "linux")]
pub fn check_fd_limit(max_connections: u32) {
    unsafe extern "C" {
        fn getrlimit(resource: i32, rlim: *mut RLimit) -> i32;
    }
    #[repr(C)]
    struct RLimit {
        rlim_cur: u64,
        rlim_max: u64,
    }
    const RLIMIT_NOFILE: i32 = 7;
    let mut lim = RLimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit(RLIMIT_NOFILE, &mut lim)` - a fixed resource
    // constant and a pointer to a local `#[repr(C)]` struct that matches the
    // kernel ABI; no aliasing or lifetime implications beyond that contract.
    // The struct is read only after rc == 0.
    unsafe {
        if getrlimit(RLIMIT_NOFILE, &mut lim) == 0 {
            let need = max_connections as u64 * 4 + 256;
            if lim.rlim_cur < need {
                eprintln!(
                    "[WARN] open-file limit {} < ~{} needed for {} connections. \
                     Raise with: ulimit -n {}",
                    lim.rlim_cur, need, max_connections, need
                );
            }
        }
    }
}

/// Non-Linux stub: no limit check.
#[cfg(not(target_os = "linux"))]
pub fn check_fd_limit(_max_connections: u32) {}

/// The struct `GetProcessMemoryInfo` fills (plain flavour).
///
/// The plain 10-field `PROCESS_MEMORY_COUNTERS` is deliberate, not the EX
/// flavour clinfer uses. Its `WorkingSetSize` is the resident set size - the
/// same quantity the old subprocess query reported and the same one the
/// bridge publishes - so the server metric stays comparable across the
/// change. The EX flavour's `PrivateUsage` answers a different question
/// ("what does this process commit"), which clinfer wants for its own
/// accounting but would silently redefine this gauge.
#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
}

// SAFETY: `GetCurrentProcess` and `GetProcessMemoryInfo` are WinAPI/psapi
// entry points with a fixed C ABI documented by Microsoft. `GetCurrentProcess`
// returns a pseudo-handle that is always valid for the calling process and
// must NOT be closed - nothing here closes it, so there is no handle to leak
// or cache. The call site below is `unsafe` and states its own reason.
#[cfg(windows)]
#[link(name = "psapi")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut core::ffi::c_void;
    fn GetProcessMemoryInfo(
        process: *mut core::ffi::c_void,
        counters: *mut ProcessMemoryCounters,
        size: u32,
    ) -> i32;
}

/// Resident set size of this process in bytes, or 0 when the query fails.
///
/// Replaces a subprocess query: that tool is deprecated and REMOVED on
/// Windows 11 24H2 and later (build 26100), where spawning it fails and the
/// gauge read a silent 0. A direct call also costs no process spawn on a
/// path that runs on every metrics scrape.
#[cfg(windows)]
pub fn process_rss_bytes() -> u64 {
    let mut counters = ProcessMemoryCounters {
        cb: core::mem::size_of::<ProcessMemoryCounters>() as u32,
        ..ProcessMemoryCounters::default()
    };
    // SAFETY: `GetCurrentProcess` returns the calling process's pseudo-handle
    // (never closed, always valid); `counters` is a correctly sized local of
    // the exact struct `size` names, and its `cb` field already states that
    // size. The struct is read only after the call reports success.
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            std::ptr::from_mut(&mut counters),
            core::mem::size_of::<ProcessMemoryCounters>() as u32,
        )
    };
    if ok == 0 {
        return 0;
    }
    counters.working_set_size as u64
}
