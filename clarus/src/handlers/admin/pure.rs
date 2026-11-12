// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Admin API pure synchronous core.
//!
//! Pure side of the `/admin/v1/*` endpoints (P24/P24a/P24b).
//! It owns the decision + render logic: reads from the storage
//! backend, the maintenance/log/metrics ring buffers (std RwLock), and the
//! maintenance atomics. Deletes operate directly on the B-tree manifests.
//! The pure side only validates, sets the atomic flags and builds the response
//! body that the conn loop writes; there is no framework wrapper layer.
//!
//! Fully synchronous: no `async`, no `tokio` spawn/fs in code.

#![forbid(unsafe_code)]

use std::sync::{Arc, RwLock};

use clarus_core::manifest::{AttrValue, read_manifest, write_manifest};
use clarus_core::query::tag;
use clarus_core::stow::{acquire, manifest_path};

use crate::audit_copies::{AuditCopiesEntry, AuditCopiesSnapshot};
use crate::audit_log::{AuditEvent, append_record};
use crate::handlers::json_common::{
    JsonVal, json_str_array, parse_json, parse_json_object, push_bool, push_f64, push_i64,
    push_json_str, push_opt_str, push_opt_u64, push_u64,
};
use crate::state::{AppState, LogEntry, MaintenanceState, MetricPoint};

// -- Shared response types -----------------------------------------------

/// A fully-rendered admin response: status, content type, body bytes.
#[derive(Debug, Clone)]
pub struct AdminResp {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

/// A fully-rendered admin error: status + `{"error":..., ...}` body bytes.
#[derive(Debug, Clone)]
pub struct AdminErr {
    pub status: u16,
    pub body: Vec<u8>,
}

/// An error response from pre-rendered native bytes (serde-free).
fn err_bytes(status: u16, body: Vec<u8>) -> AdminErr {
    AdminErr { status, body }
}

/// A 200 JSON response from pre-rendered native bytes (serde-free).
fn ok_bytes(body: Vec<u8>) -> AdminResp {
    AdminResp {
        status: 200,
        content_type: "application/json",
        body,
    }
}

// -- system_info: health / version / status -----------------------------

/// `GET /admin/v1/health`.
pub fn admin_health(state: &AppState) -> Result<AdminResp, AdminErr> {
    // Byte-exact with the legacy serde render: field order status/version/
    // uptime_seconds.
    let mut body = Vec::new();
    body.extend_from_slice(b"{\"status\":\"ok\",\"version\":");
    push_json_str(&mut body, env!("CARGO_PKG_VERSION"));
    body.extend_from_slice(b",\"uptime_seconds\":");
    push_u64(&mut body, state.start_time.elapsed().as_secs());
    body.push(b'}');
    Ok(ok_bytes(body))
}

/// `GET /admin/v1/version`.
pub fn admin_version(_state: &AppState) -> Result<AdminResp, AdminErr> {
    // Byte-exact with the legacy serde render: field order api_version/
    // core_version.
    let mut body = Vec::new();
    body.extend_from_slice(b"{\"api_version\":\"1\",\"core_version\":");
    push_json_str(&mut body, env!("CARGO_PKG_VERSION"));
    body.push(b'}');
    Ok(ok_bytes(body))
}

/// `GET /admin/v1/status`.
pub fn admin_status(state: &AppState) -> Result<AdminResp, AdminErr> {
    let uptime = state.start_time.elapsed().as_secs();
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let studies = count_studies_from_manifests(manifest_dir);
    let instances = count_instances_from_manifests(manifest_dir);
    let (cas_files, cas_bytes) = cas_store_stats(state);
    let cas_size = cas_bytes;
    let memory_rss_mb = get_memory_rss_mb();
    let (storage_used_mb, storage_total_mb) = disk_usage(state);
    let (ntp_synced, ntp_offset_ms, ntp_server) = ntp_status();

    // 6c: the SQLite-shape fields (db_size_mb / wal_size_mb) are gone from the
    // wire; the manifest and B-tree idx counters are the post-F health sources.
    let manifest_count = count_manifests(std::path::Path::new(&state.config.btree.manifest_dir));
    let idx_stale_objects =
        crate::metrics::CLARUS_IDX_STALE_OBJECTS.load(std::sync::atomic::Ordering::Relaxed);

    // Byte-exact with the legacy serde render (StatusResponse field order).
    let mut body = Vec::new();
    body.extend_from_slice(b"{\"status\":\"ok\",\"version\":");
    push_json_str(&mut body, env!("CARGO_PKG_VERSION"));
    body.extend_from_slice(b",\"uptime_seconds\":");
    push_u64(&mut body, uptime);
    body.extend_from_slice(b",\"memory_rss_mb\":");
    push_f64(&mut body, memory_rss_mb);
    body.extend_from_slice(b",\"storage_used_mb\":");
    push_f64(&mut body, storage_used_mb);
    body.extend_from_slice(b",\"storage_total_mb\":");
    push_f64(&mut body, storage_total_mb);
    body.extend_from_slice(b",\"ntp_synced\":");
    push_bool(&mut body, ntp_synced);
    body.extend_from_slice(b",\"ntp_offset_ms\":");
    push_f64(&mut body, ntp_offset_ms);
    body.extend_from_slice(b",\"ntp_server\":");
    push_json_str(&mut body, &ntp_server);
    body.extend_from_slice(b",\"studies\":");
    push_u64(&mut body, studies);
    body.extend_from_slice(b",\"instances\":");
    push_u64(&mut body, instances);
    body.extend_from_slice(b",\"manifest_count\":");
    push_u64(&mut body, manifest_count);
    body.extend_from_slice(b",\"idx_stale_objects\":");
    push_u64(&mut body, idx_stale_objects);
    body.extend_from_slice(b",\"cas_files\":");
    push_u64(&mut body, cas_files);
    body.extend_from_slice(b",\"cas_size_mb\":");
    push_f64(&mut body, cas_size as f64 / (1024.0 * 1024.0));
    body.extend_from_slice(b",\"cold_start\":");
    push_bool(&mut body, uptime < 300);
    body.extend_from_slice(b",\"cold_start_ms\":");
    push_opt_u64(&mut body, if uptime < 300 { Some(0) } else { None });
    body.push(b'}');
    Ok(ok_bytes(body))
}

/// Number of `*.manifest` files in `dir` (the manifest_count source: the
/// manifest directory is the post-excision record of stored studies).
fn count_manifests(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("manifest"))
                .count() as u64
        })
        .unwrap_or(0)
}

/// Total number of live (non-tombstoned) studies = count of `*.manifest` files.
/// Post-SQLite this is the study-count source.
fn count_studies_from_manifests(dir: &std::path::Path) -> u64 {
    count_manifests(dir)
}

/// Total number of instances across all live study manifests.
///
/// Walks every `*.manifest` file and sums `series[].instances.len()`, skipping
/// tombstones (`.del`) and unreadable/corrupt manifests (conservative - a
/// corrupt manifest is omitted rather than aborting a stats endpoint).
fn count_instances_from_manifests(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total: u64 = 0;
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("manifest") {
            continue;
        }
        if let Ok(m) = read_manifest(&path) {
            total += m
                .series
                .iter()
                .map(|s| s.instances.len() as u64)
                .sum::<u64>();
        }
    }
    total
}

/// CAS physical stats `(file_count, total_bytes)` from the live store walk.
fn cas_store_stats(state: &AppState) -> (u64, u64) {
    match state.cas.cas_stats() {
        Ok((c, b)) => (c, b),
        Err(_) => (0, 0),
    }
}

fn disk_usage(state: &AppState) -> (f64, f64) {
    #[cfg(unix)]
    {
        let store_path = state.cas.root_dir();
        let total_mb = crate::disk_utils::total_disk_mb(store_path);
        let free_mb = crate::disk_utils::available_disk_mb(store_path);
        if total_mb > 0.0 {
            return (total_mb - free_mb, total_mb);
        }
    }
    let cas_size = cas_store_stats(state).1;
    (cas_size as f64 / (1024.0 * 1024.0), 0.0)
}

fn ntp_status() -> (bool, f64, String) {
    let configured_server = std::env::var("CLARUS_NTP_SERVER").unwrap_or_default();
    let (synced, offset_ms, detected_server) = ntp_check_platform();
    let server = if configured_server.is_empty() {
        detected_server
    } else {
        configured_server
    };
    (synced, offset_ms, server)
}

#[cfg(unix)]
fn ntp_check_platform() -> (bool, f64, String) {
    match chronyc_tracking() {
        Some((synced, offset_ms, source_ip)) => (synced, offset_ms, source_ip),
        None => {
            crate::clog_warn!("NTP: chronyc unavailable - NTP status will show as unsynced");
            (false, 0.0, "chronyc unavailable".to_string())
        }
    }
}

#[cfg(target_os = "windows")]
fn ntp_check_platform() -> (bool, f64, String) {
    match w32tm_status() {
        Some((synced, offset_ms, source)) => (synced, offset_ms, source),
        None => {
            crate::clog_warn!("NTP: w32tm unavailable - NTP status will show as unsynced");
            (false, 0.0, "w32tm unavailable".to_string())
        }
    }
}

#[cfg(unix)]
fn chronyc_tracking() -> Option<(bool, f64, String)> {
    let out = std::process::Command::new("chronyc")
        .args(["-c", "tracking"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_chronyc_tracking(&text)
}

/// Parse one `chronyc -c tracking` CSV line.
///
/// `chronyc -c tracking` prints, in order:
/// ```text
/// 0 reference ID, 1 SOURCE ADDRESS, 2 STRATUM, 3 reference time (SECONDS),
/// 4 system time (SECONDS), 5 last offset (SECONDS), 6 RMS offset, ...
/// ```
///
/// The previous code was shifted by one from column 1 on: it read the source
/// ADDRESS as the stratum and the stratum as the reference name. The panel then
/// reported a stratum parsed from an IP address - always the `16` "unknown"
/// sentinel, because an address is not a number - and a sync verdict that was
/// really a parse failure. A wrong-but-plausible number is worse than an error.
///
/// Compiled on every platform, not just unix: the column mapping is the part
/// that was wrong, and a Windows build cannot otherwise test it. `chronyc` is
/// only ever RUN on unix.
#[cfg(any(unix, test))]
fn parse_chronyc_tracking(text: &str) -> Option<(bool, f64, String)> {
    let cols: Vec<&str> = text.trim().split(',').collect();
    // A line with fewer fields than we index is refused rather than defaulted:
    // an unexpected column count means this is not the format the parser was
    // written against, and guessing the rest of it would be a fabrication.
    if cols.len() < 6 {
        return None;
    }
    // Stratum 0 means unsynchronised; 16 is the "unknown" sentinel. A value
    // that does not parse also reports unsynchronised rather than "healthy".
    let stratum: i32 = cols[2].trim().parse().unwrap_or(16);
    let synced = stratum > 0 && stratum < 16;
    // Column 5 is the last offset in SECONDS; the panel is in milliseconds.
    let offset_secs: f64 = cols[5].trim().parse().ok()?;
    let offset_ms = offset_secs * 1000.0;
    Some((synced, offset_ms, cols[1].trim().to_string()))
}

#[cfg(target_os = "windows")]
fn w32tm_status() -> Option<(bool, f64, String)> {
    let out = std::process::Command::new("w32tm")
        .args(["/query", "/status"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut synced = false;
    let mut source = String::new();
    let mut has_offset = false;
    let mut big_offset = 0.0;
    for line in text.lines() {
        if line.contains("Source:") {
            source = line
                .split("Source:")
                .nth(1)
                .unwrap_or("")
                .trim()
                .to_string();
        }
        // w32tm prints the state as `Source`'s sibling line `Time since last
        // good time sync:` and, on a healthy clock, `Leap Indicator: 0(no
        // warning)`. The state word itself is `Source: Local CMOS Clock` /
        // `... pending`; the sync verdict is carried by the absence of
        // "not synced". Matching the substring "synced" also matched
        // "not synced", so an unsynchronised host reported healthy - the exact
        // inversion of what the check exists for.
        let lower = line.to_lowercase();
        if lower.contains("synced") && !lower.contains("not synced") {
            synced = true;
        }
        if line.contains("Offset:") {
            has_offset = true;
            // Example: Offset: -0.0212653s
            let val = line
                .split("Offset:")
                .nth(1)
                .unwrap_or("")
                .trim()
                .replace("s", "");
            big_offset = val.parse().unwrap_or(0.0);
        }
    }
    let _ = has_offset;
    Some((synced, big_offset * 1000.0, source))
}

fn get_memory_rss_mb() -> f64 {
    #[cfg(unix)]
    {
        if let Ok(contents) = std::fs::read_to_string("/proc/self/statm")
            && let Some(n) = contents.split_whitespace().nth(1)
            && let Ok(pages) = n.parse::<u64>()
        {
            return pages as f64 * 4096.0 / (1024.0 * 1024.0);
        }
    }
    0.0
}

// -- Response structs (mirror the handler structs - byte-exact serde) --

pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub uptime_seconds: u64,
}

pub struct VersionResponse {
    pub api_version: String,
    pub core_version: String,
}

pub struct StatusResponse {
    pub status: String,
    pub version: String,
    pub uptime_seconds: u64,
    pub memory_rss_mb: f64,
    pub storage_used_mb: f64,
    pub storage_total_mb: f64,
    pub ntp_synced: bool,
    pub ntp_offset_ms: f64,
    pub ntp_server: String,
    pub studies: u64,
    pub instances: u64,
    pub manifest_count: u64,
    pub idx_stale_objects: u64,
    pub cas_files: u64,
    pub cas_size_mb: f64,
    pub cold_start: bool,
    pub cold_start_ms: Option<u64>,
}

// -- dashboard ----------------------------------------------------------

pub struct DashboardStorage {
    pub disk_used_mb: f64,
    pub disk_total_mb: f64,
    pub disk_percent: f64,
    pub cas_files: u64,
    pub logical_instances: u64,
    pub s3_connected: bool,
}

pub struct DashboardAlert {
    pub severity: String,
    pub message: String,
}

pub struct DashboardResponse {
    pub status: String,
    pub version: String,
    pub uptime_seconds: u64,
    pub memory_rss_mb: f64,
    pub cold_start: bool,
    pub manifest_count: u64,
    pub idx_stale_objects: u64,
    pub storage: DashboardStorage,
    pub alerts: Vec<DashboardAlert>,
    pub maintenance: Option<MaintenanceState>,
}

/// `GET /admin/v1/dashboard`.
pub fn admin_dashboard(state: &AppState) -> Result<AdminResp, AdminErr> {
    let uptime = state.start_time.elapsed().as_secs();
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let logical = count_instances_from_manifests(manifest_dir);
    let (cas_files, cas_bytes) = cas_store_stats(state);
    let cas_size = cas_bytes;
    let cas_size_mb = cas_size as f64 / (1024.0 * 1024.0);

    let disk_total_mb = disk_total(state);
    let disk_percent = if disk_total_mb > 0.0 {
        (cas_size_mb / disk_total_mb) * 100.0
    } else {
        0.0
    };

    // 6c: db_size_mb is gone from the wire; manifest/idx counters replace it.
    let manifest_count = count_manifests(std::path::Path::new(&state.config.btree.manifest_dir));
    let idx_stale_objects =
        crate::metrics::CLARUS_IDX_STALE_OBJECTS.load(std::sync::atomic::Ordering::Relaxed);

    let mut alerts = Vec::new();
    if disk_percent > 80.0 {
        alerts.push(DashboardAlert {
            severity: "warning".to_string(),
            message: format!("\u{0414}\u{0438}\u{0441}\u{043a} \u{0437}\u{0430}\u{043f}\u{043e}\u{043b}\u{043d}\u{0435}\u{043d} \u{043d}\u{0430} {:.0}%", disk_percent),
        });
    }

    let maint = state.maintenance_state();
    let maintenance = if maint.enabled { Some(maint) } else { None };
    let memory_rss_mb = get_memory_rss_mb();

    let s3_connected = {
        #[cfg(feature = "s3")]
        {
            state.s3.get().is_some()
        }
        #[cfg(not(feature = "s3"))]
        {
            false
        }
    };

    // Byte-exact with the legacy serde render (DashboardResponse field order).
    let mut body = Vec::new();
    body.extend_from_slice(b"{\"status\":\"ok\",\"version\":");
    push_json_str(&mut body, env!("CARGO_PKG_VERSION"));
    body.extend_from_slice(b",\"uptime_seconds\":");
    push_u64(&mut body, uptime);
    body.extend_from_slice(b",\"memory_rss_mb\":");
    push_f64(&mut body, memory_rss_mb);
    body.extend_from_slice(b",\"cold_start\":");
    push_bool(&mut body, uptime < 300);
    body.extend_from_slice(b",\"manifest_count\":");
    push_u64(&mut body, manifest_count);
    body.extend_from_slice(b",\"idx_stale_objects\":");
    push_u64(&mut body, idx_stale_objects);
    body.extend_from_slice(b",\"storage\":{\"disk_used_mb\":");
    push_f64(&mut body, cas_size_mb);
    body.extend_from_slice(b",\"disk_total_mb\":");
    push_f64(&mut body, disk_total_mb);
    body.extend_from_slice(b",\"disk_percent\":");
    push_f64(&mut body, disk_percent);
    body.extend_from_slice(b",\"cas_files\":");
    push_u64(&mut body, cas_files);
    body.extend_from_slice(b",\"logical_instances\":");
    push_u64(&mut body, logical);
    body.extend_from_slice(b",\"s3_connected\":");
    push_bool(&mut body, s3_connected);
    body.extend_from_slice(b"},\"alerts\":[");
    for (i, alert) in alerts.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        body.extend_from_slice(b"{\"severity\":");
        push_json_str(&mut body, &alert.severity);
        body.extend_from_slice(b",\"message\":");
        push_json_str(&mut body, &alert.message);
        body.push(b'}');
    }
    body.extend_from_slice(b"]");
    if let Some(m) = &maintenance {
        body.extend_from_slice(b",\"maintenance\":{\"enabled\":");
        push_bool(&mut body, m.enabled);
        body.extend_from_slice(b",\"message\":");
        push_json_str(&mut body, &m.message);
        body.extend_from_slice(b",\"eta_minutes\":");
        push_opt_u64(&mut body, m.eta_minutes);
        body.extend_from_slice(b",\"enabled_at\":");
        push_opt_str(&mut body, m.enabled_at.as_deref());
        body.extend_from_slice(b",\"enabled_by\":");
        push_json_str(&mut body, &m.enabled_by);
        body.extend_from_slice(b"}");
    }
    body.push(b'}');
    Ok(ok_bytes(body))
}

#[allow(unused_variables)]
fn disk_total(state: &AppState) -> f64 {
    #[cfg(unix)]
    {
        let store_path = state.cas.root_dir();
        let total_mb = crate::disk_utils::total_disk_mb(store_path);
        if total_mb > 0.0 {
            return total_mb;
        }
    }
    0.0
}

// -- storage ------------------------------------------------------------

pub struct StorageTier {
    pub files: u64,
    pub size_mb: f64,
    pub studies: u64,
}

pub struct DiskInfo {
    pub used_mb: f64,
    pub total_mb: f64,
    pub percent: f64,
}

pub struct StorageResponse {
    /// CAS-shape fields, merged from the retired db_stats body (6d).
    pub logical_instances: u64,
    pub physical_cas_files: u64,
    pub dedup_ratio: f64,
    pub cas_size_mb: f64,
    pub manifest_count: u64,
    pub manifest_dir_mb: f64,
    pub idx_dir_mb: f64,
    pub idx_stale_objects: u64,
    pub hot: StorageTier,
    pub cold: StorageTier,
    pub disk: DiskInfo,
}

/// `GET /admin/v1/storage`.
pub fn admin_storage(state: &AppState) -> Result<AdminResp, AdminErr> {
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let logical = count_instances_from_manifests(manifest_dir);
    let (physical, cas_bytes) = cas_store_stats(state);
    let cas_size = cas_bytes;
    let cas_size_mb = cas_size as f64 / (1024.0 * 1024.0);
    let dedup_ratio = if logical > 0 {
        1.0 - (physical as f64 / logical as f64)
    } else {
        0.0
    };
    let studies = count_studies_from_manifests(manifest_dir);
    let manifest_count = count_manifests(manifest_dir);
    let manifest_dir_mb = dir_size_mb(manifest_dir);
    let idx_dir_mb = dir_size_mb(std::path::Path::new(&state.config.btree.idx_dir));
    let idx_stale_objects =
        crate::metrics::CLARUS_IDX_STALE_OBJECTS.load(std::sync::atomic::Ordering::Relaxed);

    // Byte-exact with the legacy serde render (StorageResponse field order).
    let mut body = Vec::new();
    body.extend_from_slice(b"{\"logical_instances\":");
    push_u64(&mut body, logical);
    body.extend_from_slice(b",\"physical_cas_files\":");
    push_u64(&mut body, physical);
    body.extend_from_slice(b",\"dedup_ratio\":");
    push_f64(&mut body, dedup_ratio);
    body.extend_from_slice(b",\"cas_size_mb\":");
    push_f64(&mut body, cas_size_mb);
    body.extend_from_slice(b",\"manifest_count\":");
    push_u64(&mut body, manifest_count);
    body.extend_from_slice(b",\"manifest_dir_mb\":");
    push_f64(&mut body, manifest_dir_mb);
    body.extend_from_slice(b",\"idx_dir_mb\":");
    push_f64(&mut body, idx_dir_mb);
    body.extend_from_slice(b",\"idx_stale_objects\":");
    push_u64(&mut body, idx_stale_objects);
    body.extend_from_slice(b",\"hot\":{\"files\":");
    push_u64(&mut body, physical);
    body.extend_from_slice(b",\"size_mb\":");
    push_f64(&mut body, cas_size_mb);
    body.extend_from_slice(b",\"studies\":");
    push_u64(&mut body, studies);
    body.extend_from_slice(b"},\"cold\":{\"files\":0,\"size_mb\":0.0,\"studies\":0}");
    body.extend_from_slice(b",\"disk\":{\"used_mb\":");
    push_f64(&mut body, cas_size_mb);
    body.extend_from_slice(b",\"total_mb\":0.0,\"percent\":0.0}");
    // s3 is always None here -> JSON null (Option<Value> serde render).
    body.extend_from_slice(b",\"s3\":null}");
    Ok(ok_bytes(body))
}

/// Recursive size of `dir` in MiB (manifest_dir_mb / idx_dir_mb source).
fn dir_size_mb(dir: &std::path::Path) -> f64 {
    let mut total: u64 = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(m) = std::fs::metadata(&path) {
                total += m.len();
            }
        }
    }
    total as f64 / (1024.0 * 1024.0)
}

// -- maintenance --------------------------------------------------------

pub struct MaintenanceRequest {
    pub enable: bool,
    pub message: String,
    pub eta_minutes: Option<u64>,
}

