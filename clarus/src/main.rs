// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! #pragma once - Defined once. Works everywhere.
//! Clarus - DICOMweb PACS.
//! A single static binary, lighter than a few tomographic slices.
//!
//! Serves DICOMweb over the synchronous std transport (serve.rs accept loop +
//! thread-per-connection; no axum/tower stack is involved).
//! Logging is the nginx-style plaintext writer (clog.rs); metrics are the
//! hand-rolled Prometheus counters.
//!
//! This is a thin binary wrapper around the `clarus_server` library crate.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::sync::Arc;

use clarus_server::clog;

use clarus_adapters::storage::ContentAddressedStorage;
use clarus_server::config;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;

// -- Minimal CLI flag parsing (no clap - std only) ----------------

/// Parsed CLI flags.
#[derive(Default)]
struct CliFlags {
    config_path: Option<String>,
    show_version: bool,
    test_config: bool,
}

/// Parse minimal CLI arguments: --version, --config <path>, -t
fn parse_cli_flags() -> CliFlags {
    let args: Vec<String> = std::env::args().collect();
    let mut flags = CliFlags::default();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--version" | "-v" => flags.show_version = true,
            "--test-config" | "-t" => flags.test_config = true,
            "--config" | "-c" => {
                i += 1;
                if i < args.len() {
                    flags.config_path = Some(args[i].clone());
                } else {
                    eprintln!("[FATAL] --config requires a path argument");
                    std::process::exit(1);
                }
            }
            other => {
                eprintln!("[FATAL] unknown argument: {other}");
                eprintln!("Usage: clarus [--config <path>] [--version] [-t]");
                std::process::exit(1);
            }
        }
        i += 1;
    }
    flags
}

// -- Process RSS collection (OS thread, no tokio dependency) ------

/// Get current process RSS (Resident Set Size) in bytes.
///
/// Platform-specific:
/// - Windows: `psapi` `GetProcessMemoryInfo` WorkingSetSize (see
///   `clarus_server::ffi::process_rss_bytes`)
/// - Linux: reads `/proc/self/status`
/// - macOS: uses `ps -o rss=`
///
/// Returns 0 on any error - the metric simply stays at 0 if collection fails.
fn get_process_rss_bytes() -> u64 {
    get_rss_bytes_impl()
}

#[cfg(target_os = "windows")]
fn get_rss_bytes_impl() -> u64 {
    clarus_server::ffi::process_rss_bytes()
}

#[cfg(target_os = "linux")]
fn get_rss_bytes_impl() -> u64 {
    match std::fs::read_to_string("/proc/self/status") {
        Ok(text) => {
            for line in text.lines() {
                if line.starts_with("VmRSS:") {
                    return line
                        .trim_start_matches("VmRSS:")
                        .trim()
                        .trim_end_matches(" kB")
                        .parse::<u64>()
                        .unwrap_or(0)
                        * 1024;
                }
            }
            0
        }
        Err(_) => 0,
    }
}

#[cfg(target_os = "macos")]
fn get_rss_bytes_impl() -> u64 {
    let pid = std::process::id();
    match std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
    {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.trim().parse::<u64>().unwrap_or(0) * 1024
        }
        Err(_) => 0,
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn get_rss_bytes_impl() -> u64 {
    0
}

/// Platform default path for the process PID file.
///
/// Linux: `/var/run/clarus-{port}.pid`. A PID file is a system-level
/// artefact on Unix and the FHS location is what init scripts expect; it
/// is not guaranteed writable, so the caller falls back (see
/// `acquire_pid_file`).
///
/// Windows/macOS: next to the config file, which is the historical and
/// always-user-writable location on those platforms.
fn default_pid_path(config_path: &std::path::Path, port: u16) -> std::path::PathBuf {
    if cfg!(target_os = "linux") {
        std::path::PathBuf::from(format!("/var/run/clarus-{port}.pid"))
    } else {
        config_dir_pid_path(config_path, port)
    }
}

/// Fallback: `clarus-{port}.pid` in the config file's directory.
fn config_dir_pid_path(config_path: &std::path::Path, port: u16) -> std::path::PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(format!("clarus-{port}.pid"))
}

/// Create/overwrite a PID file, reporting whether it is now usable.
///
/// Returns `Ok(true)` when the file holds our pid, `Ok(false)` when it
/// could not be written for a non-fatal reason (already warned), and
/// `Err(())` when a live foreign process owns it.
///
/// A stale pid (dead, unparseable, or unreadable) is overwritten, exactly
/// like nginx. A live foreign pid is the ONE fatal case: two servers must
/// not silently share a port and config.
fn try_write_pid_file(pid_path: &std::path::Path, pid: u32) -> Result<bool, ()> {
    use std::io::Write;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(pid_path)
    {
        Ok(mut f) => {
            let _ = writeln!(f, "{}", pid);
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let old_pid_str = std::fs::read_to_string(pid_path).unwrap_or_default();
            let old_pid = old_pid_str.trim().parse::<u32>().ok();
            // Same reasoning as `guard_live_holder`: in a container our pid is
            // 1 on every run, so a leftover PID file names a process that is
            // certainly us. Treating it as a live foreign holder deadlocks the
            // service behind a file only an operator can remove.
            let stale = match old_pid {
                Some(p) if p == pid => true,
                Some(p) => !is_process_alive(p),
                None => true, // Can't parse = assume stale
            };
            if !stale {
                eprintln!(
                    "[FATAL] PID file {} exists and pid={} is ALIVE. \
                     Another instance is running. Remove {} manually if stale.",
                    pid_path.display(),
                    old_pid_str.trim(),
                    pid_path.display()
                );
                return Err(());
            }
            eprintln!(
                "[WARN] Stale PID file {} (pid={} is dead). Overwriting.",
                pid_path.display(),
                old_pid_str.trim()
            );
            // Remove-then-create is not atomic, but this runs once at
            // startup before serving; a race here means two processes
            // started at the same instant, which the create_new below
            // still catches for the loser.
            let _ = std::fs::remove_file(pid_path);
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(pid_path)
            {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", pid);
                    Ok(true)
                }
                Err(e) => {
                    eprintln!(
                        "[WARN] Cannot rewrite stale PID file {}: {}. \
                         Continuing without a PID file.",
                        pid_path.display(),
                        e
                    );
                    Ok(false)
                }
            }
        }
        Err(e) => {
            eprintln!(
                "[WARN] Cannot create PID file {}: {}. \
                 Continuing without a PID file.",
                pid_path.display(),
                e
            );
            Ok(false)
        }
    }
}

