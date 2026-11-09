# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge  -  process RSS read using the standard library only.

The bridge reports its own resident set size to Prometheus
(`clarus_bridge_memory_usage_bytes`). That was the only use of a third-party
dependency, so the read is done here instead: the bridge then installs with
pure-Python packages only, which is what a single-file, offline deployment
needs.

Every platform helper returns 0 on any error. A scrape therefore never fails:
the gauge simply stays at 0, which is visibly wrong rather than accidentally
plausible.
"""

import os
import sys

_IS_WINDOWS = sys.platform == "win32"
_IS_MACOS = sys.platform == "darwin"


def _rss_linux():
    """VmRSS from /proc/self/status, in bytes.

    VmRSS is reported in kB by the kernel; the suffix is stripped rather than
    assumed so the value stays correct if the kernel ever changes units.
    """
    try:
        with open("/proc/self/status", "r", encoding="ascii", errors="replace") as fh:
            for line in fh:
                if line.startswith("VmRSS:"):
                    fields = line.split()
                    if len(fields) >= 2:
                        return int(fields[1]) * 1024
                    return 0
    except (OSError, ValueError):
        return 0
    return 0


def _rss_macos():
    """Resident set size via ps(1), in bytes.

    `ps -o rss=` prints kB with no header, which is the cheapest portable
    answer on macOS without linking a native API.
    """
    try:
        import subprocess

        out = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(os.getpid())],
            capture_output=True,
            text=True,
            timeout=5,
        )
        return int(out.stdout.strip()) * 1024
    except (OSError, ValueError, subprocess.SubprocessError):
        return 0


def _rss_windows():
    """Working set via psapi through ctypes, in bytes.

    Deliberately not `wmic`: it is deprecated and removed on Windows 11 24H2
    and later, so that path would silently report 0 on current systems.
    GetProcessMemoryInfo fills a PROCESS_MEMORY_COUNTERS struct; its first
    member is cb (the struct size), and WorkingSetSize is the third.
    """
    try:
        import ctypes
        from ctypes import wintypes

        class PROCESS_MEMORY_COUNTERS(ctypes.Structure):
            _fields_ = [
                ("cb", wintypes.DWORD),
                ("PageFaultCount", wintypes.DWORD),
                ("PeakWorkingSetSize", ctypes.c_size_t),
                ("WorkingSetSize", ctypes.c_size_t),
                ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                ("PagefileUsage", ctypes.c_size_t),
                ("PeakPagefileUsage", ctypes.c_size_t),
            ]

        counters = PROCESS_MEMORY_COUNTERS()
        counters.cb = ctypes.sizeof(PROCESS_MEMORY_COUNTERS)

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        psapi = ctypes.WinDLL("psapi", use_last_error=True)

        # Pseudo-handle for the calling process: no OpenProcess call, so no
        # handle to leak and nothing to cache between scrapes. The return type
        # must be declared: it defaults to a 32-bit int, which truncates the
        # HANDLE value and makes GetProcessMemoryInfo fail with
        # ERROR_INVALID_HANDLE (6).
        kernel32.GetCurrentProcess.restype = wintypes.HANDLE
        psapi.GetProcessMemoryInfo.argtypes = [
            wintypes.HANDLE,
            ctypes.POINTER(PROCESS_MEMORY_COUNTERS),
            wintypes.DWORD,
        ]
        psapi.GetProcessMemoryInfo.restype = wintypes.BOOL
        handle = kernel32.GetCurrentProcess()
        ok = psapi.GetProcessMemoryInfo(
            handle,
            ctypes.byref(counters),
            ctypes.sizeof(PROCESS_MEMORY_COUNTERS),
        )
        if not ok:
            return 0
        return int(counters.WorkingSetSize)
    except (OSError, AttributeError, ValueError):
        return 0


def get_process_rss_bytes():
    """Return this process's resident set size in bytes, or 0 on failure."""
    if _IS_WINDOWS:
        return _rss_windows()
    if _IS_MACOS:
        return _rss_macos()
    return _rss_linux()