pub struct MaintenanceParams {
    pub dry_run: bool,
}

pub struct MaintenanceResponse {
    pub maintenance: bool,
    pub message: String,
    pub eta_minutes: Option<u64>,
    pub enabled_at: Option<String>,
    pub enabled_by: String,
}

/// Native parse of the maintenance request body (replaces
/// `serde_json::from_slice`; defaults match the old `.unwrap_or(default)`).
fn parse_maintenance_request(body: &[u8]) -> MaintenanceRequest {
    let mut req = MaintenanceRequest {
        enable: false,
        message: String::new(),
        eta_minutes: None,
    };
    if let Some(pairs) = parse_json_object(body) {
        for (k, v) in pairs {
            match k.as_str() {
                "enable" => req.enable = matches!(v, JsonVal::Bool(true)),
                "message" => {
                    if let JsonVal::Str(s) = v {
                        req.message = s;
                    }
                }
                "eta_minutes" => {
                    if let JsonVal::U64(n) = v {
                        req.eta_minutes = Some(n);
                    }
                }
                _ => {}
            }
        }
    }
    req
}

/// Native parse of the migration request body (replaces `serde_json::from_slice`).
fn parse_migrate_request(body: &[u8]) -> MigrateRequest {
    let mut req = MigrateRequest {
        study_uid: String::new(),
        target_tier: String::new(),
        reason: String::new(),
    };
    if let Some(pairs) = parse_json_object(body) {
        for (k, v) in pairs {
            match k.as_str() {
                "study_uid" => {
                    if let JsonVal::Str(s) = v {
                        req.study_uid = s;
                    }
                }
                "target_tier" => {
                    if let JsonVal::Str(s) = v {
                        req.target_tier = s;
                    }
                }
                "reason" => {
                    if let JsonVal::Str(s) = v {
                        req.reason = s;
                    }
                }
                _ => {}
            }
        }
    }
    req
}

/// `POST /admin/v1/maintenance`. `body` = maintenance JSON request bytes,
/// `username` is read by the wrapper from headers, `dry_run` from Query.
pub fn admin_maintenance(
    state: &AppState,
    body: &[u8],
    username: &str,
    dry_run: bool,
) -> Result<AdminResp, AdminErr> {
    let req = parse_maintenance_request(body);

    if dry_run {
        // Byte-exact with the legacy `json!` serde render: would_enable,
        // dry_run, message.
        let mut body = Vec::new();
        body.extend_from_slice(b"{\"would_enable\":");
        push_bool(&mut body, req.enable);
        body.extend_from_slice(b",\"dry_run\":true,\"message\":");
        push_json_str(
            &mut body,
            if req.enable {
                "STOW-RS would return 503 Retry-After"
            } else {
                "Maintenance mode would be disabled"
            },
        );
        body.push(b'}');
        return Ok(ok_bytes(body));
    }

    let timestamp = now_iso();

    if req.enable {
        if let Ok(mut maint) = state.maintenance.write() {
            *maint = MaintenanceState {
                enabled: true,
                message: req.message.clone(),
                eta_minutes: req.eta_minutes,
                enabled_at: Some(timestamp.clone()),
                enabled_by: username.to_string(),
            };
        }
        let state_snapshot = state.maintenance_state();
        // Byte-exact with the legacy serde render (MaintenanceResponse).
        let mut body = Vec::new();
        write_maintenance_response(
            &mut body,
            true,
            &state_snapshot.message,
            state_snapshot.eta_minutes,
            state_snapshot.enabled_at.as_deref(),
            &state_snapshot.enabled_by,
        );
        Ok(ok_bytes(body))
    } else {
        if let Ok(mut maint) = state.maintenance.write() {
            *maint = MaintenanceState::default();
        }
        // Byte-exact with the legacy serde render (MaintenanceResponse).
        let mut body = Vec::new();
        write_maintenance_response(&mut body, false, "", None, None, "");
        Ok(ok_bytes(body))
    }
}

