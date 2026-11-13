# Unsafe Surface Audit - Clarus PACS

**Date:** 29.08.2026 (W-UNSAFE consolidation), re-audited 13.11.2026. **Scope:** all of
`crates/*/src` (production code; tests untouched - they already carry
`#![forbid(unsafe_code)]`).

## Policy (unchanged, now enforced structurally)

1. `unsafe` lives exclusively at the OS boundary, and since 29.08 in
   exactly TWO audited modules:
   - `crates/clarus-core/src/ffi.rs`
   - `clarus/src/ffi.rs`
   Both carry `#![allow(unsafe_code)]` + the "audited OS boundary,
   see docs/unsafe-audit.md" header and expose ONLY safe public
   functions.
2. Crate roots: `clarus-core` and `clarus` are under
   `#![deny(unsafe_code)]`; `clarus-adapters`, `clarus-ports`,
   `cltranscode` and `clarus-testing` are under
   `#![forbid(unsafe_code)]` (they contain zero unsafe, so forbid is
   enforceable there).
   NOTE on deny-vs-forbid: `forbid` at a crate root CANNOT be
   overridden by the `allow` inside the ffi module (Rust E0453) -
   `deny` can. The only `allow(unsafe_code)` overrides in the whole
   tree are the two ffi modules (grep-gated, see below), so new
   `unsafe` outside ffi.rs is a compile error exactly as promised.
3. Every unsafe block carries a `SAFETY:` justification.
4. No pointer arithmetic, no manual memory management, no `unsafe`
   in business logic.

## Inventory (verified 13.11.2026)

| # | Site | API | Why safe | Callers |
|---|------|-----|----------|---------|
| 1 | `clarus-core/src/ffi.rs:31-32` (unix) | `libc::localtime_r` | stack `tm` fully written by the call; thread-local `_r` variant; pointer valid for the call; NULL checked | `time::local_now` |
| 2 | `clarus-core/src/ffi.rs:42-43` (windows) | `libc::localtime_s` | same contract, C11 errno return checked | `time::local_now` |
| 3 | `clarus-core/src/ffi.rs:75` (unix) | `libc::sendfile` | raw fds from `AsRawFd` on live objects held by the caller; `off` points at the caller's local; errors returned, never UB | `http/response::sendfile_response` |
| 4 | `clarus-core/src/ffi.rs:83-84,114-115` (windows) | `TransmitFile` (mswsock) | handles from `AsRawSocket`/`AsRawHandle` on a live stream/file; synchronous call, null OVERLAPPED/buffers; zero return -> copy-loop fallback | `http/response::sendfile_response` |
| 5 | `clarus-core/src/ffi.rs:148,184` | `memmap2::Mmap::map` | caller opens read-only; no concurrent writer (manifests are temp+rename, rolled-over idx files immutable); memmap2 dups the handle | `manifest/read::read_manifest(_study_only)`, `idx/preload::preload_recent` |
| 6 | `clarus/src/ffi.rs:39` (unix) | `libc::kill` (signal 0) | `kill(pid, 0)` only runs the permission/existence checks - delivers nothing and dereferences no pointer; `EPERM` is read as "alive", never as "dead" | `main.rs` (startup second-instance guard, `process_exists`) |
| 7 | `clarus/src/ffi.rs:59,64` (unix) | `libc::statvfs` + `MaybeUninit::assume_init` | `CString` from validated `to_str`; `assume_init` only after rc == 0 (kernel filled the struct) | `disk_utils::total_disk_mb` / `available_disk_mb` |
| 8 | `clarus/src/ffi.rs:92-93,111-112` (windows) | `GetDiskFreeSpaceExW` | `encode_wide().chain(once(0))` - valid null-terminated UTF-16; output pointers to stack `u64`s; BOOL checked | `disk_utils::total_disk_mb` / `available_disk_mb` |
| 9 | `clarus/src/ffi.rs:133-166` (windows) | `GetStdHandle`/`GetConsoleMode`/`SetConsoleMode` | standard device pseudo-handles (-10/-11/-12), no CloseHandle needed; mode pointers to stack `u32`s; best-effort | `main.rs` (console setup) |
| 10 | `clarus/src/ffi.rs:181-199` (linux) | `getrlimit(RLIMIT_NOFILE)` | fixed resource constant; `#[repr(C)]` struct matches the kernel ABI; advisory warning only | `main.rs` (fd-limit check) |
| 11 | `clarus/src/ffi.rs:248-274` (windows) | `GetCurrentProcess` / `GetProcessMemoryInfo` (psapi) | `GetCurrentProcess` returns the calling process's pseudo-handle - always valid, never closed (nothing here closes it, so no handle leaks); `counters` is a local `#[repr(C)]` `PROCESS_MEMORY_COUNTERS` whose `cb` names its own size, read only after the call reports success | `main.rs::get_rss_bytes_impl` (process RSS metric) |