/// Resolve and write the PID file. Returns the path actually in use, or
/// an empty path when the process runs without a PID file.
///
/// Path priority is fixed and must not be re-invented:
///   1. platform default (`/var/run/clarus-{port}.pid` on Linux, next to
///      the config elsewhere);
///   2. fallback next to the config file when the default is unusable.
///
/// There is deliberately no configuration for this - no config key, no CLI
/// flag, no environment variable. The PID file is an internal artefact the
/// operator never needs to know about.
///
/// Only a live foreign pid exits the process. Every other failure is a
/// warning: a missing PID file degrades observability, it must not stop
/// the service (incident 04.09 was exactly that in the bridge).
fn acquire_pid_file(config_path: &std::path::Path, port: u16) -> std::path::PathBuf {
    let pid = std::process::id();
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    let default = default_pid_path(config_path, port);
    let fallback = config_dir_pid_path(config_path, port);
    candidates.push(default.clone());
    if fallback != default {
        candidates.push(fallback);
    }

    for candidate in &candidates {
        match try_write_pid_file(candidate, pid) {
            Ok(true) => return candidate.clone(),
            Ok(false) => continue,
            Err(()) => std::process::exit(1),
        }
    }

    eprintln!(
        "[WARN] No writable PID file location (tried {} entries). \
         Continuing without a PID file.",
        candidates.len()
    );
    std::path::PathBuf::new()
}

/// Check if a process is alive by PID (std only, no unsafe, no extra crates).
fn is_process_alive(pid: u32) -> bool {
    is_process_alive_impl(pid)
}

#[cfg(windows)]
fn is_process_alive_impl(pid: u32) -> bool {
    match std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/NH"])
        .output()
    {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.contains(&pid.to_string())
        }
        Err(_) => false,
    }
}

#[cfg(unix)]
fn is_process_alive_impl(pid: u32) -> bool {
    clarus_server::ffi::process_exists(pid)
}

#[cfg(not(any(windows, unix)))]
fn is_process_alive_impl(_pid: u32) -> bool {
    false
}

// -- FD limit check (Linux only) ----------------------------------------
// The getrlimit FFI lives in the audited OS boundary (clarus_server::ffi).

// -- Data-directory startup guard ---------------------------------------
// One instance per data directory: a second
// Clarus pointed at the same data directory must not start. This is NOT a
// lock - it is a single create_new at startup with one liveness check; no
// TTL/stale/steal policy at runtime. A crash leaves a file with a dead pid,
// which the next start overwrites (nginx style, like the pid-file).
//
// Cross-host NAS: a pid from another host looks dead locally, so the guard
// yields and both instances start - that is the documented cluster scenario
// (Valkey/cluster-redis), NOT handled here.

/// Removes the data-dir guard file when dropped; held for the process lifetime.
struct DataDirGuard {
    path: std::path::PathBuf,
}

impl Drop for DataDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Decide how to handle an existing guard file.
///
/// Returns `Some(pid_str)` when the existing guard names a live process - the
/// startup path must refuse. Returns `None` when the holder is dead or the
/// content is empty/unparseable - the guard may be taken over (nginx style).
fn guard_live_holder(old_content: &str) -> Option<String> {
    let pid = old_content
        .trim()
        .split(':')
        .next()
        .and_then(|p| p.parse::<u32>().ok());
    match pid {
        // A guard naming OUR pid is the leftover of a previous run that had
        // the same number: in a container Clarus is PID 1 every time, so the
        // guard written before a kill -9 or an OOM reports "alive" (PID 1 is
        // now this process) and every restart refuses to start until the file
        // is removed by hand. A pid file cannot name itself as a live foreign
        // holder, so this is always stale.
        Some(pid) if pid == std::process::id() => None,
        Some(pid) if is_process_alive(pid) => Some(old_content.trim().to_string()),
        _ => None,
    }
}

/// Acquire the `{data_root}/.clarus-data.lock` startup guard, or refuse to
/// start when another live instance already holds it.
///
/// Content is `pid:port` for diagnostics. If the file already exists but its
/// pid is dead (or the content is empty/unparseable), the guard is taken over
/// (nginx style, as the pid-file does). Returns the guard to hold for life.
fn acquire_data_dir_guard(data_root: &std::path::Path, port: u16) -> DataDirGuard {
    let guard_path = data_root.join(".clarus-data.lock");
    let pid = std::process::id();
    let payload = format!("{pid}:{port}");

    // A fresh deployment has no data directory yet; create it so the guard
    // file (and later the manifests/idx/audit subdirs) have a home.
    if let Err(e) = std::fs::create_dir_all(data_root) {
        eprintln!(
            "[FATAL] Cannot create data directory {}: {}",
            data_root.display(),
            e
        );
        std::process::exit(1);
    }

    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&guard_path)
    {
        Ok(mut f) => {
            use std::io::Write;
            let _ = writeln!(f, "{payload}");
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let old = std::fs::read_to_string(&guard_path).unwrap_or_default();
            match guard_live_holder(&old) {
                Some(live) => {
                    eprintln!(
                        "[FATAL] Data directory {} is already in use by pid {} \
                         (port {}). One instance per data directory (see clarus.conf \
                         [btree]/[storage]); remove {} manually if stale.",
                        data_root.display(),
                        live,
                        port,
                        guard_path.display()
                    );
                    std::process::exit(1);
                }
                None => {
                    eprintln!(
                        "[WARN] Stale data-dir guard {} (pid={} is dead). Overwriting.",
                        guard_path.display(),
                        old.trim()
                    );
                    let _ = std::fs::remove_file(&guard_path);
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&guard_path)
                    {
                        use std::io::Write;
                        let _ = writeln!(f, "{payload}");
                    }
                }
            }
        }
        Err(e) => {
            eprintln!(
                "[FATAL] Cannot create data-dir guard {}: {}",
                guard_path.display(),
                e
            );
            std::process::exit(1);
        }
    }

    DataDirGuard { path: guard_path }
}

// -- manifest-degraded startup sanity check ------------------------
//
// Returns `true` if the CAS root contains at least one `*.dcm` blob. Used to
// distinguish a *truly fresh* store (CAS empty -> normal startup) from a
// *lost* `manifests/` path (CAS non-empty -> server is blind, must rebuild).
// Walks the unified 2-2 geometry `{root}/{xx}/{yy}/{hash}.dcm` recursively.
fn cas_dir_has_blobs(cas_root: &std::path::Path) -> bool {
    let mut stack: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(cas_root) {
        for e in rd.flatten() {
            stack.push(e.path());
        }
    }
    while let Some(p) = stack.pop() {
        let md = match std::fs::metadata(&p) {
            Ok(md) => md,
            Err(_) => continue,
        };
        if md.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                for e in rd.flatten() {
                    stack.push(e.path());
                }
            }
        } else if p.extension().is_some_and(|ext| ext == "dcm") {
            return true;
        }
    }
    false
}

// --- Startup hygiene: orphan temp-file sweep -----------------------------
//
// STOW writes per-instance temp files into the CAS `_tmp/` dir and removes
// them on every normal path. A hard kill (second SIGINT) can leave orphans.
// The data-dir guard (above) guarantees no other live process writes here,
// so every regular file in `_tmp/` at startup is an orphan and is removed.
// Non-fatal by design: a file that cannot be removed is warned about and
// skipped - startup must proceed either way.
//
// Returns (files removed, bytes removed).
fn sweep_tmp_orphans(tmp_dir: &std::path::Path) -> (u64, u64) {
    let entries = match std::fs::read_dir(tmp_dir) {
        Ok(e) => e,
        Err(_) => return (0, 0), // no _tmp dir: nothing to sweep
    };
    let mut removed: u64 = 0;
    let mut bytes: u64 = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue; // subdirectories are not orphans; keep them
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                removed += 1;
                bytes = bytes.saturating_add(meta.len());
            }
            Err(e) => {
                clarus_server::clog_warn!(
                    path = %path.display(),
                    error = %e,
                    "tmp sweep: cannot remove orphan temp file"
                );
            }
        }
    }
    (removed, bytes)
}