fn now_iso() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = secs / 86400;
    let time = secs % 86400;
    let hours = time / 3600;
    let minutes = (time % 3600) / 60;
    let seconds = time % 60;
    let (year, month, day) = {
        let mut y = 1970i64;
        let mut d = days as i64;
        loop {
            let days_in_year = if (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0) {
                366
            } else {
                365
            };
            if d < days_in_year {
                break;
            }
            d -= days_in_year;
            y += 1;
        }
        let ml = if (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0) {
            [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        } else {
            [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        };
        let mut m = 0u32;
        let mut remaining = d;
        while m < 12 && remaining >= ml[m as usize] as i64 {
            remaining -= ml[m as usize] as i64;
            m += 1;
        }
        (y, m + 1, (remaining + 1) as u32)
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

// -- storage_migrate ----------------------------------------------------

pub struct MigrateRequest {
    pub study_uid: String,
    pub target_tier: String,
    pub reason: String,
}

pub struct MigrateResponse {
    pub study_uid: String,
    pub target_tier: String,
    pub available_backends: Vec<String>,
    pub message: String,
}

/// `POST /admin/v1/storage/migrate`.
pub fn admin_migrate(state: &AppState, body: &[u8]) -> Result<AdminResp, AdminErr> {
    let req = parse_migrate_request(body);

    if req.reason.trim().is_empty() {
        let mut b = Vec::new();
        write_err_body(
            &mut b,
            "Bad Request",
            "A non-empty 'reason' field is required for audit trail",
        );
        return Err(err_bytes(400, b));
    }

    let target_tier = req.target_tier.to_lowercase();
    if target_tier != "cold" && target_tier != "hot" {
        let mut b = Vec::new();
        write_err_body(&mut b, "Bad Request", "target_tier must be 'cold' or 'hot'");
        return Err(err_bytes(400, b));
    }

    let mut available_backends = vec!["hot".to_string()];
    if state.cold_cas.is_some() {
        available_backends.push("cold_path".to_string());
    }
    #[cfg(feature = "s3")]
    if state.s3.get().is_some() {
        available_backends.push("s3".to_string());
    }

    if target_tier == "cold" {
        let has_cold = state.cold_cas.is_some();
        #[cfg(feature = "s3")]
        let has_s3 = state.s3.get().is_some();
        #[cfg(not(feature = "s3"))]
        let has_s3 = false;
        if !has_cold && !has_s3 {
            // Byte-exact with the legacy `json!` serde render: error/message
            // then the available_backends array.
            let mut b = Vec::new();
            b.extend_from_slice(b"{\"error\":\"Service Unavailable\",\"message\":\"No cold storage backend configured.\",\"available_backends\":[");
            for (i, ab) in available_backends.iter().enumerate() {
                if i > 0 {
                    b.push(b',');
                }
                push_json_str(&mut b, ab);
            }
            b.extend_from_slice(b"]}");
            return Err(err_bytes(503, b));
        }
    }

    // SQLite audit write removed (file audit_log.rs is the
    // canonical evidence medium; this validation-only endpoint needs no row).

    // Byte-exact with the legacy serde render (MigrateResponse field order):
    // study_uid, target_tier, available_backends array, message.
    let mut body = Vec::new();
    body.extend_from_slice(b"{\"study_uid\":");
    push_json_str(&mut body, &req.study_uid);
    body.extend_from_slice(b",\"target_tier\":");
    push_json_str(&mut body, &target_tier);
    body.extend_from_slice(b",\"available_backends\":[");
    for (i, ab) in available_backends.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        push_json_str(&mut body, ab);
    }
    body.extend_from_slice(
        b"],\"message\":\"Migration validated. Full async migration deferred to v0.5. \
Use [storage_tiers] cold_schedule / s3_schedule for automatic migration.\"}",
    );
    Ok(ok_bytes(body))
}

// -- cas_audit ----------------------------------------------------------

/// Global CAS audit state. Stored in a static because it outlives individual
/// requests; lives in this pure module so both the start logic and the
/// wrapper's background walker share the same state.
pub static AUDIT_STATE: std::sync::LazyLock<Arc<RwLock<CasAuditState>>> =
    std::sync::LazyLock::new(|| {
        Arc::new(RwLock::new(CasAuditState {
            status: "idle".to_string(),
            files_checked: 0,
            files_total: 0,
            errors: 0,
            unreferenced: 0,
            corrupted: 0,
            unreadable: 0,
            corrupted_hashes: Vec::new(),
            started_at: None,
            eta_seconds: None,
        }))
    });

#[derive(Debug, Clone)]
pub struct CasAuditState {
    pub status: String,
    pub files_checked: u64,
    pub files_total: u64,
    /// `errors = corrupted + unreadable` (kept as their sum for wire compat).
    pub errors: u64,
    /// 2808 P1: CAS files not referenced by any live manifest (report-only,
    /// no deletion - orphans stay until the study erase reclaims them).
    pub unreferenced: u64,
    /// 0810: files whose content hash did NOT match the filename (bit rot / SEU).
    pub corrupted: u64,
    /// 0810: files that could not be read at all (I/O error, permissions).
    pub unreadable: u64,
    /// 0810: the BLAKE3 hashes (64-hex) of the CORRUPTED files, in scan order.
    /// Derived from the file name - never from re-reading a failed blob. Capped
    /// (see [`CORRUPTED_HASH_LIST_CAP`]) so a mass-corruption event cannot grow
    /// this list without bound.
    pub corrupted_hashes: Vec<String>,
    pub started_at: Option<String>,
    pub eta_seconds: Option<u64>,
}

/// Upper bound on `CasAuditState::corrupted_hashes` - the admin list is a
/// DETECTION signal, not an inventory: the counters still report the true
/// totals past the cap, so a mass-corruption event cannot exhaust memory.
pub const CORRUPTED_HASH_LIST_CAP: usize = 1024;

pub struct CasAuditParams {
    pub dry_run: bool,
}

/// `POST /admin/v1/storage/cas-audit`. Pure: status checks, dry-run form,
/// state mutation and the 202 body. The wrapper reads the returned status==202
/// to spawn the background CAS walk.
pub fn admin_cas_audit_start(state: &AppState, dry_run: bool) -> Result<AdminResp, AdminErr> {
    if dry_run {
        // The dry run never touches the state, so it needs neither the claim
        // below nor any lock ordering against it. Answered first, which also
        // keeps a dry run available while a real audit is in flight.
        {
            let audit = AUDIT_STATE.read().expect("Audit state lock poisoned");
            if audit.status == "running" {
                // Already running: 409 CONFLICT with the current state.
                let mut body = Vec::new();
                write_cas_audit_state(&mut body, &audit);
                return Ok(AdminResp {
                    status: 409,
                    content_type: "application/json",
                    body,
                });
            }
        }
        // Byte-exact with the legacy `json!` serde render.
        let cas_files = cas_store_stats(state).0;
        let mut body = Vec::new();
        body.extend_from_slice(b"{\"would_check\":");
        push_u64(&mut body, cas_files);
        body.extend_from_slice(
            b",\"dry_run\":true,\"message\":\"CAS audit would verify all files in store/\"}",
        );
        return Ok(ok_bytes(body));
    }

    let total = cas_store_stats(state).0;

    // Claim the run and set it in ONE write section. The status was read
    // under a read lock and the state rewritten under a write lock, so two
    // concurrent requests could both find "idle" and both spawn a walk -
    // two full CAS sweeps over the same store, each reporting the other's
    // progress. Checking and claiming together is the only way the "only one
    // audit at a time" contract holds.
    let audit_clone = {
        let mut audit = AUDIT_STATE.write().expect("Audit state lock poisoned");
        if audit.status == "running" {
            // Lost the race: answer 409 with the state that won.
            let mut body = Vec::new();
            write_cas_audit_state(&mut body, &audit);
            return Ok(AdminResp {
                status: 409,
                content_type: "application/json",
                body,
            });
        }
        *audit = CasAuditState {
            status: "running".to_string(),
            files_checked: 0,
            files_total: total,
            errors: 0,
            unreferenced: 0,
            corrupted: 0,
            unreadable: 0,
            corrupted_hashes: Vec::new(),
            started_at: Some(chrono_now()),
            eta_seconds: None,
        };
        audit.clone()
    };

    Ok(AdminResp {
        status: 202,
        content_type: "application/json",
        // Byte-exact with the legacy serde render (CasAuditState).
        body: {
            let mut body = Vec::new();
            write_cas_audit_state(&mut body, &audit_clone);
            body
        },
    })
}

/// `GET /admin/v1/storage/cas-audit/status`.
pub fn admin_cas_audit_status(_state: &AppState) -> Result<AdminResp, AdminErr> {
    let audit = AUDIT_STATE.read().expect("Audit state lock poisoned");
    // Byte-exact with the legacy serde render (CasAuditState).
    let mut body = Vec::new();
    write_cas_audit_state(&mut body, &audit);
    Ok(ok_bytes(body))
}

// -- audit-copies (3008) ------------------------------------------------

/// Global sidecar snapshot for `GET /admin/v1/storage/audit-copies`. Built by
/// the `audit-copies` background scanner (`crate::audit_copies::refresh`),
/// read by the GET handler; never touches CAS.
pub static AUDIT_COPIES_STATE: std::sync::LazyLock<Arc<RwLock<AuditCopiesSnapshot>>> =
    std::sync::LazyLock::new(|| Arc::new(RwLock::new(AuditCopiesSnapshot::default())));

/// Render one `{"hash":"<hex>","tier":N}` entry.
fn push_audit_copies_entry(out: &mut Vec<u8>, e: &AuditCopiesEntry) {
    out.extend_from_slice(b"{\"hash\":");
    push_json_str(out, &e.hash);
    out.extend_from_slice(b",\"tier\":");
    push_u64(out, u64::from(e.tier));
    out.push(b'}');
}

/// `GET /admin/v1/storage/audit-copies` (3008). Serves the prebuilt snapshot
/// with a fixed field order; `pending` appears ONLY before the first ever
/// successful scan (an empty manifests/ dir after a scan is `ready` with
/// zeros). `generated_at` is the snapshot build time, never request time.
pub fn admin_audit_copies(_state: &AppState) -> Result<AdminResp, AdminErr> {
    let snap = AUDIT_COPIES_STATE
        .read()
        .expect("audit-copies state lock poisoned");
    let mut body = Vec::new();
    match &snap.generated_at {
        Some(ts) => {
            body.extend_from_slice(b"{\"status\":\"ready\",\"generated_at\":");
            push_json_str(&mut body, ts);
            body.extend_from_slice(b",\"total_studies\":");
            push_u64(&mut body, snap.total_studies);
            body.extend_from_slice(b",\"total_tracked\":");
            push_u64(&mut body, snap.total_tracked);
            body.extend_from_slice(b",\"total_orphans\":");
            push_u64(&mut body, snap.total_orphans);
            body.extend_from_slice(b",\"studies\":[");
            for (i, s) in snap.studies.iter().enumerate() {
                if i > 0 {
                    body.push(b',');
                }
                body.extend_from_slice(b"{\"study_uid\":");
                push_json_str(&mut body, &s.study_uid);
                body.extend_from_slice(b",\"tracked\":[");
                for (k, e) in s.tracked.iter().enumerate() {
                    if k > 0 {
                        body.push(b',');
                    }
                    push_audit_copies_entry(&mut body, e);
                }
                body.extend_from_slice(b"],\"orphans\":[");
                for (k, e) in s.orphans.iter().enumerate() {
                    if k > 0 {
                        body.push(b',');
                    }
                    push_audit_copies_entry(&mut body, e);
                }
                body.extend_from_slice(b"]}");
            }
            body.extend_from_slice(b"]}");
        }
        None => body.extend_from_slice(
            b"{\"status\":\"pending\",\"generated_at\":null,\"total_studies\":0,\"total_tracked\":0,\"total_orphans\":0,\"studies\":[]}",
        ),
    }
    Ok(ok_bytes(body))
}

// -- recovery/rebuild -------------------------------------------

/// Global rebuild state (cas->manifests rebuild). Mirrors
/// [`AUDIT_STATE`]: a process-global that outlives individual requests, shared
/// by the start logic (here) and the dispatch-side background walker
/// (`crate::rebuild_worker::run_rebuild`).
pub static REBUILD_STATE: std::sync::LazyLock<Arc<RwLock<RebuildState>>> =
    std::sync::LazyLock::new(|| {
        Arc::new(RwLock::new(RebuildState {
            status: "idle".to_string(),
            files_scanned: 0,
            manifests_written: 0,
            instances_indexed: 0,
            skipped: 0,
            errors: 0,
            started_at: None,
            finished_at: None,
            sources: Vec::new(),
            dry_run: false,
        }))
    });

#[derive(Debug, Clone)]
pub struct RebuildState {
    /// `idle` | `running` | `done` | `error: <reason>`.
    pub status: String,
    /// DICOM blobs read from the requested sources.
    pub files_scanned: u64,
    /// Study manifests written (or that would be written in a dry run).
    pub manifests_written: u64,
    /// Instances folded into manifests (blake3, size, tier, transfer syntax).
    pub instances_indexed: u64,
    /// Blobs skipped: duplicates, malformed headers, missing required UIDs,
    /// standalone audit-copy fragments.
    pub skipped: u64,
    /// Hard errors (unreadable source root, source misconfiguration).
    pub errors: u64,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    /// Explicit source list in priority order (S3 > cold > hot).
    pub sources: Vec<String>,
    pub dry_run: bool,
}

/// The one recognised source kind -> tier mapping.
const SOURCE_TIERS: [(&str, u8); 3] = [
    ("s3", clarus_core::manifest::TIER_S3),
    ("cold", clarus_core::manifest::TIER_COLD),
    ("hot", clarus_core::manifest::TIER_HOT),
];

/// Claim a rebuild, or report that one is already running.
///
/// Callers must hold the WRITE lock: the whole point is that the "is it
/// running" test and the "make it running" assignment are one indivisible
/// step. Splitting them across a read lock and a write lock let two concurrent
/// requests both see `idle` and both start a walker.
///
/// Returns `true` when this caller claimed it. The running state, if any, is
/// read from `st` by the caller under the same lock.
fn claim_rebuild(st: &mut RebuildState, fresh: RebuildState) -> bool {
    if st.status == "running" {
        return false;
    }
    *st = fresh;
    true
}

/// `POST /admin/v1/recovery/rebuild`. Body (JSON):
/// `{"sources": ["s3","cold","hot"], "dry_run": false}`.
///
/// `sources` is an **explicit** list - never auto-detected, because a
/// misconfigured `cold_path` must error loudly rather than silently lose
/// (write nothing / serve nothing) the cold tier's data. The list is
/// re-ordered to the migration-priority S3 > cold > hot, so a duplicate blob
/// (same SOPInstanceUID in two sources) keeps the higher-priority copy.
/// Manifests are ALWAYS written to `[btree] manifest_dir`; blobs are never
/// moved or deleted. Returns 202 when a walker was started, 409 when a walker
/// is already running.
#[allow(clippy::collapsible_match)] // cfg-gated s3 arm should not collapse into the match
pub fn admin_recovery_rebuild(
    state: &AppState,
    body: &[u8],
    dry_run: bool,
) -> Result<AdminResp, AdminErr> {
    // The "is a rebuild running" check and the "claim it" write happen in ONE
    // write-lock section, below, after the request has been validated. Doing
    // the check under a read lock and the claim under a separate write lock let
    // two concurrent requests both observe "not running" and both start a
    // walker - two threads writing the same manifests, which is exactly what
    // the single-walker contract forbids.

    // Parse the explicit `sources` array from the JSON body.
    let mut requested: Vec<String> = Vec::new();
    if let Some(pairs) = parse_json_object(body) {
        for (k, v) in pairs {
            if k == "sources"
                && let Some(arr) = json_str_array(&v)
            {
                requested = arr;
            }
        }
    }
    if requested.is_empty() {
        let mut b = Vec::new();
        write_err_body(
            &mut b,
            "Bad Request",
            "sources must be a non-empty explicit array, e.g. [\"s3\",\"cold\",\"hot\"]",
        );
        return Err(err_bytes(400, b));
    }
    // Reject unknown source names.
    let mut unknown: Vec<String> = Vec::new();
    for x in &requested {
        if !SOURCE_TIERS.iter().any(|(name, _)| name == x) {
            unknown.push(x.clone());
        }
    }
    if !unknown.is_empty() {
        let mut b = Vec::new();
        write_err_body(
            &mut b,
            "Bad Request",
            &format!(
                "unknown source(s): {} — valid: {}",
                unknown.join(","),
                ["hot", "cold", "s3"].join(",")
            ),
        );
        return Err(err_bytes(400, b));
    }
    // Validate each configured source so misconfiguration errors, never
    // silently loses a tier.
    for (name, _) in SOURCE_TIERS {
        if !requested.iter().any(|x| x == name) {
            continue;
        }
        match name {
            "cold" if state.cold_cas.is_none() => {
                let mut b = Vec::new();
                write_err_body(
                    &mut b,
                    "Bad Request",
                    "cold requested but no cold tier is configured (state.cold_cas is None)",
                );
                return Err(err_bytes(400, b));
            }
            "s3" => {
                #[cfg(feature = "s3")]
                if state.s3.get().is_none() {
                    let mut b = Vec::new();
                    write_err_body(
                        &mut b,
                        "Bad Request",
                        "s3 requested but the S3 backend is not connected",
                    );
                    return Err(err_bytes(400, b));
                }
                #[cfg(not(feature = "s3"))]
                {
                    let mut b = Vec::new();
                    write_err_body(
                        &mut b,
                        "Bad Request",
                        "s3 requested but this build has no s3 feature",
                    );
                    return Err(err_bytes(400, b));
                }
            }
            _ => {}
        }
    }

    // Normalize to priority order (S3 > cold > hot).
    let mut sources: Vec<String> = Vec::new();
    for (name, _) in SOURCE_TIERS {
        if requested.iter().any(|x| x == name) {
            sources.push(name.to_string());
        }
    }

    // Claim the rebuild and report whether we got it, in one section.
    let started = {
        let mut st = REBUILD_STATE.write().expect("Rebuild state lock poisoned");
        let fresh = RebuildState {
            status: "running".to_string(),
            files_scanned: 0,
            manifests_written: 0,
            instances_indexed: 0,
            skipped: 0,
            errors: 0,
            started_at: Some(chrono_now()),
            finished_at: None,
            sources: sources.clone(),
            dry_run,
        };
        if claim_rebuild(&mut st, fresh) {
            st.clone()
        } else {
            // Another request claimed it. The state is read under the same
            // lock the claim would take, so what we report is what blocked us.
            let running = st.clone();
            drop(st);
            let mut b = Vec::new();
            write_rebuild_state(&mut b, &running);
            return Ok(AdminResp {
                status: 409,
                content_type: "application/json",
                body: b,
            });
        }
    };

    let mut body = Vec::new();
    write_rebuild_state(&mut body, &started);
    Ok(AdminResp {
        status: 202,
        content_type: "application/json",
        body,
    })
}

/// `GET /admin/v1/recovery/rebuild/status`: the live rebuild state.
pub fn admin_recovery_rebuild_status(_state: &AppState) -> Result<AdminResp, AdminErr> {
    let st = REBUILD_STATE.read().expect("Rebuild state lock poisoned");
    let mut body = Vec::new();
    write_rebuild_state(&mut body, &st);
    Ok(ok_bytes(body))
}

pub fn chrono_now() -> String {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

// -- audit_log ----------------------------------------------------------

pub struct AuditLogParams {
    pub from: Option<String>,
    pub to: Option<String>,
    pub user: Option<String>,
    pub action: Option<String>,
    pub limit: u64,
    pub offset: u64,
}

pub struct AuditLogEntryResponse {
    pub id: i64,
    pub timestamp: String,
    pub user: String,
    pub action: String,
    pub target: String,
    /// PatientID of the owning study ("" for old rows / erase rows).
    pub patient_id: String,
    pub details: Option<String>,
}

pub struct AuditLogResponse {
    pub total: u64,
    pub entries: Vec<AuditLogEntryResponse>,
}

/// `GET /admin/v1/audit-log`.
pub fn admin_audit_log(state: &AppState, params: &AuditLogParams) -> Result<AdminResp, AdminErr> {
    // Post-SQLite: audit evidence lives as JSON-lines in
    // `audit/YYYYMMDD.log`. The file-scan replaces the former SqliteBackend
    // query_audit_log - no SQLite dependency on the admin audit-log path.
    let (total, entries) = audit_log_from_files(
        std::path::Path::new(&state.config.btree.audit_dir),
        params.from.as_deref(),
        params.to.as_deref(),
        params.user.as_deref(),
        params.action.as_deref(),
        params.limit,
        params.offset,
    );
    // Byte-exact with the legacy serde render: total, then entries array;
    // details is omitted (skip_serializing_if) when None.
    let mut body = Vec::new();
    body.extend_from_slice(b"{\"total\":");
    push_u64(&mut body, total);
    body.extend_from_slice(b",\"entries\":[");
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        body.extend_from_slice(b"{\"id\":");
        push_i64(&mut body, e.id);
        body.extend_from_slice(b",\"timestamp\":");
        push_json_str(&mut body, &e.timestamp);
        body.extend_from_slice(b",\"user\":");
        push_json_str(&mut body, &e.user);
        body.extend_from_slice(b",\"action\":");
        push_json_str(&mut body, &e.action);
        body.extend_from_slice(b",\"target\":");
        push_json_str(&mut body, &e.target);
        body.extend_from_slice(b",\"patient_id\":");
        push_json_str(&mut body, &e.patient_id);
        if let Some(d) = &e.details {
            body.extend_from_slice(b",\"details\":");
            push_json_str(&mut body, d);
        }
        body.push(b'}');
    }
    body.extend_from_slice(b"]}");
    Ok(ok_bytes(body))
}

// -- access-log (ATNA-lite journal reader, 2808) -------------------------

pub struct AccessLogEntryResponse {
    pub id: i64,
    pub timestamp: String,
    pub user: String,
    pub action: String,
    pub atna: String,
    pub status: u64,
    pub study: String,
    pub series: String,
    pub sop: String,
    pub obj_type: String,
    pub q: String,
    pub count: u64,
}

pub struct AccessLogResponse {
    pub total: u64,
    pub entries: Vec<AccessLogEntryResponse>,
}

/// `GET /admin/v1/access-log` - the access-audit journal reader. Same native
/// JSON-line scan + filter shape as the delete-trail reader; the read itself
/// is self-audited by the dispatch hook (110101, GOST 27789 9.4).
pub fn admin_access_log(state: &AppState, params: &AuditLogParams) -> Result<AdminResp, AdminErr> {
    let (total, entries) = access_log_from_files(
        std::path::Path::new(&state.config.btree.access_dir),
        params.from.as_deref(),
        params.to.as_deref(),
        params.user.as_deref(),
        params.action.as_deref(),
        params.limit,
        params.offset,
    );
    let mut body = Vec::new();
    body.extend_from_slice(b"{\"total\":");
    push_u64(&mut body, total);
    body.extend_from_slice(b",\"entries\":[");
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        body.extend_from_slice(b"{\"id\":");
        push_i64(&mut body, e.id);
        body.extend_from_slice(b",\"timestamp\":");
        push_json_str(&mut body, &e.timestamp);
        body.extend_from_slice(b",\"user\":");
        push_json_str(&mut body, &e.user);
        body.extend_from_slice(b",\"action\":");
        push_json_str(&mut body, &e.action);
        body.extend_from_slice(b",\"atna\":");
        push_json_str(&mut body, &e.atna);
        body.extend_from_slice(b",\"status\":");
        push_u64(&mut body, e.status);
        body.extend_from_slice(b",\"study\":");
        push_json_str(&mut body, &e.study);
        body.extend_from_slice(b",\"series\":");
        push_json_str(&mut body, &e.series);
        body.extend_from_slice(b",\"sop\":");
        push_json_str(&mut body, &e.sop);
        body.extend_from_slice(b",\"obj_type\":");
        push_json_str(&mut body, &e.obj_type);
        body.extend_from_slice(b",\"q\":");
        push_json_str(&mut body, &e.q);
        body.extend_from_slice(b",\"count\":");
        push_u64(&mut body, e.count);
        body.push(b'}');
    }
    body.extend_from_slice(b"]}");
    Ok(ok_bytes(body))
}

/// How many rows the journal page readers pre-allocate before growing.
///
/// The request's `limit` is client-supplied and unbounded, so it may not
/// size an allocation: `?limit=18446744073709551615` is a capacity-overflow
/// abort and `?limit=1e9` reserves gigabytes for a page that only ever fills
/// to the cap. A page of audit rows is small in practice, so the pre-alloc
/// is generous for the real case and fixed for the hostile one - the vector
/// grows by itself when a legitimate request asks for more.
const PAGE_PREALLOC_CAP: u64 = 1024;

/// The largest page a single request may ask for.
///
/// Well below the pre-alloc cap's reach and far above any UI page size, so a
/// client paging through the journal is unaffected while a request naming a
/// billion rows is clamped rather than honoured.
const MAX_PAGE_LIMIT: u64 = 10_000;

/// Scan `access/YYYYMMDD.log` JSON-lines into access entries. Filters mirror
/// the delete-trail reader: `from`/`to` on the line's `ts` (ISO-8601, so
/// lexicographic order works), equality on `user` and `action`; newest-first,
/// then limit/offset. A truncated tail line (crash during a batch flush) is
/// skipped, never an error.
fn access_log_from_files(
    access_dir: &std::path::Path,
    from: Option<&str>,
    to: Option<&str>,
    user: Option<&str>,
    action: Option<&str>,
    limit: u64,
    offset: u64,
) -> (u64, Vec<AccessLogEntryResponse>) {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(access_dir)
        .map(|it| it.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    // Identity is the 1-based line ordinal across all files in name order,
    // and newest-first is a DESC on that ordinal. Assigning it needs the total
    // line count, which a streaming pass gets from a cheap line count per file
    // (no JSON parse, no entry construction): the journal is never held in
    // RAM, and a page reads only the files it must.
    let mut line_total = 0u64;
    for path in &files {
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(path) {
            line_total += text.lines().filter(|l| !l.trim().is_empty()).count() as u64;
        }
    }

    // Walk newest-file-first, numbering each line as the original pass did.
    let mut id = line_total;
    let mut total = 0u64;
    let mut skipped = 0u64;
    // The page never holds more rows than the caller can ask for, so the
    // pre-allocation is bounded by the CAP rather than by the raw request.
    // `?limit=18446744073709551615` used to reach `with_capacity` and abort
    // the process on a capacity overflow; a large-but-parseable limit
    // reserved gigabytes for a page that can never fill. Growing as rows
    // arrive costs a few reallocations and makes the worst case the cap.
    let mut page: Vec<AccessLogEntryResponse> =
        Vec::with_capacity(limit.min(PAGE_PREALLOC_CAP) as usize);
    let page_limit = limit.min(MAX_PAGE_LIMIT) as usize;
    for path in files.iter().rev() {
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let mut kept: std::collections::VecDeque<AccessLogEntryResponse> =
            std::collections::VecDeque::new();
        // The ring holds at most what this file can still contribute: the
        // rows the offset window must skip plus the free page slots.
        let window = (offset.saturating_sub(skipped))
            .saturating_add((page_limit - page.len()) as u64) as usize;
        // Once the page is full no row of this file can reach it - the page is
        // filled from newer files first - but `total` must still count every
        // match, so the lines are parsed and filtered without building rows.
        // Keeping them would mean a full `Vec` of entries for an arbitrarily
        // large old journal, allocated only to be discarded.
        let page_full = page.len() >= page_limit;
        let mut matches = 0u64;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let ordinal = id;
            id = id.saturating_sub(1);
            let v = match parse_json(line.as_bytes()) {
                Some(v) => v,
                None => continue, // crash tail: skip the last partial line
            };
            if v.obj_get("event").and_then(|x| x.as_str()) != Some("access") {
                continue;
            }
            if page_full {
                // Count-only: the filters still decide whether this line is a
                // match, but the entry is never retained.
                let entry = access_entry_from_json(v, ordinal);
                if from.is_none_or(|f| entry.timestamp.as_str() >= f)
                    && to.is_none_or(|t| entry.timestamp.as_str() <= t)
                    && user.is_none_or(|u| entry.user == u)
                    && action.is_none_or(|a| entry.action == a)
                {
                    matches += 1;
                }
                continue;
            }
            let entry = access_entry_from_json(v, ordinal);
            if from.is_none_or(|f| entry.timestamp.as_str() >= f)
                && to.is_none_or(|t| entry.timestamp.as_str() <= t)
                && user.is_none_or(|u| entry.user == u)
                && action.is_none_or(|a| entry.action == a)
            {
                // Only the rows this file can still contribute are retained.
                // The page is filled from the END of the file backwards (`rev`
                // below), so the retained set is the LATEST `window` matches:
                // a ring, not the first matches seen. Retaining every match
                // buffered a whole journal file - a busy day is millions of
                // lines - to hand a page that holds at most `page_limit`.
                // Rows that fall out of the ring are still COUNTED, which is
                // what `total` needs; they are simply not kept.
                if window == 0 {
                    matches += 1;
                } else {
                    if kept.len() == window {
                        kept.pop_front();
                        matches += 1;
                    }
                    kept.push_back(entry);
                }
            }
        }
        total += kept.len() as u64 + matches;
        // This file's rows are already newest-first after the reverse; the
        // offset counts rows in that order, so it is consumed here. Older
        // files are still scanned so `total` covers every match: breaking
        // out on a full page undercounted the total (PS3.18 8.3.4.4.1).
        for entry in kept.into_iter().rev() {
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if page.len() >= page_limit {
                continue;
            }
            page.push(entry);
        }
    }
    (total, page)
}

/// Build one access row from a parsed journal line.
fn access_entry_from_json(v: JsonVal, id: u64) -> AccessLogEntryResponse {
    AccessLogEntryResponse {
        id: id as i64,
        timestamp: v
            .obj_get("ts")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        user: v
            .obj_get("user")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        action: v
            .obj_get("action")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        atna: v
            .obj_get("atna")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        status: v.obj_get("status").and_then(|x| x.as_u64()).unwrap_or(0),
        study: v
            .obj_get("study")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        series: v
            .obj_get("series")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        sop: v
            .obj_get("sop")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        obj_type: v
            .obj_get("obj_type")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        q: v.obj_get("q")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        count: v.obj_get("count").and_then(|x| x.as_u64()).unwrap_or(1),
    }
}

/// Scan `audit/YYYYMMDD.log` JSON-lines into audit entries (post-SQLite source
/// of truth for `/admin/v1/audit-log`).
///
/// Each line persists an [`crate::audit_log::AuditEvent`] as
/// `{"event":...,"user":...,"reason":...,"target":...,...}` (no timestamp / id
/// is stored - rotation is by `YYYYMMDD.log` filename). We derive a monotonic
/// `id` across files sorted by date then line order, and derive `timestamp`
/// from the file's `YYYYMMDD` (day granularity, matching the rotation).
/// Filtering mirrors the former SqliteBackend query: `from`/`to` on timestamp,
/// equality on `user` (the `user` field) and `action` (the `event` field);
/// `details` maps to `reason`. Then `ORDER BY id DESC` (newest first), then
/// limit/offset. Returns `(total_filtered, paginated_entries)`.
/// Raw parsed audit line: (id, day, user, action, target, patient_id,
/// details). `details` maps to the row's `reason`.
type RawAuditLine = (i64, String, String, String, String, String, Option<String>);

fn audit_log_from_files(
    audit_dir: &std::path::Path,
    from: Option<&str>,
    to: Option<&str>,
    user: Option<&str>,
    action: Option<&str>,
    limit: u64,
    offset: u64,
) -> (u64, Vec<AuditLogEntryResponse>) {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(audit_dir)
        .map(|it| it.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    // Same streaming shape as the access journal: the id is the line ordinal
    // across files in name order, newest-first is a DESC on it, and the page
    // is filled while walking backwards. A busy deployment holds GB of
    // journal; paging must not depend on holding it.
    let mut line_total = 0u64;
    for path in &files {
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(path) {
            line_total += text.lines().filter(|l| !l.trim().is_empty()).count() as u64;
        }
    }

    let mut id = line_total;
    let mut total = 0u64;
    let mut skipped = 0u64;
    let mut page: Vec<RawAuditLine> = Vec::with_capacity(limit.min(PAGE_PREALLOC_CAP) as usize);
    let page_limit = limit.min(MAX_PAGE_LIMIT) as usize;
    for path in files.iter().rev() {
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        let day = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let mut kept: Vec<RawAuditLine> = Vec::new();
        // Same count-only rule as `access_log_from_files`: a full page cannot
        // accept another row, but `total` must still count every match.
        let page_full = page.len() >= page_limit;
        let mut matches = 0u64;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let ordinal = id;
            id = id.saturating_sub(1);
            // native JSON parse (serde-free) - same top-level field
            // extraction (user/event/target/reason) as the serde-era path.
            let v = match parse_json(line.as_bytes()) {
                Some(v) => v,
                None => continue,
            };
            let e_user = v
                .obj_get("user")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let e_action = v
                .obj_get("event")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let e_target = v
                .obj_get("target")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let e_details = v
                .obj_get("reason")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            // 2808: delete rows carry patient_id (default "" for old rows -
            // unknown fields are ignored, missing fields default empty).
            let e_patient = v
                .obj_get("patient_id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            // Every row carries its own second-precision `ts`
            // (YYYY-MM-DDTHH:MM:SSZ). The filename only says which DAY the row
            // was written to, so serving or filtering on it made the whole
            // journal day-granular: a `from`/`to` bound like "20260215T101500"
            // compares against "20260215", and since '-' sorts below any digit
            // the bound silently dropped rows it should have kept. Rows written
            // before `ts` existed fall back to the day stem.
            let e_ts = v
                .obj_get("ts")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| day.clone());
            if from.is_none_or(|f| e_ts.as_str() >= f)
                && to.is_none_or(|t| e_ts.as_str() <= t)
                && user.is_none_or(|u2| e_user.as_str() == u2)
                && action.is_none_or(|a2| e_action.as_str() == a2)
            {
                if page_full {
                    // Count-only: a match is a match whether or not it can
                    // reach the page, and this file's rows never can.
                    matches += 1;
                } else {
                    kept.push((
                        ordinal as i64,
                        e_ts,
                        e_user,
                        e_action,
                        e_target,
                        e_patient,
                        e_details,
                    ));
                }
            }
        }
        total += kept.len() as u64 + matches;
        // Older files are still scanned so `total` covers every match: a
        // full page must not stop the count (PS3.18 8.3.4.4.1).
        for row in kept.into_iter().rev() {
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if page.len() >= page_limit {
                continue;
            }
            page.push(row);
        }
    }
    // `page` is already newest-first (ORDER BY id DESC).
    let entries = page
        .into_iter()
        .map(|(id, ts, u, a, t, p, d)| AuditLogEntryResponse {
            id,
            timestamp: ts,
            user: u,
            action: a,
            target: t,
            patient_id: p,
            details: d,
        })
        .collect();
    (total, entries)
}

// -- instances_search ---------------------------------------------------

pub struct InstanceSearchParams {
    pub patient_name: Option<String>,
    pub patient_id: Option<String>,
    pub study_uid: Option<String>,
    pub series_uid: Option<String>,
    pub modality: Option<String>,
    pub accession_number: Option<String>,
    pub study_description: Option<String>,
    pub study_date: Option<String>,
    /// Search by SOP Instance UID (0008,0018) via the manifest scan (6b).
    pub sop_instance_uid: Option<String>,
    pub limit: u64,
    pub offset: u64,
}

pub struct MaskedInstanceSummary {
    pub sop_instance_uid: String,
    pub patient_name: String,
    pub study_uid: String,
    pub series_uid: String,
    pub modality: String,
    pub study_date: Option<String>,
    pub study_description: Option<String>,
    pub size_bytes: i64,
}

/// `GET /admin/v1/instances/search`. `is_auditor` read by the wrapper from the
/// X-Clarus-Role header.
pub fn admin_instances_search(
    state: &AppState,
    params: &InstanceSearchParams,
    is_auditor: bool,
) -> Result<AdminResp, AdminErr> {
    let limit = params.limit.min(500);
    let offset = params.offset;

    // 6b: SOP-UID lookups go through the manifest scan, never the SQLite
    // backend. Manifests are the post-excision source of truth for instances.
    if let Some(uid) = params.sop_instance_uid.as_deref() {
        return manifest_instance_search(
            std::path::Path::new(&state.config.btree.manifest_dir),
            uid,
            limit,
            offset,
            is_auditor,
        );
    }

    // Post-SQLite: general instance search walks the B-tree
    // manifests - the post-excision source of truth - no SqliteBackend.
    let masked = manifest_instances_search_general(
        std::path::Path::new(&state.config.btree.manifest_dir),
        params.patient_name.as_deref(),
        params.patient_id.as_deref(),
        params.study_uid.as_deref(),
        params.series_uid.as_deref(),
        params.modality.as_deref(),
        params.accession_number.as_deref(),
        params.study_description.as_deref(),
        params.study_date.as_deref(),
        limit,
        offset,
        is_auditor,
    );
    // Byte-exact with the legacy serde render (array of MaskedInstanceSummary).
    let mut body = Vec::new();
    write_masked_summaries(&mut body, &masked);
    Ok(ok_bytes(body))
}

fn mask_name(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = name.chars().collect();
    // A two-character name has no interior to hide, so "keep the first and
    // last, star the middle" produced the name itself with no masking at all.
    // Both initial and initial are enough to identify a patient in a small
    // worklist, which defeats the masking this function exists for.
    if chars.len() <= 2 {
        return "*".repeat(chars.len());
    }
    let mut out = String::with_capacity(name.len());
    out.push(chars[0]);
    for _ in 1..(chars.len() - 1) {
        out.push('*');
    }
    out.push(chars[chars.len() - 1]);
    out
}

/// Append a JSON array of `MaskedInstanceSummary` (byte-exact with the legacy
/// serde render: study_date/study_description always present, null when None).
fn write_masked_summaries(out: &mut Vec<u8>, items: &[MaskedInstanceSummary]) {
    out.push(b'[');
    for (i, m) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(b"{\"sop_instance_uid\":");
        push_json_str(out, &m.sop_instance_uid);
        out.extend_from_slice(b",\"patient_name\":");
        push_json_str(out, &m.patient_name);
        out.extend_from_slice(b",\"study_uid\":");
        push_json_str(out, &m.study_uid);
        out.extend_from_slice(b",\"series_uid\":");
        push_json_str(out, &m.series_uid);
        out.extend_from_slice(b",\"modality\":");
        push_json_str(out, &m.modality);
        out.extend_from_slice(b",\"study_date\":");
        push_opt_str(out, m.study_date.as_deref());
        out.extend_from_slice(b",\"study_description\":");
        push_opt_str(out, m.study_description.as_deref());
        out.extend_from_slice(b",\"size_bytes\":");
        push_i64(out, m.size_bytes);
        out.push(b'}');
    }
    out.push(b']');
}

/// Append `S3ValidationResponse` bytes (byte-exact with the legacy serde
/// render): reachable first, then only the present Option fields (all Option
/// fields skip_serializing_if none), then reason (always present).
fn write_s3_validation(
    out: &mut Vec<u8>,
    reachable: bool,
    latency_ms: Option<u64>,
    bucket: Option<&str>,
    endpoint: Option<&str>,
    reason: &str,
) {
    out.extend_from_slice(b"{\"reachable\":");
    push_bool(out, reachable);
    if let Some(lat) = latency_ms {
        out.extend_from_slice(b",\"latency_ms\":");
        push_u64(out, lat);
    }
    if let Some(b) = bucket {
        out.extend_from_slice(b",\"bucket\":");
        push_json_str(out, b);
    }
    if let Some(e) = endpoint {
        out.extend_from_slice(b",\"endpoint\":");
        push_json_str(out, e);
    }
    out.extend_from_slice(b",\"reason\":");
    push_json_str(out, reason);
    out.push(b'}');
}

/// Append a standard `{"error":<str>,"message":<str>}` body (byte-exact with
/// the legacy `json!({"error":... ,"message":...})` serde render).
fn write_err_body(out: &mut Vec<u8>, error: &str, message: &str) {
    out.extend_from_slice(b"{\"error\":");
    push_json_str(out, error);
    out.extend_from_slice(b",\"message\":");
    push_json_str(out, message);
    out.push(b'}');
}

/// Append a `MaintenanceResponse` (byte-exact with the legacy serde render:
/// maintenance/message/eta_minutes(enabled_at optional-as-null)/enabled_by).
fn write_maintenance_response(
    out: &mut Vec<u8>,
    maintenance: bool,
    message: &str,
    eta_minutes: Option<u64>,
    enabled_at: Option<&str>,
    enabled_by: &str,
) {
    out.extend_from_slice(b"{\"maintenance\":");
    push_bool(out, maintenance);
    out.extend_from_slice(b",\"message\":");
    push_json_str(out, message);
    out.extend_from_slice(b",\"eta_minutes\":");
    push_opt_u64(out, eta_minutes);
    out.extend_from_slice(b",\"enabled_at\":");
    push_opt_str(out, enabled_at);
    out.extend_from_slice(b",\"enabled_by\":");
    push_json_str(out, enabled_by);
    out.push(b'}');
}

