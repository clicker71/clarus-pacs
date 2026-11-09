// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Disk usage utilities - thin SAFE facade over the audited OS boundary
//! (`crate::ffi`): statvfs on Unix, GetDiskFreeSpaceExW on Windows.
//! The public API (`total_disk_mb` / `available_disk_mb`) is unchanged and
//! is used by the migration and admin paths.
//!
//! # Fallback
//!
//! All functions return `0.0` on any error (invalid path, OS denial, syscall
//! failure). This is by design - disk usage statistics are best-effort.
//! Admin API never panics on disk stat failures.

use std::path::Path;

/// Total disk capacity at `path`, in megabytes.
/// Returns `0.0` if the information is unavailable.
#[cfg(unix)]
pub fn total_disk_mb(path: &Path) -> f64 {
    crate::ffi::statvfs_mb(path)
        .map(|(total, _avail)| total)
        .unwrap_or(0.0)
}

/// Available (free) disk space at `path`, in megabytes.
/// Returns `0.0` if the information is unavailable.
#[cfg(unix)]
pub fn available_disk_mb(path: &Path) -> f64 {
    crate::ffi::statvfs_mb(path)
        .map(|(_total, avail)| avail)
        .unwrap_or(0.0)
}

// -- Windows implementation (GetDiskFreeSpaceExW via kernel32) ---------

/// Total disk capacity at `path`, in megabytes.
/// Uses `GetDiskFreeSpaceExW` from kernel32.dll.
/// Returns `0.0` if the information is unavailable.
#[cfg(windows)]
pub fn total_disk_mb(path: &Path) -> f64 {
    crate::ffi::disk_free_mb(path)
        .map(|(total, _avail)| total)
        .unwrap_or(0.0)
}

/// Available (free) disk space at `path`, in megabytes.
/// Uses `GetDiskFreeSpaceExW` from kernel32.dll.
/// Returns `0.0` if the information is unavailable.
#[cfg(windows)]
pub fn available_disk_mb(path: &Path) -> f64 {
    crate::ffi::disk_free_mb(path)
        .map(|(_total, avail)| avail)
        .unwrap_or(0.0)
}

/// `(total_mb, available_mb)` at `path`, or `None` when the stat itself
/// failed.
///
/// The `f64` accessors above collapse "the syscall failed" and "the volume is
/// genuinely full" into the same `0.0`, which is fine for a best-effort
/// readout but not for a GAUGE: a metric that cannot tell the two cases apart
/// either freezes at its last value during a full disk (the case an operator
/// most needs to see change) or reports zero whenever the NAS is briefly
/// unreachable. Callers that publish state use this instead.
pub fn disk_space_mb(path: &Path) -> Option<(f64, f64)> {
    #[cfg(unix)]
    {
        crate::ffi::statvfs_mb(path)
    }
    #[cfg(windows)]
    {
        crate::ffi::disk_free_mb(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}
