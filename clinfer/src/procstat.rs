// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Best-effort child-process statistics, for `/metrics`.
//!
//! A model is a child process, and the resource an operator has to reason
//! about is that child's memory, not the sidecar's. This module is the one
//! place that reads it.
//!
//! # Failure is DATA, not an error
//!
//! Every function returns `Option`: a read that fails produces an ABSENT
//! gauge rather than a wrong number or an error on the clinical path. The
//! distinction is load-bearing and is why the caller must not fold the two
//! together:
//!
//! - `None` - "no reading this time". The gauge keeps its previous value,
//!   so a child that exited between two ticks does not report 0 bytes of
//!   memory while it is still running.
//! - `Some(ProcStat::ZERO)` - a real reading of a process that is using
//!   nothing, which only the reap path ever wants to publish.
//!
//! # Platforms
//!
//! - Unix: `/proc/<pid>/status` (VmRSS, VmHWM) and `/proc/<pid>/stat`
//!   (utime + stime). `statm` is deliberately NOT read: `status` carries
//!   both RAM figures, and a second parser would be a second thing to get
//!   wrong for no field value.
//! - Windows: four audited WinAPI calls, below. std exposes only a child's
//!   pid and `wait`, so there is no portable way to read a child's memory
//!   or CPU, and this crate does not add a dependency for one gauge.
//!
//! The parsers are split out as pure functions so the `comm` trap (a
//! process name containing spaces or parentheses) is tested without any
//! `/proc` at all.

// The only `unsafe` in this crate. `clinfer/src/lib.rs` carries
// `#![deny(unsafe_code)]` and this module overrides it with `allow`, the
// same pattern `crates/clarus-server/src/ffi.rs` established: `forbid` at
// the root could not be overridden (Rust E0453), so the root stays on
// `deny` and every site below carries a `// SAFETY:` note.
//
// Four WinAPI calls, all value-in/value-out with no lifetimes crossing the
// boundary. They are declared here but only CALLED under `cfg(windows)`,
// which is why the FFI block itself is not gated: the declarations are
// harmless on Unix and gating them would need a second copy of the guard
// logic. `PROCESS_QUERY_LIMITED_INFORMATION` is the narrowest right that
// answers the question, and an `OpenProcess` failure - the child already
// exited, or the access was refused - is `None`, not a zero.
#![allow(unsafe_code)]
#![cfg_attr(not(unix), allow(dead_code))]

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

/// One reading of a running process.
///
/// `cpu_nanos` is user + system, in nanoseconds: the counter is exposed in
/// seconds, and doing the tick arithmetic here keeps the exposition free of
/// `USER_HZ` (section Q4 of the design report).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProcStat {
    /// Resident set size now, in bytes.
    pub rss_bytes: u64,
    /// High-water resident set size since the process started, in bytes.
    pub peak_rss_bytes: u64,
    /// User + system CPU time, in nanoseconds.
    pub cpu_nanos: u64,
}

/// Clock ticks per second, as `USER_HZ` defines it.
///
/// `/proc/<pid>/stat` reports CPU time in these ticks, and the value is a
/// kernel build constant: 100 on every architecture this deployment runs
/// (x86_64, aarch64), where the alternative would be reading the kernel's
/// own `USER_HZ` out of `auxv` for a number that has not differed across
/// supported targets in decades. A build with a different `USER_HZ` would
/// report CPU seconds off by a constant factor - visible, bounded, and
/// still monotone, which is what the gauge is read for.
#[cfg(unix)]
const USER_HZ: u64 = 100;

/// Where `/proc` lives when no root is given.
const DEFAULT_PROC_ROOT: &str = "/proc";

/// The `VmHWM` of THIS process, in bytes.
///
/// The sidecar's own high-water mark: it is the number an operator compares
/// against the children's, to see whether the memory went to a model or to
/// the supervisor.
#[must_use]
pub fn read_self(proc_root: Option<&Path>) -> Option<u64> {
    #[cfg(unix)]
    {
        let text = std::fs::read_to_string(root_or_default(proc_root).join("self/status")).ok()?;
        Some(parse_status(&text)?.peak_rss_bytes)
    }
    #[cfg(not(unix))]
    {
        let _ = proc_root;
        win::read_self()
    }
}