/// Append a `CasAuditState` (byte-exact with the legacy serde render: all
/// Option fields always present, null when None - no skip_serializing_if).
fn write_cas_audit_state(out: &mut Vec<u8>, a: &CasAuditState) {
    out.extend_from_slice(b"{\"status\":");
    push_json_str(out, &a.status);
    out.extend_from_slice(b",\"files_checked\":");
    push_u64(out, a.files_checked);
    out.extend_from_slice(b",\"files_total\":");
    push_u64(out, a.files_total);
    out.extend_from_slice(b",\"errors\":");
    push_u64(out, a.errors);
    out.extend_from_slice(b",\"unreferenced\":");
    push_u64(out, a.unreferenced);
    out.extend_from_slice(b",\"corrupted\":");
    push_u64(out, a.corrupted);
    out.extend_from_slice(b",\"unreadable\":");
    push_u64(out, a.unreadable);
    out.extend_from_slice(b",\"corrupted_list\":[");
    for (i, hash) in a.corrupted_hashes.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        push_json_str(out, hash);
    }
    out.extend_from_slice(b"],\"started_at\":");
    push_opt_str(out, a.started_at.as_deref());
    out.extend_from_slice(b",\"eta_seconds\":");
    push_opt_u64(out, a.eta_seconds);
    out.push(b'}');
}

/// Append a `RebuildState`. Consistent field order with
/// [`write_cas_audit_state`]: Option fields always present, null when None.
fn write_rebuild_state(out: &mut Vec<u8>, s: &RebuildState) {
    out.extend_from_slice(b"{\"status\":");
    push_json_str(out, &s.status);
    out.extend_from_slice(b",\"files_scanned\":");
    push_u64(out, s.files_scanned);
    out.extend_from_slice(b",\"manifests_written\":");
    push_u64(out, s.manifests_written);
    out.extend_from_slice(b",\"instances_indexed\":");
    push_u64(out, s.instances_indexed);
    out.extend_from_slice(b",\"skipped\":");
    push_u64(out, s.skipped);
    out.extend_from_slice(b",\"errors\":");
    push_u64(out, s.errors);
    out.extend_from_slice(b",\"started_at\":");
    push_opt_str(out, s.started_at.as_deref());
    out.extend_from_slice(b",\"finished_at\":");
    push_opt_str(out, s.finished_at.as_deref());
    out.extend_from_slice(b",\"sources\":[");
    for (i, src) in s.sources.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        push_json_str(out, src);
    }
    out.extend_from_slice(b"],\"dry_run\":");
    out.extend_from_slice(if s.dry_run { b"true" } else { b"false" });
    out.push(b'}');
}

/// Append a `DeleteResponse` (byte-exact with the legacy serde render: field
/// order deleted_instances/deleted_series/deleted_studies/files_deleted/
/// files_retained_dedup/erase/bytes_to_free/errors as a string array).
fn write_delete_response(out: &mut Vec<u8>, r: &DeleteResponse) {
    out.extend_from_slice(b"{\"deleted_instances\":");
    push_u64(out, r.deleted_instances);
    out.extend_from_slice(b",\"deleted_series\":");
    push_u64(out, r.deleted_series);
    out.extend_from_slice(b",\"deleted_studies\":");
    push_u64(out, r.deleted_studies);
    out.extend_from_slice(b",\"files_deleted\":");
    push_u64(out, r.files_deleted);
    out.extend_from_slice(b",\"files_retained_dedup\":");
    push_u64(out, r.files_retained_dedup);
    out.extend_from_slice(b",\"erase\":");
    push_json_str(out, &r.erase);
    out.extend_from_slice(b",\"bytes_to_free\":");
    push_u64(out, r.bytes_to_free);
    out.extend_from_slice(b",\"errors\":[");
    for (i, e) in r.errors.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        push_json_str(out, e);
    }
    out.extend_from_slice(b"]}");
}

// -------------------------------------------------------------------------
// IOCM recovery surface: list / view / restore sequestered instances
//
// A Clarus admin extension. There is NO standard DICOM/IHE un-reject: the
// standard defines how to hide an instance, not how to make it visible again,
// and the only widely deployed restore mechanism is dcm4chee's private
// 99DCM4CHEE Revoke code, which this server deliberately does not adopt. These
// three routes are therefore excluded from the conformance claims; they exist
// so an operator can still answer "what did that rejection hide, and what did
// it contain" without a second copy of the archive.
//
// Gated by admin_cidrs, NOT by expose_rejected_cidrs: this is the highest
// privilege surface, and the compensating control for reversing a clinical
// sequester is the immutable audit row, not an extra gate.
//
// ap101: N/A for all three. LIST and RESTORE are manifest-scan / manifest-write
// cold paths (the same class as admin_instances_search and mark_rejected);
// VIEW reads CAS but only on a per-operator admin request, never on the hot
// QIDO/WADO path.
// -------------------------------------------------------------------------

/// Query filters for `GET /admin/v1/instances/hidden`.
#[derive(Debug, Default, Clone)]
pub struct HiddenInstanceParams {
    pub study_uid: Option<String>,
    pub series_uid: Option<String>,
    pub sop_instance_uid: Option<String>,
    /// One of the four standard reason codes (113001/113037/113038/113039).
    pub reason: Option<u32>,
    pub limit: u64,
    pub offset: u64,
}

/// One row of the hidden-instance listing.
pub struct HiddenInstanceSummary {
    pub sop_instance_uid: String,
    pub study_uid: String,
    pub series_uid: String,
    pub patient_id: String,
    pub patient_name: String,
    pub modality: String,
    pub reason_code: u32,
    pub reason_meaning: &'static str,
    pub size_bytes: i64,
}

/// The human-readable meaning of a rejection reason code.
///
/// The four codes are quoted from `standard/ps3.3/rejection-note-reasons.json`
/// (PS3.16 CID 7010). An unknown code is reported as such rather than being
/// folded into a known one: a manifest may predate this table or be newer than
/// it, and inventing a meaning would mis-describe a clinical decision.
pub fn reason_meaning(code: u32) -> &'static str {
    match code {
        113_001 => "Rejected for Quality Reasons",
        113_037 => "Rejected for Patient Safety Reasons",
        113_038 => "Incorrect Modality Worklist Entry",
        113_039 => "Data Retention Policy Expired",
        _ => "Unknown rejection reason",
    }
}

/// `GET /admin/v1/instances/hidden`.
///
/// A manifest scan for instances still carrying a rejection reason. The scan
/// mirrors `manifest_instances_search_general` (every `*.manifest`, unreadable
/// ones skipped) because a hidden instance is by definition invisible to the
/// normal indexes - there is nothing cheaper to query.
pub fn admin_instances_hidden(
    state: &AppState,
    params: &HiddenInstanceParams,
    is_auditor: bool,
) -> Result<AdminResp, AdminErr> {
    let limit = params.limit.min(500);
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);

    let mut hits: Vec<HiddenInstanceSummary> = Vec::new();
    let entries = match std::fs::read_dir(manifest_dir) {
        Ok(e) => e,
        Err(_) => {
            let mut body = Vec::new();
            write_hidden_summaries(&mut body, &hits);
            return Ok(ok_bytes(body));
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("manifest") {
            continue;
        }
        let m = match read_manifest(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let st_uid = attr_text(&m.study.attrs, tag::STUDY_INSTANCE_UID).unwrap_or_default();
        if let Some(f) = params.study_uid.as_deref()
            && st_uid != f
        {
            continue;
        }
        let st_patient_id = attr_text(&m.study.attrs, tag::PATIENT_ID).unwrap_or_default();
        let st_patient = attr_text(&m.study.attrs, tag::PATIENT_NAME).unwrap_or_default();
        for series in &m.series {
            let se_uid = attr_text(&series.attrs, tag::SERIES_INSTANCE_UID).unwrap_or_default();
            if let Some(f) = params.series_uid.as_deref()
                && se_uid != f
            {
                continue;
            }
            let se_modality = attr_text(&series.attrs, tag::MODALITY).unwrap_or_default();
            for inst in &series.instances {
                if !inst.is_rejected() {
                    continue;
                }
                let inst_uid = attr_text(&inst.attrs, tag::SOP_INSTANCE_UID).unwrap_or_default();
                if let Some(f) = params.sop_instance_uid.as_deref()
                    && inst_uid != f
                {
                    continue;
                }
                if let Some(f) = params.reason
                    && inst.rejection_reason != f
                {
                    continue;
                }
                hits.push(HiddenInstanceSummary {
                    sop_instance_uid: inst_uid,
                    study_uid: st_uid.clone(),
                    series_uid: se_uid.clone(),
                    patient_id: st_patient_id.clone(),
                    // Names are never the key and are masked for an auditor,
                    // exactly like the ordinary admin instance search.
                    patient_name: if is_auditor {
                        mask_name(&st_patient)
                    } else {
                        st_patient.clone()
                    },
                    modality: se_modality.clone(),
                    reason_code: inst.rejection_reason,
                    reason_meaning: reason_meaning(inst.rejection_reason),
                    size_bytes: i64::try_from(inst.file_size).unwrap_or(i64::MAX),
                });
            }
        }
    }
    // Newest reason first is not meaningful (there is no timestamp on the
    // record); SOP Instance UID gives a stable, repeatable page order so an
    // operator can page without rows jumping between requests.
    hits.sort_by(|a, b| a.sop_instance_uid.cmp(&b.sop_instance_uid));

    let start = usize::try_from(params.offset)
        .unwrap_or(usize::MAX)
        .min(hits.len());
    let end = start
        .saturating_add(usize::try_from(limit).unwrap_or(usize::MAX))
        .min(hits.len());
    let page: Vec<HiddenInstanceSummary> = hits.drain(start..end).collect();
    let mut body = Vec::new();
    write_hidden_summaries(&mut body, &page);
    Ok(ok_bytes(body))
}

/// Append a JSON array of [`HiddenInstanceSummary`].
fn write_hidden_summaries(out: &mut Vec<u8>, items: &[HiddenInstanceSummary]) {
    out.push(b'[');
    for (i, m) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(b"{\"sop_instance_uid\":");
        push_json_str(out, &m.sop_instance_uid);
        out.extend_from_slice(b",\"study_uid\":");
        push_json_str(out, &m.study_uid);
        out.extend_from_slice(b",\"series_uid\":");
        push_json_str(out, &m.series_uid);
        out.extend_from_slice(b",\"patient_id\":");
        push_json_str(out, &m.patient_id);
        out.extend_from_slice(b",\"patient_name\":");
        push_json_str(out, &m.patient_name);
        out.extend_from_slice(b",\"modality\":");
        push_json_str(out, &m.modality);
        out.extend_from_slice(b",\"reason_code\":");
        push_u64(out, u64::from(m.reason_code));
        out.extend_from_slice(b",\"reason_meaning\":");
        push_json_str(out, m.reason_meaning);
        out.extend_from_slice(b",\"size_bytes\":");
        push_i64(out, m.size_bytes);
        out.push(b'}');
    }
    out.push(b']');
}

/// `GET /admin/v1/instances/raw?sop_instance_uid=<uid>`.
///
/// The retained bytes of one instance, whether hidden or not. Deliberately not
/// restricted to hidden instances: an operator investigating a rejection needs
/// the referenced images as well as the rejected object, and this route is
/// already behind the admin gate.
///
/// The read is audited by the caller through the access journal (an admin
/// reading a hidden instance is exactly the event an auditor must be able to
/// see), so this function stays pure and returns the bytes plus the identifiers
/// the journal row needs.
///
/// ap101: N/A - one CAS read on a per-operator admin request.
pub struct RawInstance {
    pub bytes: Vec<u8>,
    pub study_uid: String,
    pub series_uid: String,
    pub patient_id: String,
}

pub fn admin_instance_raw(state: &AppState, sop_uid: &str) -> Result<RawInstance, AdminErr> {
    let not_found = || {
        let mut b = Vec::new();
        write_err_body(&mut b, "Not Found", "Unknown SOP Instance UID");
        err_bytes(404, b)
    };
    let study_uid = match state.shadow.sop_owner_study(sop_uid) {
        Some(s) => s,
        None => return Err(not_found()),
    };
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let path = manifest_path(manifest_dir, &study_uid);
    let m = match read_manifest(&path) {
        Ok(m) => m,
        Err(_) => return Err(not_found()),
    };
    let patient_id = attr_text(&m.study.attrs, tag::PATIENT_ID).unwrap_or_default();
    for series in &m.series {
        let se_uid = attr_text(&series.attrs, tag::SERIES_INSTANCE_UID).unwrap_or_default();
        for inst in &series.instances {
            let inst_uid = attr_text(&inst.attrs, tag::SOP_INSTANCE_UID).unwrap_or_default();
            if inst_uid != sop_uid {
                continue;
            }
            // Resolve through the same CAS read the serve path uses, so an
            // instance missing from storage reports the same way it would to
            // a WADO client rather than inventing a second failure mode.
            let tier = crate::handlers::wado::helpers::tier_from_u8(inst.storage_tier);
            let cas_key = crate::handlers::wado::helpers::cas_hex_key(inst);
            let bytes = match crate::dispatch_streams::read_cas_bytes(state, &cas_key, tier) {
                Ok(b) => b,
                Err(_) => return Err(not_found()),
            };
            return Ok(RawInstance {
                bytes,
                study_uid: study_uid.clone(),
                series_uid: se_uid,
                patient_id,
            });
        }
    }
    Err(not_found())
}

/// Restore response, mirroring [`DeleteResponse`].
pub struct RestoreResponse {
    pub restored_instances: u64,
    pub errors: Vec<String>,
}

fn write_restore_response(out: &mut Vec<u8>, r: &RestoreResponse) {
    out.extend_from_slice(b"{\"restored_instances\":");
    push_u64(out, r.restored_instances);
    out.extend_from_slice(b",\"errors\":[");
    for (i, e) in r.errors.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        push_json_str(out, e);
    }
    out.extend_from_slice(b"]}");
}

/// Native parse of the restore request body.
fn parse_restore_request(
    body: &[u8],
) -> Result<(Option<Vec<String>>, Option<String>), &'static str> {
    let mut uids = None;
    let mut reason = None;
    if let Some(pairs) = parse_json_object(body) {
        for (k, v) in pairs {
            match k.as_str() {
                "sop_instance_uids" => {
                    if let Some(a) = json_str_array(&v) {
                        uids = Some(a);
                    }
                }
                "reason" => {
                    if let JsonVal::Str(s) = v {
                        reason = Some(s);
                    }
                }
                _ => {}
            }
        }
    }
    // The same refusal the delete path applies: an operator-supplied value that
    // is not a UID is a bad request, not an instance that happens to be missing.
    if let Some(list) = uids.as_deref() {
        for uid in list {
            if clarus_core::uid::validate_path_uid(uid).is_err() {
                return Err("Invalid DICOM UID");
            }
        }
    }
    Ok((uids, reason))
}

/// `POST /admin/v1/instances/restore`.
///
/// Clears the rejection reason on named instances, which is what makes them
/// visible again on every surface: the read path consults the record and
/// nothing else, so there is no second switch to flip.
///
/// Per instance, in this order (the order is the contract):
/// 1. resolve the owning study and capture PatientID + the reason being
///    cleared;
/// 2. append the immutable audit row - the trail must exist before the
///    visibility change, so a crash leaves evidence of an attempt rather than
///    an unexplained un-sequester;
/// 3. clear the reason under the manifest lease.
///
/// The KOS rejection note is never touched. It is an ordinary stored SOP and
/// remains the historical record of why the hide happened; only the flags on
/// the instances it named are cleared.
///
/// ap101: N/A - manifest write on an admin control-plane request.
pub fn admin_restore(state: &AppState, body: &[u8], username: &str) -> Result<AdminResp, AdminErr> {
    let (uids, reason) = match parse_restore_request(body) {
        Ok(parsed) => parsed,
        Err(msg) => {
            let mut b = Vec::new();
            write_err_body(&mut b, "Bad Request", msg);
            return Err(err_bytes(400, b));
        }
    };

    // FR-014: reason is mandatory, exactly as for delete. Reversing a clinical
    // sequester without recording why would make the trail useless precisely
    // when it matters most.
    let reason = match reason.as_deref() {
        Some(r) if !r.trim().is_empty() => r.trim().to_string(),
        _ => {
            let mut b = Vec::new();
            write_err_body(
                &mut b,
                "Bad Request",
                "A non-empty 'reason' field is required for audit trail",
            );
            return Err(err_bytes(400, b));
        }
    };
    let uids = match uids {
        Some(u) if !u.is_empty() => u,
        _ => {
            let mut b = Vec::new();
            write_err_body(
                &mut b,
                "Bad Request",
                "A non-empty 'sop_instance_uids' array is required",
            );
            return Err(err_bytes(400, b));
        }
    };
    if uids.len() > 500 {
        let mut b = Vec::new();
        write_err_body(
            &mut b,
            "Bad Request",
            "Maximum 500 SOPInstanceUIDs per request",
        );
        return Err(err_bytes(400, b));
    }

    let batch_id = clarus_adapters::unique_id();
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let audit_dir = std::path::Path::new(&state.config.btree.audit_dir);
    let mut response = RestoreResponse {
        restored_instances: 0,
        errors: Vec::new(),
    };
    // Restores are counted per reason that was actually cleared, so the metric
    // matches the audit rows one for one.
    let mut restored_by_reason: Vec<(u32, u64)> = Vec::new();

    for uid in &uids {
        let study_uid = match state.shadow.sop_owner_study(uid) {
            Some(s) => s,
            None => {
                response.errors.push(format!("Instance not found: {uid}"));
                continue;
            }
        };
        let cleared = match restore_instance_manifest(
            username,
            &reason,
            uid,
            &study_uid,
            &batch_id,
            manifest_dir,
            audit_dir,
        ) {
            Ok(Some(code)) => code,
            Ok(None) => {
                response
                    .errors
                    .push(format!("Instance not hidden (nothing to restore): {uid}"));
                continue;
            }
            Err(e) => {
                response.errors.push(format!("Error restoring {uid}: {e}"));
                continue;
            }
        };
        response.restored_instances += 1;
        match restored_by_reason.iter_mut().find(|(c, _)| *c == cleared) {
            Some(entry) => entry.1 += 1,
            None => restored_by_reason.push((cleared, 1)),
        }
    }

    for (code, count) in restored_by_reason {
        crate::metrics::record_restored_instances(code, count);
    }

    let mut out = Vec::new();
    write_restore_response(&mut out, &response);
    Ok(ok_bytes(out))
}

/// Restore one instance inside its study's manifest.
///
/// Returns the reason code that was cleared, or `None` when the instance was
/// already visible (which is not an error: restore is idempotent, and saying
/// "nothing to do" is more useful than a success that changed nothing).
#[allow(clippy::too_many_arguments)]
fn restore_instance_manifest(
    username: &str,
    reason: &str,
    sop_uid: &str,
    study_uid: &str,
    batch_id: &str,
    manifest_dir: &std::path::Path,
    audit_dir: &std::path::Path,
) -> Result<Option<u32>, String> {
    let mv_path = manifest_path(manifest_dir, study_uid);
    let m = read_manifest(&mv_path).map_err(|e| format!("read manifest: {e}"))?;

    // Capture what is about to be reversed BEFORE any mutation: the reason
    // code is the record of which clinical decision this restore undoes, and
    // PatientID is the identifier the trail carries (names are never written).
    let mut cleared: Option<u32> = None;
    for series in &m.series {
        for inst in &series.instances {
            let is_target = inst.attrs.iter().any(|(t, v)| {
                *t == tag::SOP_INSTANCE_UID && matches!(v, AttrValue::Text(s) if s == sop_uid)
            });
            if is_target && inst.is_rejected() {
                cleared = Some(inst.rejection_reason);
            }
        }
    }
    let Some(reason_code) = cleared else {
        return Ok(None);
    };
    let patient_id = attr_text(&m.study.attrs, tag::PATIENT_ID).unwrap_or_default();

    // FR-014 order: durable evidence BEFORE the visibility change.
    append_record(
        audit_dir,
        &AuditEvent::Restore {
            batch_id: batch_id.to_string(),
            user: username.to_string(),
            reason: reason.to_string(),
            reason_code,
            target: sop_uid.to_string(),
            ts: crate::audit_log::utc_now_iso(),
            patient_id,
            count: 1,
        },
    )
    .map_err(|e| format!("audit append failed; restore aborted: {e}"))?;

    // `mark_restored` takes the lease itself and re-reads under it, so a STOW
    // commit that landed during the audit append cannot be lost.
    let changed = clarus_core::stow::mark_restored(manifest_dir, study_uid, &[sop_uid])
        .map_err(|e| format!("mark restored: {e}"))?;
    if changed == 0 {
        // Raced with another restore; the audit row already recorded the
        // intent, which is the honest outcome - the operator did ask for it.
        return Ok(None);
    }
    // No `on_study_committed`: a rejection flag changes no study-level QIDO
    // attribute, so re-stamping the index would overwrite a valid entry.
    Ok(Some(reason_code))
}