## What moved where (consolidation, 29.08)

- `time.rs` localtime arms -> ffi `localtime_civil` (safe facade; the
  epoch fallback stays in `time.rs`).
- `http/conn.rs` unix `setsockopt` -> ffi `set_socket_timeouts`
  (later removed 11.11: the unix arm now uses the same safe std
  `set_read_timeout`/`set_write_timeout` as the Windows arm, so the
  `set_socket_timeouts` facade and its `setsockopt` site are gone).
- `http/response.rs` `sendfile` call + the `TransmitFile` extern block
  -> ffi `sendfile_once` / `transmit_file` (loops + copy fallback stay
  in response.rs).
- `idx/preload.rs` + `manifest/read.rs` `Mmap::map` -> ffi
  `map_readonly`; the two `#[allow(unsafe_code)]` markers in read.rs
  are gone.
- `disk_utils.rs` statvfs/GetDiskFreeSpaceExW -> ffi
  `statvfs_mb` / `disk_free_mb`; disk_utils is now a thin SAFE facade,
  public API `total_disk_mb`/`available_disk_mb` unchanged.
- `main.rs` console mode + getrlimit -> ffi `configure_windows_console`
  / `check_fd_limit`; main.rs root now `#![forbid(unsafe_code)]`

## What moved where (Windows process RSS, 19.10.2026)

- `main.rs` Windows `get_rss_bytes_impl` used to spawn
  `wmic process where processid=... get WorkingSetSize /value` and parse the
  text. wmic is deprecated and REMOVED on Windows 11 24H2+ (build 26100),
  where the spawn fails and the gauge silently reported 0.
- The read is now `ffi::process_rss_bytes()` (site #11), a direct psapi
  `GetProcessMemoryInfo` call using the plain 10-field
  `PROCESS_MEMORY_COUNTERS` and reading `WorkingSetSize` - the same quantity
  wmic reported, so the metric stays comparable. Not the EX flavour: clinfer
  reads `PrivateUsage` there for its own commit accounting, which is a
  different question and would quietly redefine this gauge.
- `main.rs` keeps `#![deny(unsafe_code)]`: the Windows arm is now a one-line
  call to the safe facade. The Linux (`/proc/self/status`) and macOS (`ps`)
  arms are untouched.
  (it holds no unsafe anymore).
- Stale claims fixed: `disk_utils.rs` "single point of unsafe in the
  entire codebase" and `clarus-core/src/lib.rs` "The single `unsafe`
  block lives in manifest::read" - both removed.

## Grep proof (gate)

`unsafe {` / `unsafe extern` across `crates/*/src` appear ONLY in
`clarus-core/src/ffi.rs` and `clarus/src/ffi.rs`. Everything
else matching "unsafe" is a lint attribute (`forbid/deny/allow
(unsafe_code)`) or a comment. Zero unsafe in business logic.

## Rule for future code

- New OS-boundary code may add `unsafe` ONLY inside one of the two
  ffi modules, with a `SAFETY:` comment and a new row in this table.
- Anything else: compile error under the root deny/forbid.

## History

- 19.08.2026: first audit (8 scattered sites; doc bug noted -
  "single point" claims were false).
- 29.08.2026: W-UNSAFE consolidation - all 10 sites moved into the
  two ffi modules; roots deny/forbid; docs updated.
- 11.11.2026: unix socket-timeout FFI removed (`setsockopt` replaced by
  safe std setters - the unix and Windows arms now match).
- 13.11.2026: re-audit - dropped the stale `setsockopt` row, added the
  `libc::kill` (`process_exists`) row, refreshed line numbers.