/// Read one process. `None` when it cannot be read at all.
///
/// A failure is the ordinary case for a process that exited between the
/// decision to sample and the read, so it is reported as absence rather
/// than as an error.
#[must_use]
pub fn read(pid: u32, proc_root: Option<&Path>) -> Option<ProcStat> {
    #[cfg(unix)]
    {
        let root = root_or_default(proc_root);
        let status = std::fs::read_to_string(root.join(format!("{pid}/status"))).ok()?;
        let stat = std::fs::read_to_string(root.join(format!("{pid}/stat"))).ok()?;
        Some(ProcStat {
            cpu_nanos: parse_stat(&stat)?.saturating_mul(1_000_000_000 / USER_HZ),
            ..parse_status(&status)?
        })
    }
    #[cfg(not(unix))]
    {
        let _ = proc_root;
        win::read(pid)
    }
}

/// Parse a `status` fixture and a `stat` fixture, the way [`read`] does.
///
/// Exposed for the ap101 gate, which measures the PARSE without the file
/// reads: the reads are the operating system's cost and pinning them would
/// pin one machine's page cache. The body is exactly `read`'s body minus
/// `fs::read_to_string`.
#[must_use]
pub fn parse_for_test(status: &str, stat: &str) -> Option<ProcStat> {
    Some(ProcStat {
        cpu_nanos: parse_stat(stat).map(|ticks| ticks.saturating_mul(1_000_000_000 / 100))?,
        ..parse_status(status)?
    })
}

/// `/proc` root: the given one, or the platform default. Unix only.
#[cfg(unix)]
#[must_use]
fn root_or_default(proc_root: Option<&Path>) -> PathBuf {
    proc_root.map_or_else(|| PathBuf::from(DEFAULT_PROC_ROOT), Path::to_path_buf)
}

/// Parse the `VmRSS` and `VmHWM` lines of a `status` file.
///
/// The values are in kB. Both lines are looked up by their leading token,
/// so a kernel that adds lines above them changes nothing here.
///
/// Not `cfg`-gated: the format is `/proc`'s and stable, so the parser is
/// compiled and tested on every platform - which is what keeps a Windows
/// dev box from shipping a Linux-only bug.
#[must_use]
fn parse_status(status: &str) -> Option<ProcStat> {
    let mut rss_kb = None;
    let mut peak_kb = None;
    for line in status.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        // "VmRSS:     1234 kB" - the unit is always kB, and taking the
        // first token keeps a kernel that pads differently readable.
        let Some(number) = value.split_whitespace().next() else {
            continue;
        };
        match name.trim() {
            "VmRSS" => rss_kb = number.parse::<u64>().ok(),
            "VmHWM" => peak_kb = number.parse::<u64>().ok(),
            _ => {}
        }
    }
    Some(ProcStat {
        rss_bytes: rss_kb?.saturating_mul(1024),
        peak_rss_bytes: peak_kb?.saturating_mul(1024),
        cpu_nanos: 0,
    })
}

/// Parse `utime + stime` out of a `stat` file, returning USER_HZ ticks.
///
/// The second field is the executable name in parentheses, and that name
/// MAY CONTAIN SPACES AND PARENTHESES - `(Web Content)`, `(my)proc`. Fields
/// are therefore counted from after the LAST `)`, which is the only
/// position in the line where the delimiter is unambiguous.
#[must_use]
fn parse_stat(stat: &str) -> Option<u64> {
    let tail = &stat[stat.rfind(')')? + 1..];
    let mut fields = tail.split_whitespace();
    // After the comm field, field 3 (state) is the first token of the tail,
    // so utime (14) and stime (15) are the 12th and 13th.
    let utime = fields.nth(11)?.parse::<u64>().ok()?;
    let stime = fields.next()?.parse::<u64>().ok()?;
    Some(utime.saturating_add(stime))
}

// -- Windows -------------------------------------------------------------
//
// The four calls the section above declares. `read` and `read_self` are
// only reachable under `cfg(windows)`; the declarations stay ungated so
// that the unsafe audit has ONE place to look, and `dead_code` is allowed
// on other platforms for exactly that reason.
#[cfg(not(unix))]
mod win {
    use super::ProcStat;

    /// The struct `GetProcessMemoryInfo` fills for the EX flavour.
    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCountersEx {
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
        private_usage: usize,
    }

    /// `FILETIME`: two 32-bit halves of a 64-bit 100-ns count.
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    // SAFETY: every declaration below is a WinAPI entry point with a fixed
    // C ABI documented by Microsoft, and every call site is `unsafe` and
    // carries its own justification.
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut core::ffi::c_void;
        fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn GetProcessTimes(
            process: *mut core::ffi::c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }

    // SAFETY: `GetProcessMemoryInfo` is a psapi entry point with a fixed C
    // ABI; the call sites below are `unsafe` and state their own reasons.
    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(
            process: *mut core::ffi::c_void,
            counters: *mut ProcessMemoryCountersEx,
            size: u32,
        ) -> i32;
    }

    /// A handle that closes itself.
    struct Handle(*mut core::ffi::c_void);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle came from a successful OpenProcess and is
            // closed exactly once, here.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// 100-ns units in a `FILETIME` to nanoseconds.
    fn nanos(time: FileTime) -> u64 {
        let ticks = (u64::from(time.high) << 32) | u64::from(time.low);
        ticks.saturating_mul(100)
    }

    /// The counters and CPU time of one process handle.
    pub(super) fn stat_of(handle: *mut core::ffi::c_void) -> Option<ProcStat> {
        let mut counters = ProcessMemoryCountersEx {
            cb: u32::try_from(std::mem::size_of::<ProcessMemoryCountersEx>()).ok()?,
            ..ProcessMemoryCountersEx::default()
        };
        // SAFETY: `handle` is a live process handle with query rights and
        // `counters` is a correctly sized, initialized struct of the EX
        // flavour whose `cb` names that size.
        let ok = unsafe {
            GetProcessMemoryInfo(
                handle,
                std::ptr::from_mut(&mut counters),
                std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
            )
        };
        if ok == 0 {
            return None;
        }

        let (mut creation, mut exit, mut kernel, mut user) = (
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
        );
        // SAFETY: four out-parameters of a fixed C layout, each written by
        // the call before it returns.
        let ok = unsafe {
            GetProcessTimes(
                handle,
                std::ptr::from_mut(&mut creation),
                std::ptr::from_mut(&mut exit),
                std::ptr::from_mut(&mut kernel),
                std::ptr::from_mut(&mut user),
            )
        };
        if ok == 0 {
            return None;
        }

        Some(ProcStat {
            // The private commit is the honest "what this process costs":
            // the working set is shared with every other process mapping
            // the same DLLs.
            rss_bytes: counters.private_usage as u64,
            peak_rss_bytes: counters.peak_working_set_size.max(counters.private_usage) as u64,
            cpu_nanos: nanos(kernel).saturating_add(nanos(user)),
        })
    }

    /// `read`, Windows branch.
    pub(super) fn read(pid: u32) -> Option<ProcStat> {
        // SAFETY: a value-in/value-out call; the returned handle is null on
        // failure, which is checked before it is used.
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if raw.is_null() {
            return None;
        }
        stat_of(Handle(raw).0)
    }

    /// `read_self`, Windows branch: the pseudo-handle needs no close.
    pub(super) fn read_self() -> Option<u64> {
        // SAFETY: GetCurrentProcess returns a live pseudo-handle for this
        // process; it must NOT be closed, and nothing here closes it.
        stat_of(unsafe { GetCurrentProcess() }).map(|stat| stat.peak_rss_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `status` body with the two lines the module reads.
    const STATUS: &str = "Name:\tpython3\n\
                          State:\tR (running)\n\
                          VmPeak:\t  524288 kB\n\
                          VmSize:\t  262144 kB\n\
                          VmRSS:\t   10240 kB\n\
                          VmHWM:\t   20480 kB\n\
                          VmData:\t    8192 kB\n";

    #[test]
    fn a_status_file_yields_both_ram_figures_in_bytes() {
        let stat = parse_status(STATUS).expect("both lines are present");
        assert_eq!(stat.rss_bytes, 10_240 * 1024);
        assert_eq!(stat.peak_rss_bytes, 20_480 * 1024);
    }

    #[test]
    fn a_status_file_missing_a_line_is_absent_entirely() {
        // Half a reading is worse than none: a caller that folded the
        // missing half into 0 would publish "this child uses no memory".
        let without_hwm = STATUS.replace("VmHWM:\t   20480 kB\n", "");
        assert!(parse_status(&without_hwm).is_none());
        let without_rss = STATUS.replace("VmRSS:\t   10240 kB\n", "");
        assert!(parse_status(&without_rss).is_none());
    }

    #[test]
    fn a_status_line_with_a_non_numeric_value_is_absent() {
        let odd = STATUS.replace("VmRSS:\t   10240 kB", "VmRSS:\t   unknown kB");
        assert!(parse_status(&odd).is_none());
    }

    #[test]
    fn a_comm_with_spaces_and_parens_does_not_shift_the_fields() {
        // The classic trap: the executable name is the second field, in
        // parentheses, and it may contain both spaces and parentheses. A
        // parser that split the whole line on whitespace reads the wrong
        // field, and a parser that cut at the FIRST ')' cut inside the
        // name.
        let line = "42 (Web Content (tab)) R 1 1 1 0 -1 4194304 100 0 0 0 \
                    700 300 0 0 20 0 5 0 100 200 300 400";
        assert_eq!(parse_stat(line), Some(1_000), "700 utime + 300 stime");
    }

    #[test]
    fn a_plain_comm_reads_the_same_fields() {
        let line = "7 (python3) S 1 7 7 0 -1 4194560 1000 0 0 0 12 8 0 0 20 0 1 0";
        assert_eq!(parse_stat(line), Some(20));
    }

    #[test]
    fn a_stat_line_without_a_comm_is_absent() {
        assert!(parse_stat("garbage without parentheses").is_none());
        assert!(parse_stat("7 (python3) S 1 7").is_none(), "too few fields");
        assert!(
            parse_stat("7 (python3) S 1 7 7 0 -1 4194560 1000 0 0 0 x 8").is_none(),
            "a non-numeric utime is absent, not zero"
        );
    }

    #[test]
    #[cfg(unix)]
    fn ticks_convert_to_nanoseconds_at_the_documented_user_hz() {
        // 100 ticks at USER_HZ = 100 is one second.
        assert_eq!(100 * (1_000_000_000 / USER_HZ), 1_000_000_000);
        assert_eq!(USER_HZ, 100);
    }

    #[cfg(unix)]
    #[test]
    fn a_synthetic_proc_tree_is_read() {
        let root = std::env::temp_dir().join(format!("clinfer-procstat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("4242")).expect("mkdir");
        std::fs::write(root.join("4242/status"), STATUS).expect("write");
        std::fs::write(
            root.join("4242/stat"),
            "4242 (python3) S 1 4242 4242 0 -1 4194560 100 0 0 0 40 60 0 0 20 0 1 0",
        )
        .expect("write");

        let stat = read(4242, Some(&root)).expect("both files are readable");
        assert_eq!(stat.rss_bytes, 10_240 * 1024);
        assert_eq!(stat.peak_rss_bytes, 20_480 * 1024);
        assert_eq!(stat.cpu_nanos, 1_000_000_000, "100 ticks is one second");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A missing process is ABSENT, not zero.
    ///
    /// The `read` contract is what is asserted here, and on Unix it is
    /// asserted against a synthetic tree because that is the only way to
    /// choose the answer. The Windows branch reads the OS, so it is checked
    /// against a pid that cannot exist AND against the live process - the
    /// second half is what proves the branch reads anything at all.
    #[cfg(unix)]
    #[test]
    fn a_missing_process_is_absent_rather_than_zero() {
        let root =
            std::env::temp_dir().join(format!("clinfer-procstat-none-{}", std::process::id()));
        assert_eq!(read(999_999, Some(&root)), None);
        assert_eq!(read_self(Some(&root)), None);
    }

    #[cfg(not(unix))]
    #[test]
    fn a_missing_process_is_absent_rather_than_zero() {
        // A pid far above any plausible allocation: the OS refuses it and
        // the answer is absence, which is the property under test.
        assert_eq!(read(0x7FFF_FFF0, None), None);
        // And the live process IS readable, so "absent" above is a real
        // answer rather than a branch that never reads anything.
        let stat = read(std::process::id(), None).expect("this process is readable");
        assert!(stat.rss_bytes > 0, "{stat:?}");
    }

    #[cfg(unix)]
    #[test]
    fn the_sidecars_own_peak_reads_a_synthetic_status() {
        let root = std::env::temp_dir().join(format!("clinfer-procself-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("self")).expect("mkdir");
        std::fs::write(root.join("self/status"), STATUS).expect("write");
        assert_eq!(read_self(Some(&root)), Some(20_480 * 1024));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_running_process_reads_itself() {
        // Against the real OS, on either platform: a live process has a
        // peak RSS and has used some CPU, so neither reading may be absent.
        let stat = read(std::process::id(), None).expect("this process is readable");
        assert!(stat.rss_bytes > 0, "{stat:?}");
        assert!(stat.peak_rss_bytes > 0, "{stat:?}");
        assert!(read_self(None).is_some_and(|peak| peak > 0));
    }
}