/// Look up instances by SOP Instance UID via a manifest scan (6b).
///
/// Reads every `*.manifest` in `manifest_dir`, collects the instances whose
/// SOP Instance UID equals `sop_uid`, then applies `offset`/`limit` and the
/// auditor PII masking. Unreadable manifests are skipped (best-effort scan:
/// a study being rewritten is invisible until its atomic commit lands).
fn manifest_instance_search(
    manifest_dir: &std::path::Path,
    sop_uid: &str,
    limit: u64,
    offset: u64,
    is_auditor: bool,
) -> Result<AdminResp, AdminErr> {
    let mut hits: Vec<MaskedInstanceSummary> = Vec::new();
    let entries = match std::fs::read_dir(manifest_dir) {
        Ok(e) => e,
        Err(_) => {
            let mut body = Vec::new();
            write_masked_summaries(&mut body, &hits);
            return Ok(ok_bytes(body));
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("manifest") {
            continue;
        }
        let m = match read_manifest(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let study_uid = attr_text(&m.study.attrs, tag::STUDY_INSTANCE_UID).unwrap_or_default();
        let patient_name = attr_text(&m.study.attrs, tag::PATIENT_NAME).unwrap_or_default();
        let study_date = attr_text(&m.study.attrs, tag::STUDY_DATE);
        let study_description = attr_text(&m.study.attrs, tag::STUDY_DESCRIPTION);
        for series in &m.series {
            let series_uid = attr_text(&series.attrs, tag::SERIES_INSTANCE_UID).unwrap_or_default();
            let modality = attr_text(&series.attrs, tag::MODALITY).unwrap_or_default();
            for inst in &series.instances {
                let inst_uid = attr_text(&inst.attrs, tag::SOP_INSTANCE_UID).unwrap_or_default();
                if inst_uid != sop_uid {
                    continue;
                }
                hits.push(MaskedInstanceSummary {
                    sop_instance_uid: inst_uid,
                    patient_name: if is_auditor {
                        mask_name(&patient_name)
                    } else {
                        patient_name.clone()
                    },
                    study_uid: study_uid.clone(),
                    series_uid: series_uid.clone(),
                    modality: modality.clone(),
                    study_date: study_date.clone(),
                    study_description: if is_auditor {
                        None
                    } else {
                        study_description.clone()
                    },
                    size_bytes: i64::try_from(inst.file_size).unwrap_or(i64::MAX),
                });
            }
        }
    }
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(hits.len());
    let end = start
        .saturating_add(usize::try_from(limit).unwrap_or(usize::MAX))
        .min(hits.len());
    let out: Vec<MaskedInstanceSummary> = hits.drain(start..end).collect();
    // Byte-exact with the legacy serde render (array of MaskedInstanceSummary).
    let mut body = Vec::new();
    write_masked_summaries(&mut body, &out);
    Ok(ok_bytes(body))
}

/// General admin instance search walking the B-tree manifests (post-SQLite
/// source of truth, 2.5). Mirrors the former `SqliteBackend
/// search_instances` filter semantics: `patient_name` and `study_description`
/// use substring match (`*` is a wildcard); patient_uid/study_uid/series_uid/
/// modality/accession_number/study_date use equality; ordered by study_date
/// DESC then limit/offset. Returns `Vec<MaskedInstanceSummary>` (auditor-masked),
/// matching the legacy render and the SOP-UID manifest search.
#[allow(clippy::too_many_arguments)]
fn manifest_instances_search_general(
    manifest_dir: &std::path::Path,
    patient_name: Option<&str>,
    patient_uid: Option<&str>,
    study_uid: Option<&str>,
    series_uid: Option<&str>,
    modality: Option<&str>,
    accession_number: Option<&str>,
    study_description: Option<&str>,
    study_date: Option<&str>,
    limit: u64,
    offset: u64,
    is_auditor: bool,
) -> Vec<MaskedInstanceSummary> {
    // Pattern helpers mirroring SQLite LIKE, which is what this search
    // replaced: `*` stands for `%` (match any run of characters), and the
    // comparison is case-insensitive. Trimming the wildcards and doing a
    // `contains` handled only the leading and trailing forms; an interior
    // wildcard (`Iva*ov`) was searched for as a literal `*`, so a pattern the
    // old backend matched returned nothing.
    let like = |pat: &str| -> String { pat.replace('*', "%") };
    let matches_pat = |field: &str, pat: &str| -> bool {
        if pat.is_empty() || pat == "%" {
            return true;
        }
        like_match(&field.to_lowercase(), &pat.to_lowercase())
    };

    let mut hits: Vec<MaskedInstanceSummary> = Vec::new();
    let entries = match std::fs::read_dir(manifest_dir) {
        Ok(e) => e,
        Err(_) => return hits,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("manifest") {
            continue;
        }
        let m = match read_manifest(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let st_uid = attr_text(&m.study.attrs, tag::STUDY_INSTANCE_UID).unwrap_or_default();
        let st_patient = attr_text(&m.study.attrs, tag::PATIENT_NAME).unwrap_or_default();
        let st_patient_id = attr_text(&m.study.attrs, tag::PATIENT_ID).unwrap_or_default();
        let st_date = attr_text(&m.study.attrs, tag::STUDY_DATE).unwrap_or_default();
        let st_desc = attr_text(&m.study.attrs, tag::STUDY_DESCRIPTION).unwrap_or_default();
        let st_accession = attr_text(&m.study.attrs, tag::ACCESSION_NUMBER).unwrap_or_default();
        // Study-level filters.
        if let Some(f) = study_uid
            && st_uid != f
        {
            continue;
        }
        if let Some(f) = patient_uid
            && st_patient_id != f
        {
            continue;
        }
        if let Some(f) = accession_number
            && st_accession != f
        {
            continue;
        }
        if let Some(f) = study_date
            && st_date != f
        {
            continue;
        }
        if let Some(f) = patient_name {
            let pat = like(f);
            if !matches_pat(&st_patient, &pat) {
                continue;
            }
        }
        if let Some(f) = study_description {
            let pat = like(f);
            if !matches_pat(&st_desc, &pat) {
                continue;
            }
        }
        for series in &m.series {
            let se_uid = attr_text(&series.attrs, tag::SERIES_INSTANCE_UID).unwrap_or_default();
            let se_modality = attr_text(&series.attrs, tag::MODALITY).unwrap_or_default();
            // Series-level filters.
            if let Some(f) = series_uid
                && se_uid != f
            {
                continue;
            }
            if let Some(f) = modality
                && se_modality != f
            {
                continue;
            }
            for inst in &series.instances {
                let inst_uid = attr_text(&inst.attrs, tag::SOP_INSTANCE_UID).unwrap_or_default();
                hits.push(MaskedInstanceSummary {
                    sop_instance_uid: inst_uid,
                    patient_name: if is_auditor {
                        mask_name(&st_patient)
                    } else {
                        st_patient.clone()
                    },
                    study_uid: st_uid.clone(),
                    series_uid: se_uid.clone(),
                    modality: se_modality.clone(),
                    study_date: (!st_date.is_empty()).then_some(st_date.clone()),
                    study_description: if is_auditor {
                        None
                    } else {
                        (!st_desc.is_empty()).then_some(st_desc.clone())
                    },
                    size_bytes: i64::try_from(inst.file_size).unwrap_or(i64::MAX),
                });
            }
        }
    }
    // ORDER BY study_date DESC (newest first); missing date sorts last.
    // The SOP Instance UID breaks ties. `sort_by` is stable, so without a
    // tie-break the order comes from the manifest walk - which is the order the
    // filesystem happens to return entries in. Two pages of one search could
    // then disagree about what page 2 is: an instance could appear on both, or
    // on neither, and nothing in the request would explain it. The UID is
    // unique, so the order is total and pagination is repeatable.
    hits.sort_by(|a, b| {
        let an = a.study_date.as_deref().unwrap_or("");
        let bn = b.study_date.as_deref().unwrap_or("");
        bn.cmp(an)
            .then_with(|| a.sop_instance_uid.as_str().cmp(b.sop_instance_uid.as_str()))
    });
    let start = (offset as usize).min(hits.len());
    let end = start.saturating_add(limit as usize).min(hits.len());
    hits.drain(start..end).collect()
}

/// The first textual value of `tag` in an attribute list, if any.
///
/// `UInt` values are rendered as their decimal form so numeric attributes
/// (e.g. StudyDate stored as `YYYYMMDD`) surface as strings.
fn attr_text(attrs: &[(u32, AttrValue)], tag: u32) -> Option<String> {
    attrs
        .iter()
        .find(|(t, _)| *t == tag)
        .and_then(|(_, v)| match v {
            AttrValue::Text(s) => Some(s.clone()),
            AttrValue::UInt(n) => Some(n.to_string()),
            _ => None,
        })
}

// -- metrics_history ----------------------------------------------------

pub struct MetricsHistoryParams {
    pub range: String,
}

pub struct MetricsHistoryResponse {
    pub range: String,
    pub interval_seconds: u64,
    pub points: Vec<MetricPoint>,
}

/// `GET /admin/v1/metrics/history`.
pub fn admin_metrics_history(
    state: &AppState,
    params: &MetricsHistoryParams,
) -> Result<AdminResp, AdminErr> {
    let (max_points, interval_seconds) = match params.range.as_str() {
        "6h" => (360, 60),
        "24h" => (1440, 60),
        "7d" => (1440, 360),
        _ => (60, 60),
    };
    let points: Vec<MetricPoint> = state
        .metrics_ring
        .read()
        .map(|buffer| {
            let len = buffer.len();
            let skip = len.saturating_sub(max_points as usize);
            buffer.iter().skip(skip).cloned().collect()
        })
        .unwrap_or_default();
    // Byte-exact with the legacy serde render (MetricsHistoryResponse field
    // order, MetricPoint fields per point).
    let mut body = Vec::new();
    body.extend_from_slice(b"{\"range\":");
    push_json_str(&mut body, &params.range);
    body.extend_from_slice(b",\"interval_seconds\":");
    push_u64(&mut body, interval_seconds);
    body.extend_from_slice(b",\"points\":[");
    for (i, p) in points.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        body.extend_from_slice(b"{\"timestamp\":");
        push_json_str(&mut body, &p.timestamp);
        body.extend_from_slice(b",\"memory_mb\":");
        push_f64(&mut body, p.memory_mb);
        body.extend_from_slice(b",\"disk_used_mb\":");
        push_f64(&mut body, p.disk_used_mb);
        body.push(b'}');
    }
    body.extend_from_slice(b"]}");
    Ok(ok_bytes(body))
}

// -- s3_validation ------------------------------------------------------

pub struct S3ValidationResponse {
    pub reachable: bool,
    pub latency_ms: Option<u64>,
    pub bucket: Option<String>,
    pub endpoint: Option<String>,
    pub reason: Option<String>,
}

/// `GET /admin/v1/storage/s3-validation`.
///
/// The live S3 HEAD probe is async and lives in the wrapper. The pure side
/// returns the deterministic not-compiled / not-configured forms; the wrapper
/// performs the reachability probe when the `s3` feature is built.
pub fn admin_s3_validation(state: &AppState) -> Result<AdminResp, AdminErr> {
    #[cfg(not(feature = "s3"))]
    {
        let _ = state;
        let mut body = Vec::new();
        write_s3_validation(
            &mut body,
            false,
            None,
            None,
            None,
            "s3_feature_not_compiled",
        );
        Ok(ok_bytes(body))
    }

    #[cfg(feature = "s3")]
    {
        if state.s3.get().is_none() {
            let endpoint = std::env::var("CLARUS_S3_ENDPOINT").ok();
            let bucket = std::env::var("CLARUS_S3_BUCKET").ok();
            if endpoint.is_none() || bucket.is_none() {
                let mut body = Vec::new();
                write_s3_validation(&mut body, false, None, None, None, "s3_not_configured");
                return Ok(ok_bytes(body));
            }
            let mut body = Vec::new();
            write_s3_validation(
                &mut body,
                false,
                None,
                bucket.as_deref(),
                endpoint.as_deref(),
                "s3_connection_failed",
            );
            return Ok(ok_bytes(body));
        }
        // Runtime-configured but not yet probed - the wrapper performs the
        // HEAD probe and returns the live result; this fallback is only hit
        // if the wrapper chooses not to probe.
        let _ = state;
        let mut body = Vec::new();
        write_s3_validation(
            &mut body,
            false,
            None,
            Some(&env_or_default("CLARUS_S3_BUCKET")),
            Some(&env_or_default("CLARUS_S3_ENDPOINT")),
            "s3_probe_required",
        );
        Ok(ok_bytes(body))
    }
}

#[cfg(feature = "s3")]
fn env_or_default(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

// -- delete_instances ---------------------------------------------------

pub struct DeleteRequest {
    pub sop_instance_uids: Option<Vec<String>>,
    pub study_uid: Option<String>,
    pub series_uid: Option<String>,
    pub reason: Option<String>,
    pub dry_run: bool,
}

pub struct DeleteResponse {
    pub deleted_instances: u64,
    pub deleted_series: u64,
    pub deleted_studies: u64,
    pub files_deleted: u64,
    pub files_retained_dedup: u64,
    /// Lazy-erase status: `"pending"` (study - files erased by the background
    /// worker, tombstone lifted later), `"sync"` (sub-study - refcounted files
    /// unlinked as before), `"preview"` (dry_run, nothing touched).
    pub erase: String,
    /// Bytes of CAS files to be freed (from the manifest preview; no FS walk).
    pub bytes_to_free: u64,
    pub errors: Vec<String>,
}

/// Native parse of the delete request body (replaces `serde_json::from_slice`).
///
/// Returns `Err` for a `dry_run` value that is not a JSON boolean. `"dry_run":
/// "true"` used to fall through `matches!(v, JsonVal::Bool(true))` to `false`,
/// which turned a request that plainly asked for a rehearsal into a REAL
/// deletion - the one field in this body where guessing the wrong way destroys
/// data. A malformed value is refused instead of being coerced.
fn parse_delete_request(body: &[u8]) -> Result<DeleteRequest, &'static str> {
    let mut req = DeleteRequest {
        sop_instance_uids: None,
        study_uid: None,
        series_uid: None,
        reason: None,
        dry_run: false,
    };
    if let Some(pairs) = parse_json_object(body) {
        for (k, v) in pairs {
            match k.as_str() {
                "sop_instance_uids" => {
                    if let Some(a) = json_str_array(&v) {
                        req.sop_instance_uids = Some(a);
                    }
                }
                "study_uid" => {
                    if let JsonVal::Str(s) = v {
                        req.study_uid = Some(s);
                    }
                }
                "series_uid" => {
                    if let JsonVal::Str(s) = v {
                        req.series_uid = Some(s);
                    }
                }
                "reason" => {
                    if let JsonVal::Str(s) = v {
                        req.reason = Some(s);
                    }
                }
                "dry_run" => match v {
                    JsonVal::Bool(b) => req.dry_run = b,
                    // The legacy string spellings are unambiguous, so they are
                    // read rather than refused; anything else is refused.
                    JsonVal::Str(ref s) if s.eq_ignore_ascii_case("true") => req.dry_run = true,
                    JsonVal::Str(ref s) if s.eq_ignore_ascii_case("false") => req.dry_run = false,
                    _ => return Err("'dry_run' must be a boolean"),
                },
                _ => {}
            }
        }
    }
    // A UID from this body becomes a path component below (`{study_uid}.batc
    // h`, the manifest path, the tombstone path), so it has to BE a UID before
    // anything joins it. The QIDO routes already refuse the same strings; the
    // admin surface is the one place where a rewritten or traversing UID
    // would reach the B-tree index instead of a manifest.
    for uid in req.study_uid.iter().chain(req.series_uid.iter()) {
        if clarus_core::uid::validate_path_uid(uid).is_err() {
            return Err("Invalid DICOM UID");
        }
    }
    if let Some(uids) = req.sop_instance_uids.as_deref() {
        for uid in uids {
            if clarus_core::uid::validate_path_uid(uid).is_err() {
                return Err("Invalid DICOM UID");
            }
        }
    }
    Ok(req)
}

/// `POST /admin/v1/instances/delete`.
pub fn admin_delete(state: &AppState, body: &[u8], username: &str) -> Result<AdminResp, AdminErr> {
    let req = match parse_delete_request(body) {
        Ok(req) => req,
        Err(msg) => {
            let mut b = Vec::new();
            write_err_body(&mut b, "Bad Request", msg);
            return Err(err_bytes(400, b));
        }
    };

    // FR-014: reason is mandatory.
    let reason = match req.reason.as_deref() {
        Some(r) if !r.trim().is_empty() => r.trim().to_string(),
        _ => {
            let mut b = Vec::new();
            write_err_body(
                &mut b,
                "Bad Request",
                "A non-empty 'reason' field is required for audit trail",
            );
            return Err(err_bytes(400, b));
        }
    };
    let mode_count = [
        req.sop_instance_uids.is_some(),
        req.study_uid.is_some(),
        req.series_uid.is_some(),
    ]
    .iter()
    .filter(|&&x| x)
    .count();
    if mode_count != 1 {
        let mut b = Vec::new();
        write_err_body(
            &mut b,
            "Bad Request",
            "Exactly one of sop_instance_uids, study_uid, or series_uid must be provided",
        );
        return Err(err_bytes(400, b));
    }
    if let Some(ref uids) = req.sop_instance_uids
        && uids.len() > 500
    {
        let mut b = Vec::new();
        write_err_body(
            &mut b,
            "Bad Request",
            "Maximum 500 SOPInstanceUIDs per request",
        );
        return Err(err_bytes(400, b));
    }

    let batch_id = clarus_adapters::unique_id();
    let mut response = DeleteResponse {
        deleted_instances: 0,
        deleted_series: 0,
        deleted_studies: 0,
        files_deleted: 0,
        files_retained_dedup: 0,
        erase: String::new(),
        bytes_to_free: 0,
        errors: Vec::new(),
    };

    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let audit_dir = std::path::Path::new(&state.config.btree.audit_dir);

    // Sub-study delete. Each removed instance is resolved purely from the
    // B-tree manifest (sop_owner idx -> study -> full manifest read), the
    // evidence is a file audit row appended FIRST (FR-014: visible deletion
    // must never precede the audit line), then the manifest is rewritten with
    // the instance dropped and empty series removed. The orphaned blob's
    // refcount is the still referenced (dedup with
    // another study or a remaining instance) -> file kept; else unlinked.
    let hashes: Vec<BlobRef> = if let Some(ref uids) = req.sop_instance_uids {
        if req.dry_run {
            // Preview: the same resolution the real path uses, without the
            // audit append or the manifest rewrite. A dry run that DELETED
            // would also break the FR-014 order it is supposed to rehearse.
            for uid in uids {
                match preview_instance_delete(state, uid) {
                    Some(plan) => {
                        response.deleted_instances += 1;
                        response.bytes_to_free += plan.bytes;
                        if plan.shared {
                            response.files_retained_dedup += 1;
                        }
                    }
                    None => response.errors.push(format!("Instance not found: {uid}")),
                }
            }
            response.erase = "preview".to_string();
            return Ok(admin_delete_response(response));
        }
        let mut all_hashes = Vec::new();
        for uid in uids {
            match delete_instance_manifest(
                state,
                username,
                &reason,
                uid,
                &batch_id,
                manifest_dir,
                audit_dir,
            ) {
                Ok(Some(hash)) => {
                    all_hashes.push(hash);
                    response.deleted_instances += 1;
                }
                Ok(None) => {
                    response.errors.push(format!("Instance not found: {uid}"));
                }
                Err(e) => {
                    response.errors.push(format!("Error deleting {uid}: {e}"));
                }
            }
        }
        response.erase = "sync".to_string();
        all_hashes
    } else if let Some(ref study_uid) = req.study_uid {
        return admin_delete_study(
            state, &req, &reason, study_uid, username, &batch_id, response,
        );
    } else if let Some(ref series_uid) = req.series_uid {
        if req.dry_run {
            // Preview: report what WOULD go, touch nothing. The instance
            // list of the series is what a real delete removes, so the same
            // manifest sources answer both.
            let plan = preview_series_delete(state, series_uid);
            response.deleted_series = 1;
            response.deleted_instances = plan.instances;
            response.bytes_to_free = plan.bytes;
            response.files_retained_dedup = plan.shared;
            response.erase = "preview".to_string();
            return Ok(admin_delete_response(response));
        }
        response.erase = "sync".to_string();
        // Resolved BEFORE the rewrite: once the series leaves the manifest its
        // instance UIDs are gone, and the derived objects that name those
        // instances individually would survive the deletion.
        let scope_instances = series_instance_uids(state, series_uid);
        match delete_series_manifest(
            state,
            username,
            &reason,
            series_uid,
            &batch_id,
            manifest_dir,
            audit_dir,
        ) {
            Ok((h, count)) => {
                // Counted only once the rewrite succeeded: setting it before
                // the call made a failed delete answer 200 claiming one series
                // gone, which is the one thing an operator cannot check from
                // outside.
                response.deleted_series = 1;
                response.deleted_instances = count;
                // Only the derived objects that point at THIS series go. An
                // annotation of the study's other series is not part of this
                // series and must survive it.
                let cascaded = cascade_delete(
                    state,
                    &crate::handlers::admin::cascade::StudyScope::Series {
                        series_uid,
                        instance_uids: scope_instances,
                    },
                    username,
                    &batch_id,
                );
                if cascaded > 0 {
                    crate::clog_event!(
                        crate::clog::Level::Info,
                        "SERIES_DERIVED_DELETED",
                        "",
                        "series delete cascade user={} series={} derived={} batch={}",
                        username,
                        series_uid,
                        cascaded,
                        batch_id
                    );
                }
                h
            }
            Err(e) => {
                response
                    .errors
                    .push(format!("Error deleting series {series_uid}: {e}"));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    // Sub-study deletes: decide each orphaned blob's fate via the
    // live-manifest scan. Reference gone everywhere -> unlink the CAS file
    // (sync, as before); still referenced -> dedup-shared, keep it.
    // The reference set is built ONCE: a study delete resolves one blob per
    // instance, and a per-blob scan would re-read the whole manifest store
    // once per instance.
    let live_refs = live_reference_set(manifest_dir);
    for blake3_hex in &hashes {
        if live_refs
            .as_ref()
            .is_none_or(|set| set.contains(blake3_hex))
        {
            response.files_retained_dedup += 1;
        } else {
            // The blob may live in the COLD tier: the migration worker moves
            // files there and leaves the manifest's tier byte pointing at it.
            // Looking only in the hot store made the unlink miss, so the
            // delete reported nothing freed and the file stayed on disk
            // forever - removed PHI still readable, exactly what the delete
            // was asked to prevent.
            let removed = unlink_blob_every_tier(state, blake3_hex);
            match removed {
                Ok(true) => response.files_deleted += 1,
                Ok(false) => {
                    // Nothing on disk under either tier: already gone.
                    response.files_deleted += 1;
                }
                Err(e) => {
                    crate::clog_warn!(hash = %blake3_hex, error = %e, "Failed to delete CAS file");
                    response.files_retained_dedup += 1;
                }
            }
        }
    }

    // Sub-study deletes mutated live manifests in place - rebuild the RAM
    // study index once so /studies counts/rows reflect the new state (cold
    // QIDO reads scan manifests regardless, so this is a consistency refresh).
    state.study_scanner.rebuild_index();

    // Byte-exact with the legacy serde render (DeleteResponse field order).
    let mut body = Vec::new();
    write_delete_response(&mut body, &response);
    Ok(ok_bytes(body))
}

/// Unlink a blob from whichever storage tier holds it.
///
/// `Ok(true)` when a file was removed, `Ok(false)` when neither tier had it
/// (already gone - the caller reports it as freed, matching the previous
/// `NotFound` arm), `Err` on a real I/O failure.
fn unlink_blob_every_tier(state: &AppState, blake3_hex: &str) -> std::io::Result<bool> {
    let mut any_removed = false;
    let hot = state.cas.cas_path(blake3_hex);
    match std::fs::remove_file(&hot) {
        Ok(()) => any_removed = true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if let Some(ref cold) = state.cold_cas {
        let cold_path = cold.cas_path(blake3_hex);
        match std::fs::remove_file(&cold_path) {
            Ok(()) => any_removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(any_removed)
}

/// Match `text` against a SQL-LIKE pattern where `%` matches any run of
/// characters (including none) and every other character matches itself.
///
/// Iterative, with the classic single-star backtrack point: no recursion, so
/// a pattern of many `%` cannot blow the worker stack, and no allocation.
/// `_` is NOT special here - SQLite treats it as one character and QIDO's
/// `*`-only grammar has no equivalent, so a literal underscore in a name must
/// match itself.
fn like_match(text: &str, pattern: &str) -> bool {
    let t = text.as_bytes();
    let p = pattern.as_bytes();
    let (mut ti, mut pi) = (0usize, 0usize);
    // Where to resume if the current guess fails: after the last `%`, and the
    // text position that `%` currently stands for.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'%' {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            // Let the last `%` swallow one more character and retry from
            // just after it.
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    // Trailing `%` may absorb nothing; anything else left unmatched fails.
    while pi < p.len() && p[pi] == b'%' {
        pi += 1;
    }
    pi == p.len()
}

/// A deleted instance's CAS blob reference: the blake3 hex address.
type BlobRef = String;

/// Serialize a `DeleteResponse` as the route answers it.
///
/// Split out so the preview paths return through the SAME renderer as a real
/// delete: a dry run that answered a different shape would not be a rehearsal.
fn admin_delete_response(response: DeleteResponse) -> AdminResp {
    let mut body = Vec::new();
    write_delete_response(&mut body, &response);
    ok_bytes(body)
}

/// What a real instance delete would remove, resolved without touching
/// anything.
struct DeletePlan {
    bytes: u64,
    /// The blob is still referenced elsewhere, so the CAS file would survive.
    shared: bool,
}

/// Resolve one instance's delete plan from the live manifest.
///
/// Read-only by construction: no audit row, no lease, no rewrite. `None` when
/// the instance is not in the index, which is what a real delete reports as
/// "not found".
fn preview_instance_delete(state: &AppState, sop_uid: &str) -> Option<DeletePlan> {
    let study_uid = state.shadow.sop_owner_study(sop_uid)?;
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let m = read_manifest(&manifest_path(manifest_dir, &study_uid)).ok()?;
    let mut bytes = 0u64;
    let mut hash: Option<String> = None;
    for series in &m.series {
        for inst in &series.instances {
            let is_target = inst.attrs.iter().any(|(t, v)| {
                *t == tag::SOP_INSTANCE_UID && matches!(v, AttrValue::Text(s) if s == sop_uid)
            });
            if is_target {
                if hash.is_none() {
                    hash = Some(clarus_adapters::hex_encode(&inst.blake3));
                }
                bytes += inst.file_size;
            }
        }
    }
    let hash = hash?;
    Some(DeletePlan {
        bytes,
        // One blob, one answer: the full scan is the cheapest form here.
        shared: blob_is_referenced_live(manifest_dir, &hash),
    })
}

/// What a real series delete would remove.
struct SeriesDeletePlan {
    instances: u64,
    bytes: u64,
    shared: u64,
}

/// Resolve a whole series' delete plan from the live manifest. Read-only.
/// SOP Instance UIDs held by one series, read from the owning study's manifest.
///
/// The series cascade needs them to claim a derived object that names its
/// source images by ReferencedSOPInstanceUID only: the reference names an
/// instance, and the instance's series is the fact being deleted. Read from
/// the same manifests `delete_series_manifest` reads, so the two cannot
/// disagree about which series the instance belongs to.
fn series_instance_uids(state: &AppState, series_uid: &str) -> std::collections::HashSet<String> {
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let mut out = std::collections::HashSet::new();
    let Ok(entries) = std::fs::read_dir(manifest_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("manifest") {
            continue;
        }
        let Ok(m) = read_manifest(&path) else {
            continue;
        };
        for series in &m.series {
            let is_target = series.attrs.iter().any(|(t, v)| {
                *t == tag::SERIES_INSTANCE_UID && matches!(v, AttrValue::Text(s) if s == series_uid)
            });
            if !is_target {
                continue;
            }
            for inst in &series.instances {
                if let Some(uid) = inst.attrs.iter().find_map(|(t, v)| match (t, v) {
                    (t, AttrValue::Text(s)) if *t == tag::SOP_INSTANCE_UID => Some(s.clone()),
                    _ => None,
                }) {
                    out.insert(uid);
                }
            }
        }
    }
    out
}

fn preview_series_delete(state: &AppState, series_uid: &str) -> SeriesDeletePlan {
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let mut plan = SeriesDeletePlan {
        instances: 0,
        bytes: 0,
        shared: 0,
    };
    // There is no series-level index: the owning study is found by looking
    // for a study whose manifest carries the series. That is the same source
    // `delete_series_manifest` reads, so the preview cannot disagree with it.
    let Ok(entries) = std::fs::read_dir(manifest_dir) else {
        return plan;
    };
    // Built lazily on the first series hit: a preview whose series does not
    // exist must not pay for a store-wide scan.
    let mut live_refs: Option<Option<std::collections::HashSet<String>>> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("manifest") {
            continue;
        }
        let Ok(m) = read_manifest(&path) else {
            continue;
        };
        for series in &m.series {
            let is_target = series.attrs.iter().any(|(t, v)| {
                *t == tag::SERIES_INSTANCE_UID && matches!(v, AttrValue::Text(s) if s == series_uid)
            });
            if !is_target {
                continue;
            }
            for inst in &series.instances {
                let hash = clarus_adapters::hex_encode(&inst.blake3);
                plan.instances += 1;
                plan.bytes += inst.file_size;
                // `shared` is a per-series dedup count over the whole store, so
                // the reference set is built once for the series rather than
                // re-reading every manifest per instance.
                let refs = live_refs.get_or_insert_with(|| live_reference_set(manifest_dir));
                let referenced = match refs {
                    Some(set) => set.contains(&hash),
                    None => true, // can't verify -> keep the file
                };
                if referenced {
                    plan.shared += 1;
                }
            }
        }
    }
    plan
}

/// Delete one instance from its study's B-tree manifest.
///
/// FR-014 order is preserved: the file audit row is appended FIRST, then the
/// manifest is rewritten with the instance dropped. The empty series is
/// removed and an empty study manifest is allowed to stay (a sub-study delete
/// never tombstones the study itself - that is the study-level operation).
///
/// Returns `Ok(Some(blake3_hex))` on success, `Ok(None)` when
/// the SOP UID is unknown, `Err` on a hard failure (audit append or manifest
/// write). The orphaned blob's refcount is decided by the caller via
/// [`blob_is_referenced_live`].
fn delete_instance_manifest(
    state: &AppState,
    username: &str,
    reason: &str,
    sop_uid: &str,
    batch_id: &str,
    manifest_dir: &std::path::Path,
    audit_dir: &std::path::Path,
) -> Result<Option<BlobRef>, String> {
    let study_uid = match state.shadow.sop_owner_study(sop_uid) {
        Some(s) => s,
        None => return Ok(None),
    };
    let mv_path = manifest_path(manifest_dir, &study_uid);
    let m = read_manifest(&mv_path).map_err(|e| format!("read manifest: {e}"))?;

    // Locate the target instance(s) + size before removal (for the audit row).
    let mut bytes_to_free = 0u64;
    let mut count = 0u64;
    let mut target: Option<BlobRef> = None;
    for series in &m.series {
        for inst in &series.instances {
            let is_target = inst.attrs.iter().any(|(t, v)| {
                *t == tag::SOP_INSTANCE_UID && matches!(v, AttrValue::Text(s) if s == sop_uid)
            });
            if is_target {
                if target.is_none() {
                    target = Some(clarus_adapters::hex_encode(&inst.blake3));
                }
                bytes_to_free += inst.file_size;
                count += 1;
            }
        }
    }
    let Some(hash) = target else {
        return Ok(None);
    };

    // Patient identity (identifier only - names are never written): captured
    // from the in-memory study manifest BEFORE the append/tombstone order.
    let patient_id = attr_text(&m.study.attrs, tag::PATIENT_ID).unwrap_or_default();

    // FR-014: durable audit evidence BEFORE visibility changes.
    append_record(
        audit_dir,
        &AuditEvent::Delete {
            batch_id: batch_id.to_string(),
            user: username.to_string(),
            reason: reason.to_string(),
            target: sop_uid.to_string(),
            ts: crate::audit_log::utc_now_iso(),
            patient_id,
            count,
            bytes_to_free,
        },
    )
    .map_err(|e| format!("audit append failed; delete aborted: {e}"))?;

    // Serialize against a concurrent STOW commit into the same study.
    let _lease = acquire();

    // TOCTOU fix: the pre-lease copy is stale - a
    // STOW commit may have landed during the audit append. Re-read UNDER the
    // lease so the rewrite cannot lose those instances.
    let mut m = match read_manifest(&mv_path) {
        Ok(m) => m,
        Err(_) => return Ok(None), // vanished concurrently; nothing to rewrite
    };

    // B3: drop the cached entry BEFORE the rewrite; reinstall after.
    let (cache_v, _taken) = clarus_core::manifest_cache::begin_write(&study_uid);

    // Drop the instance(s) from the FRESH copy, then drop now-empty series.
    let mut dropped = 0usize;
    m.series.retain_mut(|s| {
        let before = s.instances.len();
        s.instances.retain(|i| {
            !i.attrs.iter().any(|(t, v)| {
                *t == tag::SOP_INSTANCE_UID && matches!(v, AttrValue::Text(u) if u == sop_uid)
            })
        });
        dropped += before - s.instances.len();
        !s.instances.is_empty()
    });
    if dropped == 0 {
        return Ok(None); // target gone by now; entry stays removed, safe
    }
    // Recomputed (RAM-only) counts stay consistent for in-memory consumers.
    m.study.series_count = m.series.len() as u64;
    m.study.instance_count = m.series.iter().map(|s| s.instances.len() as u64).sum();

    // The study empties when its LAST instance goes. Leaving the empty
    // manifest behind made a ghost: QIDO kept answering for a study that has
    // no instances, and nothing ever reclaimed its blobs because no
    // erase-worker tombstone existed. An emptied study must leave the store
    // the same way a study deletion takes it - rename to `.del`, RAM index
    // dropped, `.batch` sidecar for the erase - so the two paths cannot
    // disagree about what "deleted" means.
    if m.series.is_empty() {
        // The study-level evidence row. The instance row above names the
        // SOP; this one names the STUDY, which is the target the
        // `erase_complete` row will close. Both carry the same batch, so an
        // auditor reading the journal sees the instance delete and the study
        // tombstone it caused as one event.
        append_record(
            audit_dir,
            &AuditEvent::Delete {
                batch_id: batch_id.to_string(),
                user: username.to_string(),
                reason: "last instance removed".to_string(),
                target: study_uid.clone(),
                ts: crate::audit_log::utc_now_iso(),
                patient_id: attr_text(&m.study.attrs, tag::PATIENT_ID).unwrap_or_default(),
                // Zero by construction: the study holds nothing left.
                count: 0,
                bytes_to_free: 0,
            },
        )
        .map_err(|e| format!("study audit append failed; not tombstoned: {e}"))?;
        tombstone_empty_study(state, &study_uid, batch_id, manifest_dir, mv_path, &m)?;
        crate::clog_event!(
            crate::clog::Level::Info,
            "STUDY_DELETED",
            "",
            "study tombstoned by last-instance delete user={} study={} sop={} batch={}",
            username,
            study_uid,
            sop_uid,
            batch_id
        );
        return Ok(Some(hash));
    }

    write_manifest(&mv_path, &m).map_err(|e| format!("write manifest: {e}"))?;
    clarus_core::manifest_cache::end_write(
        &study_uid,
        cache_v,
        std::sync::Arc::new(m),
        std::fs::metadata(&mv_path).map(|md| md.len()).unwrap_or(0),
        manifest_dir,
    );

    Ok(Some(hash))
}

/// Delete one series from its study's B-tree manifest.
///
/// The owning study is found by scanning live manifests for a series carrying
/// the UID. FR-014 audit-append precedes the manifest rewrite. Returns
/// `(Vec<blake3_hex>, instance_count)` for the caller's refcount pass.
fn delete_series_manifest(
    state: &AppState,
    username: &str,
    reason: &str,
    series_uid: &str,
    batch_id: &str,
    manifest_dir: &std::path::Path,
    audit_dir: &std::path::Path,
) -> Result<(Vec<BlobRef>, u64), String> {
    let study_uid = find_study_for_series(manifest_dir, series_uid)
        .ok_or_else(|| format!("series not found: {series_uid}"))?;
    let mv_path = manifest_path(manifest_dir, &study_uid);
    let m = read_manifest(&mv_path).map_err(|e| format!("read manifest: {e}"))?;

    let mut hashes = Vec::new();
    let mut bytes_to_free = 0u64;
    let mut count = 0u64;
    let wanted_series = m
        .series
        .iter()
        .filter(|s| {
            s.attrs.iter().any(|(t, v)| {
                *t == tag::SERIES_INSTANCE_UID && matches!(v, AttrValue::Text(u) if u == series_uid)
            })
        })
        .count();
    if wanted_series == 0 {
        return Ok((Vec::new(), 0));
    }
    for s in &m.series {
        let is_target = s.attrs.iter().any(|(t, v)| {
            *t == tag::SERIES_INSTANCE_UID && matches!(v, AttrValue::Text(u) if u == series_uid)
        });
        if is_target {
            for inst in &s.instances {
                hashes.push(clarus_adapters::hex_encode(&inst.blake3));
                bytes_to_free += inst.file_size;
                count += 1;
            }
        }
    }

    // FR-014: durable audit evidence BEFORE visibility changes. The owning
    // study's PatientID is captured from the in-memory manifest first.
    let patient_id = attr_text(&m.study.attrs, tag::PATIENT_ID).unwrap_or_default();
    append_record(
        audit_dir,
        &AuditEvent::Delete {
            batch_id: batch_id.to_string(),
            user: username.to_string(),
            reason: reason.to_string(),
            target: series_uid.to_string(),
            ts: crate::audit_log::utc_now_iso(),
            patient_id,
            count,
            bytes_to_free,
        },
    )
    .map_err(|e| format!("audit append failed; delete aborted: {e}"))?;

    let _lease = acquire();

    // TOCTOU fix: the pre-lease copy is stale - a
    // STOW commit may have landed during the audit append. Re-read UNDER the
    // lease so the rewrite cannot lose those instances.
    let mut m = match read_manifest(&mv_path) {
        Ok(m) => m,
        Err(_) => return Ok((Vec::new(), 0)), // vanished concurrently
    };

    // B3: drop the cached entry BEFORE the rewrite; reinstall after.
    let (cache_v, _taken) = clarus_core::manifest_cache::begin_write(&study_uid);

    if !m.series.iter().any(|s| {
        s.attrs.iter().any(|(t, v)| {
            *t == tag::SERIES_INSTANCE_UID && matches!(v, AttrValue::Text(u) if u == series_uid)
        })
    }) {
        return Ok((Vec::new(), 0)); // target gone by now; entry stays removed
    }

    m.series.retain(|s| {
        !s.attrs.iter().any(|(t, v)| {
            *t == tag::SERIES_INSTANCE_UID && matches!(v, AttrValue::Text(u) if u == series_uid)
        })
    });
    m.study.series_count = m.series.len() as u64;
    m.study.instance_count = m.series.iter().map(|s| s.instances.len() as u64).sum();

    // A series delete that empties the study must leave the store the same
    // way the instance path does; otherwise a ghost study stays QIDO-visible
    // with no instances and no erase-worker tombstone to reclaim its blobs.
    if m.series.is_empty() {
        append_record(
            audit_dir,
            &AuditEvent::Delete {
                batch_id: batch_id.to_string(),
                user: username.to_string(),
                reason: "last series removed".to_string(),
                target: study_uid.clone(),
                ts: crate::audit_log::utc_now_iso(),
                patient_id: attr_text(&m.study.attrs, tag::PATIENT_ID).unwrap_or_default(),
                // Zero by construction: the study holds nothing left.
                count: 0,
                bytes_to_free: 0,
            },
        )
        .map_err(|e| format!("study audit append failed; not tombstoned: {e}"))?;
        tombstone_empty_study(state, &study_uid, batch_id, manifest_dir, mv_path, &m)?;
        crate::clog_event!(
            crate::clog::Level::Info,
            "STUDY_DELETED",
            "",
            "study tombstoned by last-series delete user={} study={} series={} batch={}",
            username,
            study_uid,
            series_uid,
            batch_id
        );
        return Ok((hashes, count));
    }

    write_manifest(&mv_path, &m).map_err(|e| format!("write manifest: {e}"))?;
    clarus_core::manifest_cache::end_write(
        &study_uid,
        cache_v,
        std::sync::Arc::new(m),
        std::fs::metadata(&mv_path).map(|md| md.len()).unwrap_or(0),
        manifest_dir,
    );

    Ok((hashes, count))
}

/// Retire a study that has just lost its last series or instance.
///
/// Both delete paths must land the study in the same terminal state, or they
/// drift: one tombstone, one ghost that QIDO keeps answering and no
/// erase-worker will ever reclaim. The caller appends the study-level audit
/// row first (FR-014 order: evidence before visibility), then hands the
/// already-emptied manifest here.
///
/// The caller holds the write lease, so the RAM manifest cache entry taken by
/// `begin_write` is deliberately left out - the study is gone.
fn tombstone_empty_study(
    state: &AppState,
    study_uid: &str,
    batch_id: &str,
    manifest_dir: &std::path::Path,
    mv_path: std::path::PathBuf,
    m: &clarus_core::manifest::Manifest,
) -> Result<(), String> {
    // Persist the emptied manifest before the rename so the tombstone carries
    // accurate counts for the erase-worker's accounting.
    write_manifest(&mv_path, m).map_err(|e| format!("write manifest: {e}"))?;
    let tombstone = clarus_core::stow::tombstone_path(manifest_dir, study_uid);
    std::fs::rename(&mv_path, &tombstone).map_err(|e| format!("tombstone rename: {e}"))?;
    // Sidecar so the erase-worker closes THIS batch with erase_complete.
    let _ = std::fs::write(
        manifest_dir.join(format!("{study_uid}.batch")),
        batch_id.as_bytes(),
    );
    // The study is invisible to readers the instant the rename lands, so the
    // RAM index forgets it in the same breath.
    state.study_scanner.on_study_erased(study_uid);
    Ok(())
}

/// Scan every live manifest for any instance referencing `blake3_hex`. `true`
/// means the blob is still referenced -> dedup keeps the file; `false` means it
/// is now orphaned.
///
/// An unreadable manifest answers `true`. Skipping it and continuing answers
/// for the whole store using a set of manifests that is missing one, and the
/// skipped study is exactly the one whose blobs would be unlinked by mistake:
/// the file looks orphaned because its referrer could not be read. Deferring
/// costs disk until the next pass; guessing costs an instance.
fn blob_is_referenced_live(manifest_dir: &std::path::Path, blake3_hex: &str) -> bool {
    match live_reference_set(manifest_dir) {
        Some(set) => set.contains(blake3_hex),
        None => true, // can't verify -> keep the file
    }
}

/// Collect the hashes of every instance in every live manifest.
///
/// Built ONCE per request: a study delete resolves one blob per instance, so
/// the per-blob form re-read and re-decoded every manifest once per instance -
/// O(instances x manifests) reads of the whole store for a single delete.
/// The set is the same evidence, read once.
///
/// `None` when a manifest could not be read or the directory could not be
/// listed: the caller must then treat every blob as referenced (see
/// [`blob_is_referenced_live`] for why guessing costs an instance).
fn live_reference_set(manifest_dir: &std::path::Path) -> Option<std::collections::HashSet<String>> {
    let rd = std::fs::read_dir(manifest_dir).ok()?;
    let mut set = std::collections::HashSet::new();
    for ent in rd.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".manifest") {
            continue;
        }
        let m = read_manifest(&ent.path()).ok()?;
        for series in &m.series {
            for inst in &series.instances {
                set.insert(clarus_adapters::hex_encode(&inst.blake3));
            }
        }
    }
    Some(set)
}

/// Find the owning study of a series UID by scanning live manifests.
fn find_study_for_series(manifest_dir: &std::path::Path, series_uid: &str) -> Option<String> {
    let mut out = None;
    if let Ok(rd) = std::fs::read_dir(manifest_dir) {
        for ent in rd.flatten() {
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".manifest") {
                continue;
            }
            let Ok(m) = read_manifest(&ent.path()) else {
                continue;
            };
            let hit = m.series.iter().any(|s| {
                s.attrs.iter().any(|(t, v)| {
                    *t == tag::SERIES_INSTANCE_UID
                        && matches!(v, AttrValue::Text(u) if u == series_uid)
                })
            });
            if hit {
                out = Some(name.trim_end_matches(".manifest").to_string());
                break;
            }
        }
    }
    out
}

/// study delete: audit-append -> tombstone rename -> B-tree manifest
/// (DbActor removed). Files are erased only by the background
/// erase-worker. `dry_run` touches nothing.
fn admin_delete_study(
    state: &AppState,
    req: &DeleteRequest,
    reason: &str,
    study_uid: &str,
    username: &str,
    batch_id: &str,
    mut response: DeleteResponse,
) -> Result<AdminResp, AdminErr> {
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let mv_path = clarus_core::stow::manifest_path(manifest_dir, study_uid);
    let del_path = clarus_core::stow::tombstone_path(manifest_dir, study_uid);

    // Preview from the live manifest (count + bytes_to_free + patient_id) -
    // no FS walk.
    //
    // When the study is ALREADY tombstoned the live manifest is gone, and
    // reading only it made the repeat-delete audit row claim zero instances
    // and zero bytes - the evidence that an investigation would read. The
    // tombstone still holds the study's shape, so it is the fallback source:
    // the numbers must describe the study, not whichever file happened to be
    // at the live path at this instant.
    let source = if mv_path.is_file() {
        &mv_path
    } else {
        &del_path
    };
    let (count, bytes_to_free, patient_id) = match clarus_core::manifest::read_manifest(source) {
        Ok(m) => {
            let c = m.series.iter().flat_map(|s| &s.instances).count() as u64;
            let b: u64 = m
                .series
                .iter()
                .flat_map(|s| &s.instances)
                .map(|i| i.file_size)
                .sum();
            let pid = attr_text(&m.study.attrs, tag::PATIENT_ID).unwrap_or_default();
            (c, b, pid)
        }
        Err(_) => (0, 0, String::new()),
    };
    response.deleted_instances = count;
    response.deleted_studies = 1;
    response.bytes_to_free = bytes_to_free;

    // Study absent both as manifest and as tombstone -> honest 404.
    if count == 0 && !del_path.exists() {
        let mut b = Vec::new();
        let msg = format!("Study {study_uid} not found");
        write_err_body(&mut b, "Not Found", &msg);
        return Err(err_bytes(404, b));
    }

    if req.dry_run {
        // Preview requests must be visible to the auditor (delete_preview row),
        // but nothing is touched: no rename, no DbActor.
        let _ = append_record(
            std::path::Path::new(&state.config.btree.audit_dir),
            &AuditEvent::DeletePreview {
                batch_id: batch_id.to_string(),
                user: username.to_string(),
                reason: reason.to_string(),
                target: study_uid.to_string(),
                ts: crate::audit_log::utc_now_iso(),
                patient_id: patient_id.clone(),
                count,
                bytes_to_free,
            },
        );
        response.erase = "preview".to_string();
        crate::clog_event!(
            crate::clog::Level::Info,
            "STUDY_DELETED",
            "",
            "study delete preview user={} study={} instances={} bytes={} batch={}",
            username,
            study_uid,
            count,
            bytes_to_free,
            batch_id
        );
        let mut body = Vec::new();
        write_delete_response(&mut body, &response);
        return Ok(ok_bytes(body));
    }

    // FR-014: the audit row is written (and fsync'd) BEFORE the tombstone makes
    // the deletion visible. A failed append aborts - visible deletion without
    // evidence is the one unacceptable outcome.
    if let Err(e) = append_record(
        std::path::Path::new(&state.config.btree.audit_dir),
        &AuditEvent::Delete {
            batch_id: batch_id.to_string(),
            user: username.to_string(),
            reason: reason.to_string(),
            target: study_uid.to_string(),
            ts: crate::audit_log::utc_now_iso(),
            patient_id,
            count,
            bytes_to_free,
        },
    ) {
        let mut b = Vec::new();
        let msg = format!("Audit append failed; delete aborted: {e}");
        write_err_body(&mut b, "Internal Server Error", &msg);
        return Err(err_bytes(500, b));
    }

    // The study is now invisible. Take the derived objects with it: the
    // workitem named from this study, and any presentation state or
    // segmentation that references into it. Runs after the tombstone so a
    // failure here cannot resurrect a study the caller was told is deleted,
    // and it never fails the request - a leftover derived object is
    // recoverable, an ambiguous deletion response is not.
    //
    // The scope was resolved from the manifest before the rename: a trained
    // "any reference" rule is what let a study delete claim a derived object
    // of an unrelated study, and after the rename there is nothing left to
    // enumerate the study's own UIDs from.
    let study_scope = study_scope_snapshot(&mv_path, study_uid);

    // Tombstone: atomic rename (instant visibility; readers filter .manifest).
    // Idempotent: an existing tombstone means it is already deleted.
    if mv_path.exists() {
        // The coordinator's in-process Mutex serializes this rename against
        // the STOW writer's manifest commit, so a delete can never be raced
        // by a concurrent commit into a resurrection window. The
        // audit append stays before the delete, which is what we want.
        let _lease = clarus_core::stow::acquire();
        // B3: drop the cached entry BEFORE the rename; no end_write - the
        // study is gone, the next get_or_load must 404 from disk.
        let _ = clarus_core::manifest_cache::begin_write(study_uid);
        std::fs::rename(&mv_path, &del_path).map_err(|e| {
            let mut b = Vec::new();
            let msg = format!("Tombstone rename failed: {e}");
            write_err_body(&mut b, "Internal Server Error", &msg);
            err_bytes(500, b)
        })?;
        // Sidecar correlating this tombstone to its audit batch_id, so the
        // erase-worker can close the loop with `erase_complete {batch_id,...}`.
        let _ = std::fs::write(
            manifest_dir.join(format!("{study_uid}.batch")),
            batch_id.as_bytes(),
        );
        // the study is now invisible to QIDO (readers filter `.manifest`),
        // so drop it from the RAM index at the same instant (no-op when no
        // index is attached).
        state.study_scanner.on_study_erased(study_uid);
        // Operational visibility: the full reason lives ONLY in the audit row
        // (the evidence medium); the log carries the batch_id to join them.
        crate::clog_event!(
            crate::clog::Level::Info,
            "STUDY_DELETED",
            "",
            "study delete user={} study={} instances={} bytes={} batch={}",
            username,
            study_uid,
            count,
            bytes_to_free,
            batch_id
        );
    }

    response.erase = "pending".to_string();
    let cascaded = cascade_delete(state, &study_scope, username, batch_id);
    if cascaded > 0 {
        crate::clog_event!(
            crate::clog::Level::Info,
            "STUDY_DERIVED_DELETED",
            "",
            "study delete cascade user={} study={} derived={} batch={}",
            username,
            study_uid,
            cascaded,
            batch_id
        );
    }

    // Byte-exact with the legacy serde render (DeleteResponse field order).
    let mut body = Vec::new();
    write_delete_response(&mut body, &response);
    Ok(ok_bytes(body))
}

/// The study and series UIDs a study contains, read from its live manifest.
///
/// Called BEFORE the tombstone rename. A missing or unreadable manifest
/// yields an empty set, and an empty set claims nothing - the cascade then
/// leaves derived objects alone rather than claiming every one in the store.
fn study_scope_snapshot<'a>(
    manifest_path: &std::path::Path,
    study_uid: &'a str,
) -> crate::handlers::admin::cascade::StudyScope<'a> {
    let mut series_uids = std::collections::HashSet::new();
    let mut instance_uids = std::collections::HashSet::new();
    let _ = (&mut series_uids, &mut instance_uids);
    if let Ok(manifest) = read_manifest(manifest_path) {
        for series in &manifest.series {
            if let Some(uid) = attr_text(&series.attrs, tag::SERIES_INSTANCE_UID) {
                series_uids.insert(uid);
            }
            for instance in &series.instances {
                if let Some(uid) = attr_text_of(instance, tag::SOP_INSTANCE_UID) {
                    instance_uids.insert(uid);
                }
            }
        }
    }
    crate::handlers::admin::cascade::StudyScope::Study {
        study_uid,
        series_uids,
        instance_uids,
    }
}

/// Remove the derived objects a scope claims, returning how many went.
///
/// The store is read first and the removals are one per object: a removal
/// that fails is logged and the rest continue, because a deletion that stops
/// half way leaves more behind than one that skips a stubborn object.
///
/// `user` and `batch_id` carry the CALLER's audit identity: when the last
/// instance of a study goes, its tombstone must be closed by the same
/// `erase_complete` row the caller's `delete` row already names, so the
/// cascade joins that batch instead of opening one nobody can reconcile.
fn cascade_delete(
    state: &AppState,
    scope: &crate::handlers::admin::cascade::StudyScope<'_>,
    user: &str,
    batch_id: &str,
) -> usize {
    use crate::handlers::admin::cascade::{self, StudyScope};
    let mut removed = 0usize;
    if let StudyScope::Study { study_uid, .. } = scope
        && cascade::remove_workitem_for_study(state, study_uid)
    {
        removed += 1;
    }
    for target in cascade::collect(state, scope) {
        match remove_derived_object(state, &target, user, batch_id) {
            Ok(true) => removed += 1,
            Ok(false) => {}
            Err(error) => {
                crate::clog_warn!(
                    sop = %target.sop_instance_uid,
                    error = %error,
                    "could not remove a derived object"
                );
            }
        }
    }
    removed
}

/// Remove one derived object from the store.
///
/// The object is tombstoned through the same path a study uses, so a derived
/// object leaves exactly the way any other stored object does.
fn remove_derived_object(
    state: &AppState,
    target: &crate::handlers::admin::cascade::Removal,
    user: &str,
    batch_id: &str,
) -> Result<bool, String> {
    use clarus_core::stow::{acquire, manifest_path, tombstone_path};

    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    // Manifests are named `{study_uid}.manifest`, so the object's OWNING
    // STUDY is the key. Keying by the series UID made every read miss and the
    // cascade a silent no-op.
    let path = manifest_path(manifest_dir, &target.study_instance_uid);
    // The lease covers the read-modify-write: editing outside it let a
    // concurrent STOW commit overwrite the removal.
    let _lease = acquire();
    let Ok(manifest) = read_manifest(&path) else {
        // No live manifest for the owning study: the object was already taken
        // with its own study, so there is nothing left to do.
        return Ok(false);
    };
    let mut manifest = manifest;
    let before: usize = manifest.series.iter().map(|s| s.instances.len()).sum();
    // E1: the CAS blob this removal orphans, captured BEFORE the instance
    // leaves the manifest - after the rewrite nothing names it any more, so
    // this is the last moment the hash is still reachable from live data.
    let mut orphan_blob: Option<([u8; 32], u8)> = None;
    for series in &manifest.series {
        for instance in &series.instances {
            if attr_text_of(instance, tag::SOP_INSTANCE_UID).as_deref()
                == Some(target.sop_instance_uid.as_str())
            {
                orphan_blob = Some((instance.blake3, instance.storage_tier));
            }
        }
    }
    for series in &mut manifest.series {
        series.instances.retain(|instance| {
            attr_text_of(instance, tag::SOP_INSTANCE_UID).as_deref()
                != Some(target.sop_instance_uid.as_str())
        });
    }
    manifest
        .series
        .retain(|series| !series.instances.is_empty());
    let after: usize = manifest.series.iter().map(|s| s.instances.len()).sum();
    if before == after {
        return Ok(false);
    }

    // E2: every removed SOP needs its OWN audit row.
    //
    // The caller's delete row names the study/series it was addressed to. A
    // cascade removal happens inside a DIFFERENT live study - possibly another
    // patient's - so no existing row mentions this object, and without one the
    // archive loses a clinical object with no record. The row goes in BEFORE
    // the manifest rewrite: a deletion that cannot be recorded must not happen
    // (FR-014), and the caller turns this into its own per-object error.
    let owner_patient_id = attr_text(&manifest.study.attrs, tag::PATIENT_ID).unwrap_or_default();
    if let Err(e) = crate::audit_log::append_record(
        std::path::Path::new(&state.config.btree.audit_dir),
        &AuditEvent::Delete {
            batch_id: batch_id.to_string(),
            user: user.to_string(),
            reason: "derived object removed from a live study".to_string(),
            target: target.sop_instance_uid.clone(),
            ts: crate::audit_log::utc_now_iso(),
            patient_id: owner_patient_id,
            count: 1,
            bytes_to_free: 0,
        },
    ) {
        crate::clog_warn!(
            sop = %target.sop_instance_uid,
            study = %target.study_instance_uid,
            error = %e,
            "cascade could not record the derived-object removal; manifest untouched"
        );
        return Err(format!("audit append failed: {e}"));
    }

    if manifest.series.is_empty() {
        // The last instance of the study is gone, so the manifest goes and
        // becomes a tombstone. This must be the SAME three steps a study
        // deletion performs, because the erase-worker and the RAM index only
        // know about the study through them: a bare rename made the study
        // invisible on disk while QIDO kept serving it from the index, and no
        // `erase_complete` row ever closed the batch.
        //
        // The audit row comes FIRST, before the rename: visible deletion
        // without evidence is the one unacceptable outcome (FR-014).
        let patient_id = attr_text(&manifest.study.attrs, tag::PATIENT_ID).unwrap_or_default();
        if let Err(e) = crate::audit_log::append_record(
            std::path::Path::new(&state.config.btree.audit_dir),
            &AuditEvent::Delete {
                batch_id: batch_id.to_string(),
                user: user.to_string(),
                // The caller's delete named an INSTANCE or a SERIES; this row
                // records that the last one took the study with it.
                reason: "last instance removed".to_string(),
                target: target.study_instance_uid.clone(),
                ts: crate::audit_log::utc_now_iso(),
                patient_id,
                // Zero by construction: this branch is reached only when every
                // series is empty, so the study holds nothing left to count.
                // The deleted object itself is the caller's own audit row.
                count: 0,
                bytes_to_free: 0,
            },
        ) {
            // Do NOT rename: an unrecorded deletion is worse than a failed
            // one. The caller turns this into its own per-object error.
            crate::clog_warn!(
                study = %target.study_instance_uid,
                error = %e,
                "cascade could not record the study tombstone; not renamed"
            );
            return Err(format!("audit append failed: {e}"));
        }
        let _ = clarus_core::manifest_cache::begin_write(&target.study_instance_uid);
        let tombstone = tombstone_path(manifest_dir, &target.study_instance_uid);
        std::fs::rename(&path, &tombstone).map_err(|e| e.to_string())?;
        // Sidecar so the erase-worker closes the loop with
        // `erase_complete {batch_id,...}` for the batch already in the journal.
        let _ = std::fs::write(
            manifest_dir.join(format!("{}.batch", target.study_instance_uid)),
            batch_id.as_bytes(),
        );
        // The study is invisible to readers the instant the rename lands, so
        // the RAM index must forget it in the same breath.
        state
            .study_scanner
            .on_study_erased(&target.study_instance_uid);
        crate::clog_event!(
            crate::clog::Level::Info,
            "STUDY_DELETED",
            "",
            "study tombstoned by last-instance cascade user={} study={} sop={} batch={}",
            user,
            target.study_instance_uid,
            target.sop_instance_uid,
            batch_id
        );
        return Ok(true);
    }
    // Reinstall the entry from the rewritten manifest; without this the cache
    // stays dropped and the next read re-parses for nothing.
    let (cache_v, _taken) = clarus_core::manifest_cache::begin_write(&target.study_instance_uid);
    write_manifest(&path, &manifest).map_err(|e| e.to_string())?;
    clarus_core::manifest_cache::end_write(
        &target.study_instance_uid,
        cache_v,
        std::sync::Arc::new(manifest),
        std::fs::metadata(&path).map(|md| md.len()).unwrap_or(0),
        manifest_dir,
    );
    // E1: hand the orphaned blob to the erase-worker.
    //
    // The comment this replaces claimed the reconciler reclaims it because the
    // manifest no longer references it. It does not: `erase_once`/`gc_orphans`
    // read only the `.del` tombstones and the `.orphans`/`.audited` sidecars,
    // so a derived object dropped from a LIVE study reaches none of them and
    // its CAS file survives forever - removed PHI still on disk, reported by
    // the CAS audit only as unreferenced. Writing the sidecar line is what
    // makes `gc_orphans` see it, and that pass re-checks the refcount before
    // unlinking, so a blob another instance still names stays.
    if let Some((blake3, tier)) = orphan_blob {
        let hex = clarus_adapters::hex_encode(&blake3);
        match crate::handlers::stow_pure::append_sidecar_line(
            manifest_dir,
            &target.study_instance_uid,
            ".orphans",
            &hex,
            tier,
        ) {
            Ok(_) => {}
            Err(e) => {
                // Best effort: the object is already gone from the manifest and
                // failing here would report a removal that did happen as an
                // error. The unreclaimed blob is visible to the CAS audit.
                crate::clog_warn!(
                    sop = %target.sop_instance_uid,
                    error = %e,
                    "could not record the orphaned blob for reclaim"
                );
            }
        }
    }
    crate::clog_info!(
        sop = %target.sop_instance_uid,
        class = %target.name,
        "derived object removed"
    );
    Ok(true)
}

/// A text attribute of an instance by tag.
fn attr_text_of(instance: &clarus_core::manifest::InstanceRecord, wanted: u32) -> Option<String> {
    instance
        .attrs
        .iter()
        .find_map(|(tag, value)| match (tag, value) {
            (t, AttrValue::Text(text)) if *t == wanted => Some(text.clone()),
            _ => None,
        })
}

// -- logs_stream (7b): pure filtering + snapshot + poll -----------------

pub struct LogStreamParams {
    pub level: String,
    pub component: String,
    pub tags: Option<String>,
}

/// Level/component/tag filters for the log ring buffer.
pub struct LogFilter {
    pub level: String,
    pub component: String,
    pub tags: Vec<String>,
}

impl LogFilter {
    /// Build from the query params (defaults applied).
    pub fn from_params(p: &LogStreamParams) -> Self {
        Self {
            level: p.level.to_uppercase(),
            component: p.component.to_lowercase(),
            tags: p
                .tags
                .as_deref()
                .map(|t| t.split(',').map(|s| s.trim().to_lowercase()).collect())
                .unwrap_or_default(),
        }
    }
}

/// Phase 1: buffered entries matching the filter, in order.
pub fn initial_log_snapshot(state: &AppState, f: &LogFilter) -> Vec<LogEntry> {
    state
        .log_buffer
        .read()
        .map(|buf| buf.iter().filter(|e| log_matches(e, f)).cloned().collect())
        .unwrap_or_default()
}

/// Phase 2: new entries after `cursor` (the last emitted ring index).
/// Advances the cursor past them; returns the matching entries.
pub fn poll_logs(state: &AppState, cursor: &mut usize, f: &LogFilter) -> Vec<LogEntry> {
    let mut out = Vec::new();
    if let Ok(buf) = state.log_buffer.read() {
        let current_len = buf.len();
        if current_len > *cursor {
            for entry in buf.iter().skip(*cursor) {
                if log_matches(entry, f) {
                    out.push(entry.clone());
                }
            }
            *cursor = current_len;
        }
    }
    out
}

/// True if `entry` matches the level/component/tag filters.
fn log_matches(entry: &LogEntry, f: &LogFilter) -> bool {
    let levels = ["DEBUG", "INFO", "WARN", "ERROR"];
    let entry_level_idx = levels.iter().position(|&l| l == entry.level).unwrap_or(0);
    let filter_level_idx = levels.iter().position(|&l| l == f.level).unwrap_or(0);
    if entry_level_idx < filter_level_idx {
        return false;
    }
    if f.component != "all" && entry.component.to_lowercase() != f.component {
        return false;
    }
    if !f.tags.is_empty() {
        let entry_tag = entry.tag.as_deref().unwrap_or("").to_lowercase();
        if !f.tags.iter().any(|t| entry_tag.contains(t.as_str())) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use clarus_core::stow::{
        InstanceModel, SeriesModel, StudyManifestAttrs, write_study_manifest_full,
    };

    /// The chronyc CSV parse reads the columns chrony actually prints.
    /// 0 reference ID, 1 source address, 2 stratum, 3 reference time,
    /// 4 system time, 5 last offset.
    ///
    /// The old parser was shifted by one from column 1: stratum came from the
    /// source ADDRESS (`192.168.1.1`, which parses as nothing and fell back to
    /// the `16` "unknown" sentinel), so a synchronised host reported
    /// unsynchronised and the "source" column showed the stratum.
    #[test]
    fn chronyc_tracking_reads_the_documented_columns() {
        // Synced: stratum 2, last offset +0.000021 s.
        let line = "C0A80101,192.168.1.1,2,0000000000000000,0000000000000000,\
                    0.000021234,0.000031234,0.000012345,0.000001234,1.0,0.0,0.0,0.0,0.0";
        let (synced, offset_ms, source) = parse_chronyc_tracking(line).expect("parses");
        assert!(synced, "stratum 2 is synchronised");
        assert!(
            (offset_ms - 0.021234).abs() < 1e-9,
            "offset_ms = {offset_ms}"
        );
        assert_eq!(source, "192.168.1.1", "column 1 is the source address");

        // Stratum 0 means unsynchronised, whatever the other columns say.
        let unsynced = "00000000,,0,0000000000000000,0000000000000000,\
                        0.5,0.0,0.0,0.0,1.0,0.0,0.0,0.0,0.0";
        let (synced, _, _) = parse_chronyc_tracking(unsynced).expect("parses");
        assert!(!synced, "stratum 0 is not synchronised");

        // A field that is not a number must not read as "healthy": the stratum
        // falls back to the unknown sentinel, which is unsynchronised.
        let garbage = "C0A80101,192.168.1.1,not-a-stratum,0,0,\
                       0.1,0.0,0.0,0.0,1.0,0.0,0.0,0.0,0.0";
        let (synced, _, _) = parse_chronyc_tracking(garbage).expect("parses");
        assert!(!synced, "an unparseable stratum is not a sync verdict");

        // Too few columns: refused rather than indexed blindly.
        assert_eq!(parse_chronyc_tracking("C0A80101,192.168.1.1,2"), None);
    }

    /// Concurrent rebuild requests: exactly one wins.
    ///
    /// The claim is one write-lock section, so a thread that observes `idle`
    /// also performs the transition. With the old split (read-lock test, then a
    /// separate write-lock assignment) both threads observed `idle` and both
    /// started a walker.
    #[test]
    fn two_concurrent_rebuild_claims_yield_one_winner() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let state = Arc::new(std::sync::Mutex::new(RebuildState {
            status: "idle".to_string(),
            files_scanned: 0,
            manifests_written: 0,
            instances_indexed: 0,
            skipped: 0,
            errors: 0,
            started_at: None,
            finished_at: None,
            sources: Vec::new(),
            dry_run: true,
        }));
        let winners = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let state = Arc::clone(&state);
            let winners = Arc::clone(&winners);
            handles.push(std::thread::spawn(move || {
                let fresh = RebuildState {
                    status: "running".to_string(),
                    files_scanned: 0,
                    manifests_written: 0,
                    instances_indexed: 0,
                    skipped: 0,
                    errors: 0,
                    started_at: None,
                    finished_at: None,
                    sources: vec!["hot".to_string()],
                    dry_run: true,
                };
                let mut st = state.lock().expect("lock");
                if claim_rebuild(&mut st, fresh) {
                    winners.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        for h in handles {
            h.join().expect("join");
        }
        assert_eq!(
            winners.load(Ordering::SeqCst),
            1,
            "exactly one request may claim the rebuild"
        );
        assert_eq!(state.lock().expect("lock").status, "running");
    }

    /// Write one study with one instance into `dir`, returning the SOP UID.
    fn write_study(
        dir: &std::path::Path,
        study_uid: &str,
        series_uid: &str,
        sop_uid: &str,
        patient_name: &str,
    ) {
        let mut attrs = StudyManifestAttrs {
            patient_name: Some(patient_name.to_string()),
            study_instance_uid: study_uid.to_string(),
            study_date: Some("20260801".to_string()),
            study_description: Some("desc".to_string()),
            modality: "CT".to_string(),
            ..StudyManifestAttrs::default()
        };
        let series = SeriesModel {
            series_instance_uid: series_uid.to_string(),
            modality: "CT".to_string(),
            instances: vec![InstanceModel {
                sop_instance_uid: sop_uid.to_string(),
                transfer_syntax_uid: "1.2.840.10008.1.2.1".to_string(),
                storage_tier: clarus_core::manifest::TIER_HOT,
                file_size: 4242,
                ..InstanceModel::default()
            }],
            ..SeriesModel::default()
        };
        write_study_manifest_full(dir, &mut attrs, &[series]).expect("write manifest");
    }

    /// The scan finds the instance and returns unmasked data for a full admin.
    #[test]
    fn manifest_search_finds_instance() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_study(
            tmp.path(),
            "1.2.3.study",
            "1.2.3.series",
            "1.2.3.sop",
            "Smith^John",
        );
        let resp =
            manifest_instance_search(tmp.path(), "1.2.3.sop", 500, 0, false).expect("search");
        let body = parse_json(&resp.body).expect("json");
        let arr = body.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(
            arr[0].obj_get("sop_instance_uid").and_then(|x| x.as_str()),
            Some("1.2.3.sop")
        );
        assert_eq!(
            arr[0].obj_get("study_uid").and_then(|x| x.as_str()),
            Some("1.2.3.study")
        );
        assert_eq!(
            arr[0].obj_get("series_uid").and_then(|x| x.as_str()),
            Some("1.2.3.series")
        );
        assert_eq!(
            arr[0].obj_get("modality").and_then(|x| x.as_str()),
            Some("CT")
        );
        assert_eq!(
            arr[0].obj_get("patient_name").and_then(|x| x.as_str()),
            Some("Smith^John")
        );
        assert_eq!(arr[0].obj_get("size_bytes"), Some(&JsonVal::U64(4242)));
    }

    /// The auditor role sees masked names and no study description.
    #[test]
    fn manifest_search_masks_pii_for_auditor() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_study(
            tmp.path(),
            "1.2.4.study",
            "1.2.4.series",
            "1.2.4.sop",
            "Smith^John",
        );
        let resp = manifest_instance_search(tmp.path(), "1.2.4.sop", 500, 0, true).expect("search");
        let body = parse_json(&resp.body).expect("json");
        let arr = body.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(
            arr[0].obj_get("patient_name").and_then(|x| x.as_str()),
            Some(mask_name("Smith^John").as_str())
        );
        assert_ne!(
            arr[0].obj_get("patient_name").and_then(|x| x.as_str()),
            Some("Smith^John")
        );
        // The writer renders `None` as JSON null; the auditor never sees the text.
        assert_eq!(arr[0].obj_get("study_description"), Some(&JsonVal::Null));
    }

    /// Offset and limit are applied after the scan (same SOP UID planted in
    /// three studies so the result set is larger than one page).
    #[test]
    fn manifest_search_applies_limit_and_offset() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for i in 0..3 {
            write_study(
                tmp.path(),
                &format!("1.2.5.{i}.study"),
                &format!("1.2.5.{i}.series"),
                "1.2.5.shared.sop",
                "Smith^John",
            );
        }
        let page =
            manifest_instance_search(tmp.path(), "1.2.5.shared.sop", 2, 0, false).expect("search");
        let body = parse_json(&page.body).expect("json");
        let arr = body.as_array().expect("array");
        assert_eq!(arr.len(), 2, "limit clips the page");

        let tail = manifest_instance_search(tmp.path(), "1.2.5.shared.sop", 500, 1, false)
            .expect("search");
        let body = parse_json(&tail.body).expect("json");
        let arr = body.as_array().expect("array");
        assert_eq!(arr.len(), 2, "offset skips the first hit");
    }

    /// The store-wide reference set answers the same question as the per-blob
    /// scan, so a delete resolving many blobs pays for one walk.
    ///
    /// Falsification anchor: the second half asserts a hash NO manifest names
    /// is absent - a set built from a subset (or one that always answers
    /// `true`) would keep every blob and never free disk.
    #[test]
    fn the_live_reference_set_names_every_stored_blob_and_nothing_else() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_study(
            tmp.path(),
            "1.2.6.1.study",
            "1.2.6.1.series",
            "1.2.6.1.sop",
            "Alpha^One",
        );
        write_study(
            tmp.path(),
            "1.2.6.2.study",
            "1.2.6.2.series",
            "1.2.6.2.sop",
            "Beta^Two",
        );

        let set = live_reference_set(tmp.path()).expect("every manifest is readable");
        // Both studies contributed, so no manifest was skipped.
        let stored: Vec<String> = std::fs::read_dir(tmp.path())
            .expect("dir")
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".manifest"))
            .flat_map(|e| read_manifest(&e.path()).expect("manifest").series)
            .flat_map(|s| s.instances)
            .map(|i| clarus_adapters::hex_encode(&i.blake3))
            .collect();
        assert_eq!(stored.len(), 2, "two instances are stored");
        for hash in &stored {
            assert!(
                set.contains(hash),
                "a stored blob is reported as referenced"
            );
        }
        assert!(
            !set.contains(&"f".repeat(64)),
            "an unreferenced hash stays absent, so its blob would be unlinked"
        );
    }

    /// A manifest that cannot be decoded makes the whole set unusable: the
    /// caller must then keep every file, because the skipped study is exactly
    /// the one whose blobs a partial set would mark orphaned.
    #[test]
    fn an_unreadable_manifest_makes_the_reference_set_unavailable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_study(
            tmp.path(),
            "1.2.7.1.study",
            "1.2.7.1.series",
            "1.2.7.1.sop",
            "Gamma^Three",
        );
        std::fs::write(
            tmp.path().join("1.2.7.2.study.manifest"),
            b"\xFF\xFF\xFF\xFF",
        )
        .expect("corrupt manifest");

        assert!(
            live_reference_set(tmp.path()).is_none(),
            "one undecodable manifest must not yield a partial answer"
        );
        assert!(
            blob_is_referenced_live(tmp.path(), &"a".repeat(64)),
            "an unusable set keeps the blob"
        );
    }

    /// `push_f64` is byte-exact with serde_json/ryu across the positional
    /// value range admin endpoints emit (integral floats keep their `.0`).
    /// `push_f64` is byte-exact with serde_json/ryu across the positional
    /// value range admin endpoints emit (integral floats keep their `.0`).
    #[test]
    fn push_f64_matches_serde_render() {
        // Expected byte shapes match serde_json/ryu (integral floats keep `.0`).
        let cases: [(f64, &str); 7] = [
            (0.0, "0.0"),
            (1.0, "1.0"),
            (5.0, "5.0"),
            (42.7, "42.7"),
            (1048576.5, "1048576.5"),
            (0.5, "0.5"),
            (-5.0, "-5.0"),
        ];
        for (v, expected) in cases {
            let mut native = Vec::new();
            push_f64(&mut native, v);
            assert_eq!(String::from_utf8(native).unwrap(), expected, "f64={v}");
        }
    }

    // -- native request-body parsers (byte-parity with serde) -------

    #[test]
    fn parse_maintenance_request_matches_serde() {
        // Full body -> all fields.
        let q =
            parse_maintenance_request(br#"{"enable":true,"message":"Disk full","eta_minutes":15}"#);
        assert!(q.enable);
        assert_eq!(q.message, "Disk full");
        assert_eq!(q.eta_minutes, Some(15));

        // Missing optional fields (serde `#[serde(default)]`) -> defaults.
        let q = parse_maintenance_request(br#"{"enable":false}"#);
        assert!(!q.enable);
        assert_eq!(q.message, "");
        assert_eq!(q.eta_minutes, None);

        // Malformed body -> Default (old `.unwrap_or(default)`).
        let q = parse_maintenance_request(b"not json");
        assert!(!q.enable);
        assert_eq!(q.message, "");
        assert_eq!(q.eta_minutes, None);
    }

    #[test]
    fn parse_migrate_request_matches_serde() {
        let q = parse_migrate_request(
            br#"{"study_uid":"1.2.3","target_tier":"cold","reason":"cleanup"}"#,
        );
        assert_eq!(q.study_uid, "1.2.3");
        assert_eq!(q.target_tier, "cold");
        assert_eq!(q.reason, "cleanup");

        // Missing fields -> default "".
        let q = parse_migrate_request(br#"{"study_uid":"1.2.3"}"#);
        assert_eq!(q.study_uid, "1.2.3");
        assert_eq!(q.target_tier, "");
        assert_eq!(q.reason, "");

        // Unknown keys tolerated (serde ignores unknown fields in these structs).
        let q = parse_migrate_request(br#"{"study_uid":"9","bogus":1}"#);
        assert_eq!(q.study_uid, "9");
    }

    #[test]
    fn parse_delete_request_matches_serde() {
        let q = parse_delete_request(
            br#"{"sop_instance_uids":["1.2.3","4.5.6"],"reason":"purge","dry_run":true}"#,
        )
        .expect("a well-formed body parses");
        assert_eq!(
            q.sop_instance_uids,
            Some(vec!["1.2.3".to_string(), "4.5.6".to_string()])
        );
        assert_eq!(q.reason, Some("purge".to_string()));
        assert!(q.dry_run);
        assert_eq!(q.study_uid, None);
        assert_eq!(q.series_uid, None);

        let q = parse_delete_request(br#"{"study_uid":"1.2.3","reason":"x"}"#)
            .expect("a well-formed body parses");
        assert_eq!(q.study_uid, Some("1.2.3".to_string()));
        assert_eq!(q.sop_instance_uids, None);
        assert!(!q.dry_run);

        // Malformed body -> defaults (old `.unwrap_or(default)`).
        let q = parse_delete_request(b"").expect("an empty body is the default request");
        assert_eq!(q.sop_instance_uids, None);
        assert_eq!(q.study_uid, None);
        assert!(!q.dry_run);
    }

    /// A UID that is not a UID must be refused before it is joined onto a
    /// manifest directory.
    ///
    /// `../index` renamed `<btree>/index.manifest` to `.del`, and a
    /// `sop_owner/<day>` path reached the B-tree index directly. The QIDO
    /// routes already refuse these strings; the admin surface accepted them
    /// because nothing between the JSON body and `manifest_dir.join(..)`
    /// looked at the value.
    #[test]
    fn a_delete_request_with_a_traversing_uid_is_refused() {
        for body in [
            r#"{"study_uid":"../index","reason":"x"}"#,
            r#"{"series_uid":"../index","reason":"x"}"#,
            r#"{"sop_instance_uids":["../../../etc/passwd"],"reason":"x"}"#,
            r#"{"study_uid":"..","reason":"x"}"#,
            r#"{"study_uid":"1.2/../3","reason":"x"}"#,
            r#"{"study_uid":"","reason":"x"}"#,
        ] {
            let err = match parse_delete_request(body.as_bytes()) {
                Ok(_) => panic!("a non-UID must not reach a path join: {body}"),
                Err(e) => e,
            };
            assert_eq!(err, "Invalid DICOM UID", "for {body}");
        }
        // The well-formed neighbours still parse, so the refusal is about the
        // value and not about the request shape.
        assert!(parse_delete_request(br#"{"study_uid":"1.2.840.10008.1","reason":"x"}"#).is_ok());
    }

    /// The restore path takes the same UIDs and must apply the same refusal.
    #[test]
    fn a_restore_request_with_a_traversing_uid_is_refused() {
        assert_eq!(
            parse_restore_request(br#"{"sop_instance_uids":["../../x"],"reason":"y"}"#)
                .expect_err("a non-UID must be refused"),
            "Invalid DICOM UID"
        );
        let (uids, reason) =
            parse_restore_request(br#"{"sop_instance_uids":["1.2.3"],"reason":"y"}"#)
                .expect("a well-formed body parses");
        assert_eq!(uids, Some(vec!["1.2.3".to_string()]));
        assert_eq!(reason, Some("y".to_string()));
    }

    /// A `dry_run` that is not a boolean must be refused, not coerced. The
    /// dangerous direction is `"dry_run": "true"` read as `false`, which turns
    /// a request to rehearse into a request to delete - and cannot be undone.
    #[test]
    fn a_non_boolean_dry_run_is_refused_rather_than_guessed() {
        for body in [
            br#"{"reason":"x","dry_run":1}"#.as_slice(),
            br#"{"reason":"x","dry_run":null}"#.as_slice(),
            br#"{"reason":"x","dry_run":{}}"#.as_slice(),
            br#"{"reason":"x","dry_run":"yes"}"#.as_slice(),
        ] {
            assert!(
                parse_delete_request(body).is_err(),
                "{} must be refused",
                String::from_utf8_lossy(body)
            );
        }
        // The two unambiguous string spellings are read, not refused: a client
        // that sent a quoted boolean meant that boolean.
        for (body, want) in [
            (br#"{"reason":"x","dry_run":"TRUE"}"#.as_slice(), true),
            (br#"{"reason":"x","dry_run":"False"}"#.as_slice(), false),
        ] {
            let q = parse_delete_request(body).expect("a boolean spelling is accepted");
            assert_eq!(q.dry_run, want);
        }
    }

    /// Write one daily access journal file.
    fn write_access_day(dir: &std::path::Path, day: &str, ts: &[&str]) {
        let mut body = String::new();
        for (i, t) in ts.iter().enumerate() {
            body.push_str(&format!(
                "{{\"event\":\"access\",\"ts\":\"{t}\",\"user\":\"u{i}\",\"action\":\"R\",\"sop\":\"{i}\"}}\n"
            ));
        }
        std::fs::write(dir.join(format!("{day}.log")), body).expect("write journal");
    }

    /// Paging over a multi-file journal must return the NEWEST rows, count the
    /// whole filtered set, and let a second page continue where the first
    /// stopped - the property that makes the streamed reader equivalent to the
    /// former "load everything, then page" one.
    #[test]
    fn access_log_pages_newest_first_across_files() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = temp.path();
        write_access_day(
            dir,
            "20260101",
            &["2026-01-01T00:00:01Z", "2026-01-01T00:00:02Z"],
        );
        write_access_day(
            dir,
            "20260102",
            &["2026-01-02T00:00:01Z", "2026-01-02T00:00:02Z"],
        );

        let (total, page1) = access_log_from_files(dir, None, None, None, None, 3, 0);
        assert_eq!(total, 4, "every access line is counted");
        assert_eq!(page1.len(), 3, "the page is capped by limit");
        // Newest file first, newest line first inside it.
        assert_eq!(page1[0].timestamp, "2026-01-02T00:00:02Z");
        assert_eq!(page1[1].timestamp, "2026-01-02T00:00:01Z");
        assert_eq!(page1[2].timestamp, "2026-01-01T00:00:02Z");

        let (total2, page2) = access_log_from_files(dir, None, None, None, None, 3, 3);
        assert_eq!(total2, 4, "the total is independent of the page");
        assert_eq!(page2.len(), 1, "one row remains after the first page");
        assert_eq!(page2[0].timestamp, "2026-01-01T00:00:01Z");

        // A filter narrows both the page and the total.
        let (total3, page3) = access_log_from_files(dir, None, None, Some("u0"), None, 10, 0);
        assert_eq!(total3, 2, "one matching row per day");
        assert_eq!(page3.len(), 2);
        assert_eq!(page3[0].timestamp, "2026-01-02T00:00:01Z");
    }

    /// A truncated tail (a crash mid-flush) is skipped, never an error, and it
    /// does not shift the ordinals of the rows before it.
    #[test]
    fn access_log_skips_a_truncated_tail_line() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = temp.path();
        write_access_day(dir, "20260101", &["2026-01-01T00:00:01Z"]);
        let path = dir.join("20260101.log");
        let mut body = std::fs::read_to_string(&path).expect("read");
        body.push_str("{\"event\":\"access\",\"ts\":\"2026-01-01T00:00:0");
        std::fs::write(&path, body).expect("append partial line");

        let (total, page) = access_log_from_files(dir, None, None, None, None, 10, 0);
        assert_eq!(total, 1, "the partial line is not an entry");
        assert_eq!(page.len(), 1);
    }

    /// R3: the client's `limit` must never size an allocation.
    ///
    /// `?limit=18446744073709551615` reached `Vec::with_capacity` and aborted
    /// the process on a capacity overflow; `?limit=1e9` reserved ~200 GB.
    /// Both requests are legal HTTP and both come from an unauthenticated
    /// query string, so neither may decide how much memory the server takes.
    #[test]
    fn an_absurd_limit_neither_panics_nor_allocates_by_it() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = temp.path();
        std::fs::create_dir_all(dir.join("access")).expect("dir");
        write_access_day(
            &dir.join("access"),
            "20260102",
            &["2026-01-02T00:00:01Z", "2026-01-02T00:00:02Z"],
        );

        // u64::MAX: the request that used to abort.
        let (total, page) =
            access_log_from_files(&dir.join("access"), None, None, None, None, u64::MAX, 0);
        assert_eq!(total, 2, "the rows are still counted");
        assert_eq!(page.len(), 2, "and the page holds what exists, not 2^64");

        // A large-but-parseable limit is clamped to the page cap rather than
        // taken as a reservation size.
        let (_, page) = access_log_from_files(
            &dir.join("access"),
            None,
            None,
            None,
            None,
            1_000_000_000,
            0,
        );
        assert_eq!(page.len(), 2, "a billion-row page is clamped, not reserved");
    }

    /// The same bound on the audit journal reader.
    #[test]
    fn an_absurd_limit_is_bounded_on_the_audit_journal_too() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = temp.path();
        std::fs::create_dir_all(dir.join("audit")).expect("dir");
        std::fs::write(
            dir.join("audit/20260102.log"),
            "{\"event\":\"delete\",\"user\":\"admin\",\"target\":\"1.2.3\"}\n",
        )
        .expect("write");

        let (total, entries) =
            audit_log_from_files(&dir.join("audit"), None, None, None, None, u64::MAX, 0);
        assert_eq!(total, 1);
        assert_eq!(entries.len(), 1);
    }

    /// The cap is a real bound: a request below it is honoured exactly.
    #[test]
    fn a_limit_below_the_cap_is_honoured() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = temp.path();
        std::fs::create_dir_all(dir.join("access")).expect("dir");
        write_access_day(
            &dir.join("access"),
            "20260102",
            &[
                "2026-01-02T00:00:01Z",
                "2026-01-02T00:00:02Z",
                "2026-01-02T00:00:03Z",
            ],
        );
        let (_, page) = access_log_from_files(&dir.join("access"), None, None, None, None, 2, 0);
        assert_eq!(page.len(), 2, "a two-row page is a two-row page");
    }

    /// The total counts EVERY match, including rows in files older than the
    /// one that filled the page. Stopping the scan at a full page made the
    /// admin UI report fewer entries than the journal holds.
    #[test]
    fn access_log_total_counts_files_older_than_a_full_page() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = temp.path().join("access");
        std::fs::create_dir_all(&dir).expect("access dir");
        write_access_day(&dir, "20260101", &["2026-01-01T00:00:01Z"]);
        write_access_day(&dir, "20260102", &["2026-01-02T00:00:01Z"]);
        write_access_day(&dir, "20260103", &["2026-01-03T00:00:01Z"]);

        let (total, page) = access_log_from_files(&dir, None, None, None, None, 1, 0);
        assert_eq!(
            page.len(),
            1,
            "page stays capped by limit: total={total} rows={}",
            page.len()
        );
        assert_eq!(total, 3, "total must cover all three files, not just one");
    }

    /// The rows a full page cannot reach are COUNTED, not retained, and the
    /// count is neither doubled nor lost: the file that fills the page
    /// contributes one match, the older file contributes all three of its own.
    #[test]
    fn access_log_counts_every_match_in_files_a_full_page_skips() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = temp.path().join("access");
        std::fs::create_dir_all(&dir).expect("access dir");
        write_access_day(&dir, "20260101", &["2026-01-01T00:00:01Z"]);
        write_access_day(
            &dir,
            "20260102",
            &[
                "2026-01-02T00:00:01Z",
                "2026-01-02T00:00:02Z",
                "2026-01-02T00:00:03Z",
            ],
        );

        let (total, page) = access_log_from_files(&dir, None, None, None, None, 1, 0);
        assert_eq!(page.len(), 1, "the page is capped at one row");
        assert_eq!(
            total, 4,
            "one row fills the page and the older file's three are still counted"
        );
        // The row on the page is the NEWEST one, i.e. from the newer file.
        assert_eq!(page[0].timestamp, "2026-01-02T00:00:03Z");
    }

    /// S41: a page request must not buffer a whole journal file.
    ///
    /// The per-file retained vector grew with the number of MATCHES, not with
    /// what the page can hold, so a busy day (millions of lines) was read into
    /// memory to answer a page of ten rows. The retained set is now bounded by
    /// the offset window plus the free page slots; this test pins the two
    /// things that bound must not break - the page content and the total.
    #[test]
    fn a_page_is_complete_even_though_the_retained_set_is_bounded() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = temp.path().join("access");
        std::fs::create_dir_all(&dir).expect("access dir");
        // Twenty rows in one day file - well past the three-row page.
        let stamps: Vec<String> = (0..20)
            .map(|i| format!("2026-01-01T00:00:{i:02}Z"))
            .collect();
        let refs: Vec<&str> = stamps.iter().map(String::as_str).collect();
        write_access_day(&dir, "20260101", &refs);

        // Newest-first: the page is the last three of the day.
        let (total, page) = access_log_from_files(&dir, None, None, None, None, 3, 0);
        assert_eq!(total, 20, "every match is still counted");
        let got: Vec<&str> = page.iter().map(|e| e.timestamp.as_str()).collect();
        assert_eq!(
            got,
            vec![
                "2026-01-01T00:00:19Z",
                "2026-01-01T00:00:18Z",
                "2026-01-01T00:00:17Z"
            ],
            "the bounded ring must still yield the NEWEST rows"
        );

        // With an offset the ring must also carry the skipped rows.
        let (total, page) = access_log_from_files(&dir, None, None, None, None, 2, 5);
        assert_eq!(total, 20);
        let got: Vec<&str> = page.iter().map(|e| e.timestamp.as_str()).collect();
        assert_eq!(
            got,
            vec!["2026-01-01T00:00:14Z", "2026-01-01T00:00:13Z"],
            "offset must skip inside the newest-first order, not inside the file"
        );
    }

    /// S50: the pattern matcher implements LIKE, not a substring test.
    ///
    /// `*` was translated to `%` and then TRIMPED off both ends, leaving a
    /// plain `contains` - so any wildcard in the middle of a pattern was
    /// looked for as a literal `*`. The admin search replaced a SQLite LIKE
    /// query, so such a pattern must still match.
    #[test]
    fn a_like_pattern_matches_an_interior_wildcard() {
        // The documented separator is `*`, translated to `%` before matching.
        assert!(like_match("ivanov", "iva%ov"), "interior wildcard");
        assert!(like_match("ivanov", "%ov"), "trailing wildcard");
        assert!(like_match("ivanov", "iva%"), "leading wildcard");
        assert!(like_match("ivanov", "%"), "bare wildcard matches all");
        assert!(like_match("ivanov", "ivanov"), "no wildcard");
        assert!(
            like_match("ivan", "ivan%"),
            "a trailing wildcard may absorb nothing"
        );
        assert!(!like_match("ivanov", "petrov"), "a different name");
        assert!(!like_match("ivanov", "iva%x"), "the tail must match");
        // Two stars on one pattern exercise the backtracking point.
        assert!(like_match("abcdef", "%c%e%"), "several wildcards");
        assert!(
            !like_match("abcdef", "%c%z%"),
            "several wildcards, bad tail"
        );
        // `_` is not a wildcard in this grammar: a name with an underscore
        // must match itself.
        assert!(like_match("a_b", "a_b"));
        assert!(!like_match("axb", "a_b"));
    }

    /// The search that consumes the matcher must find a name through an
    /// interior wildcard, which is the end-user visible half of S50.
    #[test]
    fn the_admin_search_honours_a_wildcard_inside_the_name() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = temp.path().join("manifests");
        std::fs::create_dir_all(&dir).expect("dir");
        write_study(&dir, "1.2.3.500", "1.2.3.500.1", "1.2.3.500.1.1", "Ivanov");

        let hits = manifest_instances_search_general(
            &dir,
            Some("Iva*ov"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            100,
            0,
            false,
        );
        assert_eq!(
            hits.len(),
            1,
            "an interior wildcard must select the study, as the LIKE query did"
        );

        let none = manifest_instances_search_general(
            &dir,
            Some("Iva*xyz"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            100,
            0,
            false,
        );
        assert!(none.is_empty(), "a non-matching pattern selects nothing");
    }
}