// -- Main ----------------------------------------------------------

fn main() {
    #[cfg(windows)]
    clarus_server::ffi::configure_windows_console();

    // Parse CLI flags
    let flags = parse_cli_flags();

    if flags.show_version {
        println!("clarus v{}", env!("CARGO_PKG_VERSION"));
        return;
    }

    // KHC-002: --config has a default: clarus.conf next to the executable
    let config_path = flags.config_path.clone().unwrap_or_else(|| {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        exe_dir
            .join("clarus.conf")
            .to_str()
            .unwrap_or("clarus.conf")
            .to_string()
    });
    let path = std::path::Path::new(&config_path);

    if !path.exists() {
        eprintln!("[FATAL] config file not found: {}", path.display());
        eprintln!("Create a clarus.conf file or use --config <path>");
        std::process::exit(1);
    }

    // Security: verify config file permissions (chmod 600) - use eprintln (tracing not up yet)
    if let Err(e) = config::verify_config_permissions(path) {
        eprintln!("{e}");
        std::process::exit(1);
    }

    let args = match config::Config::from_file(path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("[FATAL] Error parsing {}: {}", path.display(), e);
            std::process::exit(1);
        }
    };

    // -t / --test-config: validate and exit (no runtime needed)
    if flags.test_config {
        println!("Config OK: {}", path.display());
        println!("  host: {}:{}", args.host, args.port);
        println!("  storage: {}", args.storage_dir);
        println!("  log_level: {}", args.log_level);
        println!("  max_instance_size_mb: {}", args.max_instance_size_mb);
        println!(
            "  max_frame_bytes: {}",
            clarus_server::config::size_canonical(args.max_frame_bytes)
        );
        println!(
            "  max_frames_per_instance: {}",
            args.max_frames_per_instance
        );
        println!("  header_timeout_secs: {}", args.header_timeout_secs);
        println!("  body_timeout_secs: {}", args.body_timeout_secs);
        println!("  keepalive_timeout_secs: {}", args.keepalive_timeout_secs);
        println!("  send_timeout_secs: {}", args.send_timeout_secs);
        println!("  drain_timeout_secs: {}", args.drain_timeout_secs);
        println!("  max_connections: {}", args.max_connections);
        println!(
            "  max_json_body: {}",
            clarus_server::config::size_canonical(args.max_json_body)
        );
        println!(
            "  max_part_header: {}",
            clarus_server::config::size_canonical(args.max_part_header)
        );
        println!("  hostname: {}", args.hostname);
        println!("  require_proxy_identity: {}", args.require_proxy_identity);
        println!(
            "  anyclass_enable: {} (IOCM 113037/113038/113039 may hide diagnostic images)",
            args.anyclass_enable
        );
        println!(
            "  expose_rejected_cidrs: {} net(s) (non-local nets allowed to query rejected instances; empty = local only)",
            args.expose_rejected_cidrs.len()
        );
        println!(
            "  modifying_system: {} (0400,0563 for update alibis)",
            args.resolved_modifying_system()
        );
        println!(
            "  orphan_gc_interval_sec: {} (0 = orphan GC disabled)",
            args.btree.orphan_gc_interval_sec
        );
        #[cfg(feature = "s3")]
        if !args.s3.endpoint.is_empty() {
            println!("  s3: {} / {}", args.s3.endpoint, args.s3.bucket);
        }
        // -- [transcode] bounded pool (2608): effective values ------
        println!(
            "  transcode_max_concurrent: {} (global permit pool, default = cores/4 clamp 1..32)",
            args.transcode.max_concurrent
        );
        println!(
            "  transcode_max_megapixels: {} (pre-decode pixel budget, default 64)",
            args.transcode.max_megapixels
        );
        // -- [workflow] MWL + MPPS (PS3.18 14/15): effective values -----
        println!(
            "  workflow.enabled: {} (MWL/MPPS surface; off by default)",
            args.workflow.enabled
        );
        println!("  workflow.dir: {}", args.workflow.resolved_dir());
        println!(
            "  workflow.retention_days: {} (0 = keep forever)",
            args.workflow.retention_days
        );
        println!(
            "  workflow.mwl_default_limit: {} (0 = unlimited)",
            args.workflow.mwl_default_limit
        );
        println!(
            "  workflow.mwl_max_responses: {} (hard ceiling)",
            args.workflow.mwl_max_responses
        );
        // -- [ups.coding_schemes] accepted non-99 designators -----
        // "99*" schemes are always accepted (PS3.3 8.2) and are deliberately
        // not listed: printing them would suggest they had to be declared.
        println!(
            "  ups.coding_schemes: {} (public designators accepted besides the implicit 99* private ones)",
            if args.ups.coding_schemes.is_empty() {
                "-".to_string()
            } else {
                args.ups.coding_schemes.join(", ")
            }
        );
        return;
    }

    // own std writer + filter (no tracing pipeline). In debug builds
    // everything also goes to stderr via clog; in release the whitelist/level
    // filter applies. The handle is kept alive for the process lifetime.
    let log_dir = path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(&args.log_dir);
    let _log_writer = clarus_server::log_writer::init(&log_dir, args.log_level.to_clog());

    // Advisory open-file (NOFILE) limit check - warn only.
    clarus_server::ffi::check_fd_limit(args.max_connections);
    // -- nginx-style PID file ------------------------------------------
    // No configuration: the platform default (`/var/run/clarus-{port}.pid`
    // on Linux, next to the config elsewhere) with a fallback next to the
    // config. A write failure is a warning, not a fatal error: no PID file
    // is a loss of observability, not a reason to refuse to serve. A live
    // foreign pid IS fatal.
    let pid_path = acquire_pid_file(path, args.port);

    // -- Data-directory startup guard ----------------------------------
    // One instance per data directory. Taken over `storage_dir`, not just the
    // B-tree root: `sweep_tmp_orphans` cleans orphaned temp files under the
    // storage tree at startup, and a guard that a sweep can delete guards
    // nothing. `storage_dir` contains the B-tree root in every default layout,
    // so the broader directory is the safe superset.
    let data_root = std::path::PathBuf::from(&args.storage_dir);
    let _data_guard = acquire_data_dir_guard(&data_root, args.port);

    // Security: the audit dir (the only evidence of deletions) must be
    // owner-only (0700 on Unix) BEFORE serving traffic. Unix enforces +
    // self-fixes (anomaly recorded in the trail); Windows warns only
    // (NTFS ACL model). Same hard-fail pattern as the config check above.
    if let Err(e) = clarus_server::audit_log::ensure_audit_dir_permissions(std::path::Path::new(
        &args.btree.audit_dir,
    )) {
        eprintln!("{e}");
        std::process::exit(1);
    }
    // The access journal joins the SAME startup permission check (fail-closed
    // per W1's decision) - no second policy forked.
    if let Err(e) = clarus_server::audit_log::ensure_audit_dir_permissions(std::path::Path::new(
        &args.btree.access_dir,
    )) {
        eprintln!("{e}");
        std::process::exit(1);
    }
    // Access-audit journal (ATNA-lite, 2808): bounded ring + writer thread.
    // The handle lives for the process lifetime; dropping it at shutdown
    // drains the ring (graceful drain).
    let _access_writer = clarus_server::access_log::init(
        std::path::Path::new(&args.btree.access_dir),
        args.audit.access_retention_days,
        args.audit.access_prune_dry_run,
    );

    clarus_server::clog_event!(
        crate::clog::Level::Info,
        "STARTUP_CONFIG",
        "clarus starting",
        version = env!("CARGO_PKG_VERSION"),
        config = %path.display(),
        host = %args.host,
        port = %args.port,
        log_level = %args.log_level,
        pid = std::process::id(),
        pid_file = %pid_path.display(),
        "startup"
    );
    clarus_server::clog_event!(
        crate::clog::Level::Info,
        "STARTUP_CONFIG",
        "",
        storage_dir = %args.storage_dir,
        "storage configuration"
    );
    clarus_server::clog_event!(
        crate::clog::Level::Info,
        "STARTUP_CONFIG",
        "",
        "runtime configuration"
    );

    // Server runs on std threads end-to-end (no tokio runtime).
    clarus_server::clog_event!(
        crate::clog::Level::Info,
        "STARTUP_CONFIG",
        "",
        "thread-per-connection runtime (no tokio)"
    );

    {
        // Build shared application state (B-tree manifests + CAS; no SQLite).
        // CAS lives under storage_dir/cas per nosql-sprints.md directory layout:
        // ./data/cas/{xx}/{yy}/{blake3}.dcm (source of truth, 2-2 sharding).
        let cas_root = std::path::PathBuf::from(&args.storage_dir).join("cas");
        let cas = ContentAddressedStorage::new(cas_root)
            .expect("Failed to initialize content-addressed storage");

        // Startup hygiene: drop orphans of a previous hard-killed run. The
        // data-dir guard (above) guarantees no live process shares the dir.
        let (swept_files, swept_bytes) = sweep_tmp_orphans(&cas.temp_dir());
        if swept_files > 0 {
            clarus_server::clog_info!(
                "tmp sweep: removed {} orphan temp file(s), {} bytes",
                swept_files,
                swept_bytes,
            );
        }

        // Cold storage (FR-017b): second CAS instance for file-system tiered storage.
        // Reuses the same ContentAddressedStorage - no new type, no new trait.
        let cold_cas = if let Some(ref cold_path) = args.cold_path {
            match ContentAddressedStorage::new(std::path::PathBuf::from(cold_path)) {
                Ok(cas) => {
                    clarus_server::clog_info!(cold_path = %cold_path, "Cold storage initialized");
                    Some(cas)
                }
                Err(e) => {
                    clarus_server::clog_error!(cold_path = %cold_path, error = %e, "Failed to initialize cold storage - continuing without cold tier");
                    None
                }
            }
        } else {
            None
        };

        clarus_server::disk_health::spawn_disk_probe(std::path::PathBuf::from(&args.storage_dir));

        // Uses tasklist on Windows (no unsafe, no extra crates).
        // Runs independently of tokio - survives runtime shutdown.
        std::thread::Builder::new()
            .name("rss-probe".into())
            .spawn(move || {
                loop {
                    let rss = get_process_rss_bytes();
                    clarus_server::metrics::update_process_rss_bytes(rss);
                    std::thread::sleep(std::time::Duration::from_secs(10));
                }
            })
            .expect("Failed to spawn RSS probe thread");

        // Free-space probe for the /health/status DISK indicator. Runs
        // independently every ~20s (well below the 15-30s spec band) so the
        // endpoint only reads cheap atomics and never blocks on statvfs.
        let disk_free_dir = std::path::PathBuf::from(&args.storage_dir);
        std::thread::Builder::new()
            .name("disk-free-probe".into())
            .spawn(move || {
                loop {
                    clarus_server::metrics::update_disk_free_gauges(&disk_free_dir);
                    std::thread::sleep(std::time::Duration::from_secs(20));
                }
            })
            .expect("Failed to spawn disk-free probe thread");

        // S3-compatible object storage (cold tier fallback).
        // Background connect on a std thread - server starts instantly, S3
        // connects asynchronously. `OnceLock` is set when connection succeeds.
        #[cfg(feature = "s3")]
        let s3_cell: Arc<
            std::sync::OnceLock<clarus_adapters::storage::s3_client::S3Client>,
        > = Arc::new(std::sync::OnceLock::new());
        #[cfg(feature = "s3")]
        {
            use clarus_adapters::storage::s3::ObjectStore;
            use clarus_adapters::storage::s3_client::S3Client;
            let s3_cell_bg = s3_cell.clone();
            let endpoint = args.s3.endpoint.clone();
            let access_key = args.s3.access_key.clone();
            let secret_key = args.s3.secret_key.clone();
            let region = args.s3.region.clone();
            let bucket = args.s3.bucket.clone();
            let prefix = args.s3.prefix.clone();
            std::thread::Builder::new()
                .name("s3-connect".into())
                .spawn(move || {
                    let client = S3Client::new(
                        &endpoint,
                        &access_key,
                        &secret_key,
                        &region,
                        &bucket,
                        &prefix,
                    );
                    // S3Client is blocking (reqwest::blocking) - runs
                    // directly on this std thread, no tokio runtime needed.
                    let result = client.put_object("_health", b"ok".to_vec(), "text/plain");
                    match result {
                        Ok(_etag) => {
                            clarus_server::clog_info!(
                                endpoint = %endpoint,
                                bucket = %bucket,
                                "S3 storage connected (background)"
                            );
                            clarus_server::metrics::record_s3_health(true, 0);
                            let _ = s3_cell_bg.set(client);
                        }
                        Err(e) => {
                            clarus_server::clog_error!(
                                error = %e,
                                endpoint = %endpoint,
                                "S3 background connect failed - continuing without S3"
                            );
                            clarus_server::metrics::record_s3_health(false, 0);
                        }
                    }
                })
                .expect("Failed to spawn S3 connect thread");
        }

        let codec_registry = clarus_adapters::codec::CodecRegistry::default();

        // 2608: bounded transcode/render permit pool. N comes from the config
        // (`[transcode] transcode_max_concurrent`, or the automatic default
        // clamp(cores/4, 1..=32)) - a hard cap on concurrent decodes/renders
        // so the worst case is bounded in both CPU and memory, never an
        // unbounded queue.
        let transcode_pool =
            clarus_server::state::TranscodePool::new(args.transcode.max_concurrent);

        // B-tree index writer: writes dates/modality/sop_owner .idx rows for
        // every accepted STOW (the SQLite tier it once mirrored was excised
        // in - this is now the primary writer).
        let shadow =
            clarus_server::shadow::ShadowTier::new(std::path::PathBuf::from(&args.btree.idx_dir));
        clarus_server::clog_info!(idx_dir = %args.btree.idx_dir, "B-tree index writer up");

        // preload recent immutable .idx files into the page cache so
        // the first QIDO queries over hot days pay zero page faults. The
        // mappings are dropped immediately below - this warms the OS page
        // cache, it does not hold RAM.
        {
            let idx_root = std::path::PathBuf::from(&args.btree.idx_dir);
            let today =
                clarus_core::idx::rollover::day_key_from_system_time(std::time::SystemTime::now());
            let maps = clarus_core::idx::preload::preload_all(
                std::path::Path::new(""),
                &idx_root,
                today,
                args.btree.preload_days,
            );
            clarus_server::clog_info!(
                preloaded = maps.len(),
                "index preload: mapped recent immutable .idx files"
            );
            // Drop the mappings immediately: the mappings were for warming the
            // page cache, which the kernel retains after unmap.
            drop(maps);
        }

        // B-tree query scanner - the read side.
        // Answers study QIDO from `btree.manifest_dir` (CBOR manifests) using
        // the `idx/` accelerators written by the shadow tier.
        let index = clarus_core::query::StudyQidoIndex::new();
        let build_index = index.clone();
        let lazy_manifest_dir = std::path::PathBuf::from(&args.btree.manifest_dir);
        let mut study_scanner = clarus_core::query::StudyScanner::new(
            std::path::PathBuf::from(&args.btree.manifest_dir),
            std::path::PathBuf::from(&args.btree.idx_dir),
        );
        study_scanner.attach_index(index);
        // build the RAM study index lazily in the background. The server
        // starts immediately; until the build finishes the cold QIDO path falls
        // back to the manifest scan (correct, just slower). `kill -9` recovery
        // simply rebuilds on the next start - no on-disk index to get stale.
        std::thread::Builder::new()
            .name("study-index-build".into())
            .spawn(move || {
                // manifest-degradation (Etap 2): the RAM index builder
                // silently skips corrupt manifests; surface each through the
                // live MANIFEST_CORRUPT / MANIFEST_DEGRADED signal so a study
                // that never reaches the index is not invisible until restart.
                let corrupt = build_index.build_and_swap(&lazy_manifest_dir);
                if corrupt > 0 {
                    clarus_server::metrics::MANIFEST_CORRUPT
                        .fetch_add(corrupt, std::sync::atomic::Ordering::Relaxed);
                    clarus_server::metrics::MANIFEST_DEGRADED
                        .store(1, std::sync::atomic::Ordering::Relaxed);
                    clarus_server::clog_warn!(
                        corrupt = corrupt,
                        "RAM study index: corrupt manifests skipped during build"
                    );
                }
                clarus_server::clog_info!(
                    indexed = %build_index.len(),
                    "RAM study index built"
                );
            })
            .expect("spawn study-index-build");
        clarus_server::clog_info!(
            manifest_dir = %args.btree.manifest_dir,
            "B-tree study scanner initialized (QIDO primary path, RAM index)"
        );

        let app_state = AppState {
            cas,
            codec_registry,
            transcode_pool,
            ups_events: DualEventSink::new(),
            start_time: std::time::Instant::now(),
            metrics_ring: Arc::new(std::sync::RwLock::new(std::collections::VecDeque::new())),
            log_buffer: Arc::new(std::sync::RwLock::new(std::collections::VecDeque::new())),
            maintenance: Arc::new(std::sync::RwLock::new(
                clarus_server::state::MaintenanceState::default(),
            )),
            #[cfg(feature = "s3")]
            s3: s3_cell,
            cold_cas,
            shadow,
            study_scanner,
            config: args.clone(),
            workitems_dir: std::path::PathBuf::from(&args.btree.root).join("workitems"),
            commitments_dir: std::path::PathBuf::from(&args.btree.root).join("commitments"),
            readiness: clarus_server::handlers::readiness::Readiness::new(),
        };

        // [ups] push_attempts: the push worker is a free function with no
        // AppState, so the validated value is published once here. The config
        // parse already range-checked it to [1..5].
        clarus_server::push::set_push_attempts(app_state.config.ups.push_attempts);

        // / flush the CBOR journal ring into the SAME
        // writer as `clog` (one daily-rotated file, one writer thread). One
        // background thread drains the journal tokens and pushes the formatted
        // plaintext lines into the log-writer's bounded channel, so the handler
        // hotpath never touches disk and there is no second writer/file.
        {
            let shadow_flush = app_state.shadow.clone();
            std::thread::Builder::new()
                .name("journal-flush".into())
                .spawn(move || {
                    let mut out: Vec<u8> = Vec::new();
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        shadow_flush.drain_tokens(&mut out);
                        if !out.is_empty() {
                            clarus_server::clog::push_plaintext(std::mem::take(&mut out));
                        }
                    }
                })
                .expect("Failed to spawn journal-flush thread");
        }

        // Lazy Recovery - reconcile manifests <-> .idx at
        // startup. Runs once in the background so the server accepts requests
        // immediately; reports the stale-object count into
        // `clarus_idx_stale_objects` (0 = the B-tree tier is ready to serve).
        {
            let manifests_dir = std::path::PathBuf::from(&args.btree.manifest_dir);
            let idx_dir = std::path::PathBuf::from(&args.btree.idx_dir);
            // the shared hot CAS root lives under storage_dir/cas.
            // Captured here so the sanity check can tell a lost manifests/ path
            // from a truly fresh store.
            let cas_root = std::path::PathBuf::from(&args.storage_dir).join("cas");
            std::thread::Builder::new()
                .name("lazy-recovery".into())
                .spawn(move || {
                    match clarus_core::recovery::reconcile_idx(&manifests_dir, &idx_dir) {
                        Ok(report) => {
                            clarus_server::metrics::CLARUS_IDX_STALE_OBJECTS
                                .store(report.entries_added, std::sync::atomic::Ordering::Relaxed);
                            // surface corrupt manifests through
                            // the /health + /metrics degraded signal.
                            if report.corrupt > 0 {
                                clarus_server::metrics::MANIFEST_CORRUPT
                                    .store(report.corrupt, std::sync::atomic::Ordering::Relaxed);
                                clarus_server::metrics::MANIFEST_DEGRADED
                                    .store(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            clarus_server::clog_info!(
                                scanned = report.manifests_scanned,
                                added = report.entries_added,
                                corrupt = report.corrupt,
                                "Lazy recovery: B-tree idx reconciled with manifests"
                            );
                        }
                        Err(clarus_core::recovery::ReconcileError::NoManifestDir(_)) => {
                            // distinguish a truly fresh store from a
                            // lost manifests/ path. If CAS is non-empty but no
                            // manifests exist, the server is blind (cannot serve
                            // QIDO/WADO) - that is the cas->manifests rebuild gap.
                            if cas_dir_has_blobs(&cas_root) {
                                clarus_server::metrics::MANIFEST_MISSING
                                    .store(1, std::sync::atomic::Ordering::Relaxed);
                                clarus_server::metrics::MANIFEST_DEGRADED
                                    .store(1, std::sync::atomic::Ordering::Relaxed);
                                clarus_server::clog_error!(
                                    "CAS is non-empty but manifests/ is missing or empty \
                                     (server is blind to stored studies). Run \
                                     POST /admin/v1/recovery/rebuild to rebuild \
                                     manifests/idx/ngrams from CAS."
                                );
                            } else {
                                // Fresh store (first boot or a wiped data dir):
                                // nothing to reconcile. Normal state, no warning.
                            }
                        }
                        Err(e) => {
                            clarus_server::clog_warn!(
                                error = %e,
                                "Lazy recovery skipped (cannot read manifests dir)"
                            );
                        }
                    }
                })
                .expect("Failed to spawn lazy-recovery thread");
        }

        // Spawn storage tier migration workers (FR-017b).
        // Three independent std::threads: cold, S3, emergency.
        // Uses cron schedules from [storage_tiers] instead of hardcoded 1-hour sleep.
        clarus_server::migration_worker::spawn_migration_workers(app_state.clone());

        // spawn the lazy erase-worker (non-blocking file eraser).
        // A delete tombstones a study; this background thread physically
        // erases the CAS files and lifts the tombstone. Same std-thread
        // pattern as journal-flush / lazy-recovery.
        clarus_server::erase_worker::spawn_erase_worker(app_state.clone());

        // UPS retention: one pass now, then every 24h, removing workitems
        // whose state is final and whose last update is past
        // `[ups] final_state_retention_days`. Start-up is a no-op when the
        // dial is 0. Same std-thread pattern as the push and erase workers.
        clarus_server::ups_retention::spawn_ups_retention(app_state.clone());

        // Storage Commitment results follow the same discipline: the sweep
        // removes result files older than `[commitment] result_retention_days`
        // and is a no-op when the dial is 0. Check Commit Result answers 404
        // for a swept Transaction UID, which PS3.18 Table 13.5.3-1 allows.
        clarus_server::commitment_retention::spawn_commitment_retention(app_state.clone());

        // MWL/MPPS retention: the same discipline again. One pass now, then
        // every 24h, removing MWL rows and FINAL MPPS rows older than
        // `[workflow] retention_days`. An IN PROGRESS MPPS row is never
        // swept, and the whole thread is absent when the dial is 0 or the
        // surface is disabled.
        clarus_server::workflow_retention::spawn_workflow_retention(app_state.clone());

        // Runtime B-tree modality-idx fixer (2208 idx runtime heal): a small
        // std thread drains an mpsc channel of exact-UID heal requests poked
        // by the QIDO path, appending the missing modality-idx row when a
        // study is in the manifest but out of sync with the accelerator. Same
        // bounded-channel / try_send pattern as log-writer.
        clarus_server::idx_fixer::spawn(std::path::PathBuf::from(&app_state.study_scanner.idx_dir));

        // nginx-way: all auth is handled by reverse proxy.
        // Server is open on localhost - nginx enforces Basic Auth / mTLS / JWT / IP-whitelist.
        clarus_server::clog_warn!(
            "AUTHENTICATION IS DISABLED — server accepts all requests. \
             Place nginx/ALB/mTLS in front of Clarus before production deployment. \
             Configure the proxy per clarus.conf (trusted_proxies + admin_cidrs notes)"
        );
        let state = Arc::new(app_state);
        // The synchronous transport (serve.rs) owns the wire end-to-end; there
        // is no framework layer above it. The axum/tower stack was removed in
        // Sprint E.

        // Input Readiness sweeper: promotes a generated workitem to READY
        // (and publishes its offer) once its study has stopped receiving
        // instances. Started only when this process is the Task Producer, so a
        // deployment whose Producer is a modality, a RIS or an external
        // orchestrator pays neither the thread nor the tick.
        if state.config.ups.task_producer {
            clarus_server::handlers::readiness::spawn_sweeper(Arc::clone(&state));
        }

        // Metrics: background thread - update storage gauges (physical .dcm
        // walk + manifest-aware instances/audited counts)
        let storage_dir_clone = std::path::PathBuf::from(&args.storage_dir);
        let metrics_manifest_dir = std::path::PathBuf::from(&args.btree.manifest_dir);
        std::thread::Builder::new()
            .name("metrics-storage".into())
            .spawn(move || {
                // Metrics: first measurement immediately at startup
                clarus_server::metrics::update_storage_gauges_full(
                    &storage_dir_clone,
                    Some(&metrics_manifest_dir),
                );
                // Metrics: then every 300 seconds (5 min) - storage metrics change slowly
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(300));
                    clarus_server::metrics::update_storage_gauges_full(
                        &storage_dir_clone,
                        Some(&metrics_manifest_dir),
                    );
                }
            })
            .expect("Failed to spawn metrics-storage thread");

        // 3008: sidecar snapshot scanner for GET /admin/v1/storage/audit-copies.
        // Reads manifests/ only - never CAS. First pass immediately, then 60 s.
        clarus_server::audit_copies::spawn(std::path::PathBuf::from(&args.btree.manifest_dir));

        // Bind and serve
        let addr: SocketAddr = format!("{}:{}", args.host, args.port)
            .parse()
            .expect("Invalid host:port combination");

        clarus_server::clog_event!(
            crate::clog::Level::Info,
            "LISTEN",
            "",
            host = %args.host,
            port = %args.port,
            "listening on {addr}"
        );

        let listener = std::net::TcpListener::bind(addr).unwrap_or_else(|e| {
            let errno = e.raw_os_error().unwrap_or(0);
            eprintln!("[emerg] bind() to {} failed ({}: {})", addr, errno, e);
            std::process::exit(1);
        });

        // Graceful shutdown coordination (E-008): atomics + a std condvar,
        // independent of the tokio runtime so Ctrl+C is honoured on Windows.
        let shutdown = clarus_core::shutdown::Shutdown::new();

        let term = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        signal_hook::flag::register(signal_hook::consts::SIGTERM, term.clone())
            .expect("Failed to register SIGTERM handler");
        signal_hook::flag::register(signal_hook::consts::SIGINT, term.clone())
            .expect("Failed to register SIGINT handler");
        // On Windows, SIGBREAK is sent on Ctrl+C in some terminals.
        //
        // ONE flag per signal: the FIRST signal is a graceful stop, and the
        // signal thread below latches `term` to turn the SECOND one into
        // `request_force`. A second registration writing a separate `force`
        // flag made every first signal look like a fast shutdown and cut
        // in-flight STOW/WADO.
        #[cfg(windows)]
        signal_hook::flag::register(signal_hook::consts::SIGBREAK, term.clone())
            .expect("Failed to register SIGBREAK handler");

        // Signal thread: only watches the flag and requests a graceful stop.
        // No process::exit() timer here - the drain loop owns shutdown.
        let sd_signal = shutdown.clone();
        std::thread::spawn(move || {
            while !term.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            clarus_server::clog_event!(
                crate::clog::Level::Info,
                "SHUTDOWN",
                "",
                "SIGINT/SIGTERM received via OS signal - initiating shutdown"
            );
            sd_signal.request_stop();
            // Latch the flag back and watch for the second signal: during
            // drain a second Ctrl+C forces immediate exit (nginx fast
            // shutdown). The race window between request_stop and the store
            // is humanly unreachable, so the latch is good enough.
            term.store(false, std::sync::atomic::Ordering::Relaxed);
            while !term.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            clarus_server::clog_event!(
                crate::clog::Level::Info,
                "SHUTDOWN",
                "",
                "Second signal received - forcing immediate exit"
            );
            sd_signal.request_force();
        });

        // Accept thread: runs the synchronous transport (accept + conn loops).
        let sd_accept = shutdown.clone();
        let st_accept = state.clone();
        let timeouts = clarus_core::http::conn::Timeouts::from_secs(
            args.header_timeout_secs,
            args.body_timeout_secs,
            args.keepalive_timeout_secs,
            args.send_timeout_secs,
        );
        let max_conn = args.max_connections;
        let accept_thread = std::thread::spawn(move || {
            clarus_server::serve::serve(listener, st_accept, sd_accept, timeouts, max_conn);
        });

        // Coordinate shutdown: stop accepting, drain in-flight requests, then
        // stop the DbActor only once active connections reach zero (or the
        // drain deadline beats SO_SNDTIMEO). A second signal forces a fast exit.
        // The deadline is anchored at the moment the stop signal actually
        // arrived. Measured from here it would start before the wait, so a
        // signal that took its time (a sleeping supervisor, a slow terminal)
        // shortened the drain by exactly that delay - and a shutdown that
        // arrives late is when there is the most in flight to drain.
        let sd_drain = shutdown.clone();
        // Wait for the stop signal (the accept thread stops accepting on it).
        while !sd_drain.stopping() {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let drain_deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(args.drain_timeout_secs);
        clarus_server::clog_event!(
            crate::clog::Level::Info,
            "SHUTDOWN",
            "",
            "Shutting down: draining in-flight connections..."
        );
        loop {
            if sd_drain.forced() {
                clarus_server::clog_event!(
                    crate::clog::Level::Info,
                    "SHUTDOWN",
                    "",
                    "Second signal received - immediate exit"
                );
                break;
            }
            if sd_drain.active() == 0 {
                break;
            }
            if std::time::Instant::now() >= drain_deadline {
                clarus_server::clog_warn!(
                    "Drain deadline reached with {} active connection(s) - proceeding",
                    sd_drain.active()
                );
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // Order: drain -> join accept thread -> flush the access journal ->
        // pid removed. The journal must be flushed before the process can
        // return: its writer is a detached thread and the last lines queued
        // here are the ones that describe the requests just drained.
        let _ = accept_thread.join();
        _access_writer.shutdown();

        // Clean up PID file on graceful exit (nginx-style).
        if let Err(e) = std::fs::remove_file(&pid_path) {
            clarus_server::clog_warn!("Failed to remove PID file {}: {}", pid_path.display(), e);
        } else {
            clarus_server::clog_info!("Removed PID file {}", pid_path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawn and reap a short-lived child, returning its now-dead PID.
    ///
    /// PID 0 is the System Idle Process on Windows (always "alive"), so it is
    /// NOT a reliable dead pid there - reaping a real child is deterministic
    /// and cross-platform.
    fn dead_pid() -> u32 {
        #[cfg(unix)]
        {
            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg("exit 0")
                .spawn()
                .expect("spawn sh");
            let pid = child.id();
            let _ = child.wait();
            pid
        }
        #[cfg(windows)]
        {
            let mut child = std::process::Command::new("cmd")
                .args(["/C", "exit"])
                .spawn()
                .expect("spawn cmd");
            let pid = child.id();
            let _ = child.wait();
            pid
        }
        #[cfg(not(any(unix, windows)))]
        {
            u32::MAX
        }
    }

    /// First acquire creates the guard file with the current pid.
    #[test]
    fn guard_first_acquire_writes_our_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let guard_path = dir.path().join(".clarus-data.lock");
        let _guard = acquire_data_dir_guard(dir.path(), 8024);
        let content = std::fs::read_to_string(&guard_path).expect("guard file present");
        let pid = content
            .trim()
            .split(':')
            .next()
            .unwrap()
            .parse::<u32>()
            .unwrap();
        assert_eq!(pid, std::process::id(), "guard holds our pid");
    }

    /// Drop removes the guard file.
    #[test]
    fn guard_drop_removes_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let guard_path = dir.path().join(".clarus-data.lock");
        {
            let _guard = acquire_data_dir_guard(dir.path(), 8024);
            assert!(guard_path.exists(), "guard created");
        }
        assert!(!guard_path.exists(), "guard removed on drop");
    }

    /// An existing guard naming OUR pid is the leftover of a previous run, not
    /// a conflict: in a container Clarus is PID 1 on every start, so the guard
    /// surviving a kill -9 reports "alive" (PID 1 is now this process) and
    /// every restart would refuse until an operator removed the file by hand.
    #[test]
    fn guard_own_pid_is_a_stale_leftover() {
        let own = format!("{}:8024", std::process::id());
        assert_eq!(
            guard_live_holder(&own),
            None,
            "a guard naming our own pid is a leftover of a same-pid run"
        );
    }

    /// A guard naming a DIFFERENT live pid is a real conflict (must refuse).
    ///
    /// The live holder is our own parent process, which is by construction
    /// alive and has a pid distinct from ours.
    #[test]
    fn guard_foreign_live_pid_is_a_conflict() {
        let parent = std::process::id();
        let other = (1..=u32::MAX)
            .find(|p| *p != parent && is_process_alive(*p))
            .expect("some other pid is alive");
        let alive = format!("{other}:8024");
        let holder = guard_live_holder(&alive);
        assert_eq!(
            holder.as_deref(),
            Some(alive.as_str()),
            "foreign live pid => refuse"
        );
    }

    /// An existing guard naming a DEAD (reaped child) pid is stale - takeover.
    #[test]
    fn guard_dead_pid_is_stale() {
        let dead = format!("{}:8024", dead_pid());
        assert_eq!(guard_live_holder(&dead), None, "dead pid => takeover");
    }

    /// Empty / unparseable guard content is treated as dead (takeover).
    #[test]
    fn guard_empty_or_unparseable_is_stale() {
        assert_eq!(guard_live_holder(""), None, "empty => stale");
        assert_eq!(
            guard_live_holder("not-a-pid:8024"),
            None,
            "unparseable => stale"
        );
        assert_eq!(guard_live_holder("   "), None, "whitespace => stale");
        assert_eq!(
            guard_live_holder("99999999:8024"),
            None,
            "dead-looking pid => stale"
        );
    }

    /// Rewriting over a stale (dead pid) guard succeeds: file is replaced.
    #[test]
    fn guard_takes_over_stale_guard() {
        let dir = tempfile::tempdir().expect("tempdir");
        let guard_path = dir.path().join(".clarus-data.lock");
        // Plant a stale guard with a reaped (dead) pid.
        std::fs::write(&guard_path, format!("{}:9999", dead_pid())).expect("plant stale");
        let _guard = acquire_data_dir_guard(dir.path(), 8024);
        let content = std::fs::read_to_string(&guard_path).expect("guard file present");
        let pid = content
            .trim()
            .split(':')
            .next()
            .unwrap()
            .parse::<u32>()
            .unwrap();
        assert_eq!(
            pid,
            std::process::id(),
            "stale guard taken over with our pid"
        );
    }

    /// Startup sweep removes every regular file from `_tmp/`.
    #[test]
    fn tmp_sweep_removes_orphan_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tmp = dir.path().join("_tmp");
        std::fs::create_dir_all(&tmp).expect("mkdir");
        std::fs::write(tmp.join("clarus_stow_a1b2.dcm"), b"orphan-1").expect("plant");
        std::fs::write(tmp.join("c3d4.dcm"), b"orphan-2").expect("plant");
        let (removed, bytes) = sweep_tmp_orphans(&tmp);
        assert_eq!(removed, 2, "both orphans removed");
        assert_eq!(bytes, 8 + 8, "byte accounting");
        assert_eq!(
            std::fs::read_dir(&tmp).unwrap().count(),
            0,
            "dir empty after sweep"
        );
    }

    /// Empty or missing `_tmp/` is a silent no-op.
    #[test]
    fn tmp_sweep_empty_or_missing_dir_is_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tmp = dir.path().join("_tmp");
        std::fs::create_dir_all(&tmp).expect("mkdir");
        assert_eq!(sweep_tmp_orphans(&tmp), (0, 0), "empty dir: nothing");
        let missing = dir.path().join("does-not-exist");
        assert_eq!(sweep_tmp_orphans(&missing), (0, 0), "missing dir: nothing");
    }

    /// Files outside `_tmp/` are never touched; subdirectories are kept.
    #[test]
    fn tmp_sweep_keeps_outside_files_and_subdirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tmp = dir.path().join("_tmp");
        std::fs::create_dir_all(tmp.join("sub")).expect("mkdir sub");
        std::fs::write(tmp.join("sub").join("keep.dcm"), b"keep").expect("plant sub file");
        std::fs::write(dir.path().join("outside.dcm"), b"outside").expect("plant outside");
        let (removed, _) = sweep_tmp_orphans(&tmp);
        assert_eq!(removed, 0, "nothing removed");
        assert!(
            dir.path().join("outside.dcm").exists(),
            "outside file untouched"
        );
        assert!(
            tmp.join("sub").join("keep.dcm").exists(),
            "subdir file untouched"
        );
    }

    // NOTE: no test for the per-entry warn-and-continue branch. On this
    // toolchain an undeletable regular file cannot be fabricated with std
    // only: std opens files with FILE_SHARE_DELETE and `remove_file` strips
    // the read-only attribute before deleting (both verified empirically).
    // The branch is defensive-only and covered by review.

    // -- PID file policy (stage 4, task I) ------------------------------

    /// A fresh PID file is created and holds our pid.
    #[test]
    fn pid_file_is_created_with_our_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("clarus-9999.pid");
        assert_eq!(try_write_pid_file(&pid_path, 4321), Ok(true));
        assert_eq!(
            std::fs::read_to_string(&pid_path).unwrap().trim(),
            "4321",
            "the file must contain the pid we passed"
        );
    }

    /// A stale (unparseable / dead) pid is overwritten, nginx style.
    #[test]
    fn stale_pid_file_is_overwritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("clarus-9999.pid");
        // Unparseable content -> treated as stale.
        std::fs::write(&pid_path, "not-a-pid\n").expect("plant stale");
        assert_eq!(try_write_pid_file(&pid_path, 4321), Ok(true));
        assert_eq!(std::fs::read_to_string(&pid_path).unwrap().trim(), "4321");

        // A numeric but non-existent pid is also stale. PID 0 is NOT usable
        // here: on Windows it is the System Idle Process and `tasklist`
        // reports it as alive, and on Unix `kill -0 0` probes the whole
        // process group. A very high pid cannot be running on either.
        let dead_pid = 999_999_999u32;
        std::fs::write(&pid_path, format!("{dead_pid}\n")).expect("plant dead pid");
        assert_eq!(try_write_pid_file(&pid_path, 777), Ok(true));
        assert_eq!(std::fs::read_to_string(&pid_path).unwrap().trim(), "777");
    }

    /// A pid that is provably alive must be refused.
    ///
    /// The liveness probe is `tasklist` on Windows and `kill -0` on Unix,
    /// so a synthetic pid cannot be used portably. Instead the file is
    /// seeded with a pid that IS alive: our own.
    #[test]
    fn live_pid_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("clarus-9999.pid");
        let our_pid = std::process::id();
        std::fs::write(&pid_path, format!("{our_pid}\n")).expect("plant live pid");

        assert_eq!(
            try_write_pid_file(&pid_path, our_pid + 1),
            Err(()),
            "a live foreign pid must be refused"
        );
        // The original content is untouched - the losing process must not
        // clobber the winner's file while aborting.
        assert_eq!(
            std::fs::read_to_string(&pid_path).unwrap().trim(),
            our_pid.to_string()
        );
    }

    /// An unwritable location degrades to Ok(false) with a warning - it
    /// must never panic or abort. On Windows a non-existent parent
    /// directory reproduces "cannot create" without needing chmod.
    #[test]
    fn unwritable_pid_path_warns_and_continues() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing_parent = dir.path().join("does-not-exist").join("x.pid");
        assert_eq!(
            try_write_pid_file(&missing_parent, 4321),
            Ok(false),
            "a create failure must be non-fatal"
        );
        assert!(!missing_parent.exists());
    }

    /// Resolution is automatic: no override exists, so the platform default
    /// is what gets written. This test replaces the former
    /// `explicit_pid_path_wins_over_default` - the override is gone by
    /// design (owner verdict 13.09: no config key, no CLI flag, no env var).
    #[test]
    fn pid_path_is_resolved_automatically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("clarus.conf");
        std::fs::write(&config, "").expect("touch config");

        let got = acquire_pid_file(&config, 9999);
        // The returned path is one of the two fixed candidates, and it
        // exists with our pid - never an operator-supplied location.
        let expected_default = default_pid_path(&config, 9999);
        let expected_fallback = config_dir_pid_path(&config, 9999);
        assert!(
            got == expected_default || got == expected_fallback,
            "resolved to an unexpected path: {}",
            got.display()
        );
        assert!(got.exists(), "the resolved path must have been written");
        assert_eq!(
            std::fs::read_to_string(&got).unwrap().trim(),
            std::process::id().to_string()
        );
    }

    /// The platform default name carries the port, so two instances on
    /// different ports never collide.
    #[test]
    fn default_pid_path_is_port_tagged() {
        let config = std::path::Path::new("/etc/clarus/clarus.conf");
        let p = default_pid_path(config, 11112);
        assert!(
            p.to_string_lossy().ends_with("clarus-11112.pid"),
            "unexpected default path: {}",
            p.display()
        );
        let f = config_dir_pid_path(config, 11112);
        assert!(f.to_string_lossy().ends_with("clarus-11112.pid"));
        if !cfg!(target_os = "linux") {
            // Non-Linux default is the config directory (no /var/run).
            assert_eq!(p, f);
            assert!(!p.to_string_lossy().contains("/var/run"));
        }
    }
}
