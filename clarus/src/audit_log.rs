// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Append-only JSON audit trail.
//!
//! THE ONLY EVIDENCE OF DELETIONS - back up this directory.
//!
//! Every destructive admin action appends one JSON line (plus `fsync`) to
//! `audit/YYYYMMDD.log`, rotated by UTC calendar date. This is the FSTEC
//! export surface, so JSON remains the single place the codebase uses it as
//! a wire format (the deletion evidence must survive the SQLite excision and
//! the erase-worker, which would otherwise destroy the in-database trail).
//!
//! # Contract
//! - Order on the admin thread: **audit-append -> tombstone rename** -> 200 ->
//!   DbActor -> worker. The visible deletion MUST NOT precede the audit row
//!   (FR-014); the reverse (audit row without rename) is acceptable.
//! - `delete` row carries `{ts, batch_id, user, reason, target, count,
//!   bytes_to_free}` - hashes are NOT written (the auditor does not need
//!   them; they remain readable in the `.del` until erasure).
//! - `dry_run=true` writes a `delete_preview` row - preview requests are
//!   otherwise invisible to the auditor.
//! - The erase-worker appends a matching `erase_complete {batch_id, files,
//!   bytes}` row, closing the loop so the audit survives erasure.
//! - `fsync` on every write: this is the admin path, not a hotpath.
//!
//! # Permissions
//! - Unix: files `0600`; the DIRECTORY is enforced `0700` at startup and
//!   re-checked on every append (self-fix + WARN; startup aborts if the
//!   chmod itself fails). A permissions anomaly on the "only evidence"
//!   dir is itself evidence and is recorded in the trail
//!   (`security_perm_fix` rows).
//! - Windows: the code deliberately does nothing - `0o600`/chmod do not
//!   exist there. The equivalent is an ACL set ONCE by the OS admin on the
//!   directory (inheritance covers future rotation):
//!   `icacls <audit_dir> /inheritance:r /grant:r "SYSTEM:(OI)(CI)F"
//!   "BUILTIN\\Administrators:(OI)(CI)F" "<service_account>:(OI)(CI)F"`
//!   (documented in `clarus.conf`). Encryption at rest is the OS-level mount
//!   encryption (BitLocker/LUKS/EFS).
//! - The trail is NOT tamper-proof: it lives on the same filesystem as the
//!   data. Actor attribution and tamper protection are OS-level controls
//!   (auditd, chattr +a, off-box backup) - docs/ops recipes, not code.

use crate::handlers::json_common::{push_bool, push_json_str_loose, push_u64};

use std::io::Write;
use std::path::{Path, PathBuf};

/// One audit event. Serialized as a single JSON line, then a newline.
#[derive(Debug, Clone)]
pub enum AuditEvent {
    /// An admin initiated a delete. Written BEFORE the tombstone rename.
    Delete {
        batch_id: String,
        user: String,
        reason: String,
        /// Study/series/instance target UID.
        target: String,
        /// UTC ISO-8601 second resolution `YYYY-MM-DDTHH:MM:SSZ`
        /// (GOST 27789 sec. 7 EventDateTime (M)).
        ts: String,
        /// PatientID of the owning study, captured from the study manifest
        /// BEFORE the tombstone rename (FR-014 order preserved).
        /// Missing/empty -> "" (never invented; coerced studies carry the
        /// honest AUTOPAT_ generated ID). Names are never written.
        patient_id: String,
        /// Number of instances affected.
        count: u64,
        /// Sum of `InstanceRecord.file_size` for files to be freed - the
        /// `bytes_to_free` preview, from the manifest, no FS access.
        bytes_to_free: u64,
    },
    /// A dry-run preview of a delete. Nothing was or will be erased.
    DeletePreview {
        batch_id: String,
        user: String,
        reason: String,
        target: String,
        /// UTC ISO-8601 second resolution `YYYY-MM-DDTHH:MM:SSZ`.
        ts: String,
        /// PatientID of the owning study (identifier; names never written).
        patient_id: String,
        count: u64,
        bytes_to_free: u64,
    },
    /// The erase-worker physically erased the files of a tombstoned study.
    EraseComplete {
        batch_id: String,
        /// Number of files removed (blake3 + legacy copies).
        files: u64,
        /// Bytes freed (sum of removed file sizes).
        bytes: u64,
    },
    /// An administrator cleared a rejection sequester, making the named
    /// instances visible again. Written BEFORE the manifest mutation.
    ///
    /// This is the record of a REVERSED clinical decision, so it carries the
    /// reason code being cleared: an auditor reading the trail must be able
    /// to tell which kind of hide was undone (a reviewable Quality rejection
    /// or a patient-safety one) without reconstructing it from the KOS note.
    /// The note itself is never modified - it remains the record of why the
    /// hide happened in the first place.
    Restore {
        batch_id: String,
        user: String,
        /// The operator's mandatory free-text justification.
        reason: String,
        /// The rejection reason code being CLEARED (113001/113037/113038/
        /// 113039) - i.e. which clinical decision this restore reverses.
        reason_code: u32,
        /// SOP Instance UID of the restored instance.
        target: String,
        /// UTC ISO-8601 second resolution `YYYY-MM-DDTHH:MM:SSZ`.
        ts: String,
        /// PatientID of the owning study (identifier; names never written).
        patient_id: String,
        /// Instances in this row (1 for a per-instance row).
        count: u64,
    },
    /// One IOCM rejection note sequestered instances in one study. Written
    /// BEFORE `mark_rejected` mutates the manifest.
    ///
    /// A hide is a clinical sequester - it removes diagnostic images from
    /// every ordinary search and retrieve and has no standard reversal - so it
    /// belongs in the same immutable trail as a delete, and its mirror image
    /// here is [`AuditEvent::Restore`].
    ///
    /// One row per (study, reason) group rather than per instance: a KOS may
    /// name thousands of instances, and the referenced UIDs remain queryable
    /// through the still-stored note. `count` is therefore the number of
    /// instances the note NAMED in that study - the request's intent - not the
    /// number `mark_rejected` actually flipped, which is already carried by
    /// `clarus_rejected_instances_total`.
    Reject {
        user: String,
        /// The rejection reason code applied (113037/113038/113039).
        reason_code: u32,
        /// StudyInstanceUID whose instances were sequestered.
        study: String,
        /// SOP Instance UID of the KOS note that carried the rejection, when
        /// the batch recorded one. Empty -> the field is omitted rather than
        /// written as a misleading empty string; the note is still findable by
        /// querying the reason code.
        note_uid: String,
        /// Instances the note named in this study.
        count: u64,
        /// UTC ISO-8601 second resolution `YYYY-MM-DDTHH:MM:SSZ`.
        ts: String,
    },
    /// A permissions anomaly on the audit directory was observed (and the
    /// self-fix attempted). The trail records its own protection events -
    /// a perms anomaly on the "only evidence" dir is itself evidence.
    SecurityPermFix {
        /// Audit directory path at observation time.
        dir: String,
        /// Observed mode at the moment of the anomaly (`& 0o777`).
        mode_before: u32,
        /// Whether the `chmod 0700` self-fix succeeded.
        fixed: bool,
        /// Observation point: `"startup"` or `"append"`.
        source: &'static str,
    },
}

/// Append one audit event as a JSON line + newline to today's log, then fsync.
///
/// Rotation is by UTC calendar date: each call derives `YYYYMMDD` from
/// [`std::time::SystemTime`] and appends to `audit_dir/YYYYMMDD.log`,
/// creating it (with `0600` on Unix) if absent. `fsync` follows every write so
/// the evidence is durable before the caller mutates visibility.
///
/// Before the write the directory permissions are observed and, on Unix,
/// self-fixed to `0700` with the anomaly recorded in the trail itself
/// (best-effort: a failed fix WARNs but never fails the record).
pub fn append_record(audit_dir: &Path, event: &AuditEvent) -> std::io::Result<()> {
    let dir = normalize_dir(audit_dir);
    std::fs::create_dir_all(&dir)?;
    if let Err(e) = observe_and_fix_dir_permissions(&dir, "append") {
        // A failed re-chmod must never fail the audit record itself.
        crate::clog_warn!("{}", e);
    }
    append_event_line(&dir, event)
}

/// Raw append of one rendered event line + fsync (no permission logic).
fn append_event_line(dir: &Path, event: &AuditEvent) -> std::io::Result<()> {
    // Byte-exact with the legacy serde-tagged render (see tests), then EOL.
    let mut line = Vec::new();
    write_audit_event(&mut line, event);
    line.push(b'\n');

    let path = today_log_path(dir);
    // Append-only: never truncate. 0600 on Unix; nothing on Windows (ACL).
    let mut file = new_append(&path)?;
    file.write_all(&line)?;
    file.sync_all()?;
    Ok(())
}

/// Append `e` as a JSON object (byte-exact with the legacy
/// `serde_json::to_string` serde-tagged-enum render):
/// `{"event":<snake_case_variant>,<fields in declaration order>}`.
fn write_audit_event(out: &mut Vec<u8>, e: &AuditEvent) {
    match e {
        AuditEvent::Delete {
            batch_id,
            user,
            reason,
            target,
            ts,
            patient_id,
            count,
            bytes_to_free,
        } => {
            out.extend_from_slice(b"{\"event\":\"delete\"");
            write_field_str(out, "batch_id", batch_id);
            write_field_str(out, "user", user);
            write_field_str(out, "reason", reason);
            write_field_str(out, "target", target);
            write_field_str(out, "ts", ts);
            write_field_str(out, "patient_id", patient_id);
            write_field_u64(out, "count", *count);
            write_field_u64(out, "bytes_to_free", *bytes_to_free);
        }
        AuditEvent::DeletePreview {
            batch_id,
            user,
            reason,
            target,
            ts,
            patient_id,
            count,
            bytes_to_free,
        } => {
            out.extend_from_slice(b"{\"event\":\"delete_preview\"");
            write_field_str(out, "batch_id", batch_id);
            write_field_str(out, "user", user);
            write_field_str(out, "reason", reason);
            write_field_str(out, "target", target);
            write_field_str(out, "ts", ts);
            write_field_str(out, "patient_id", patient_id);
            write_field_u64(out, "count", *count);
            write_field_u64(out, "bytes_to_free", *bytes_to_free);
        }
        AuditEvent::EraseComplete {
            batch_id,
            files,
            bytes,
        } => {
            out.extend_from_slice(b"{\"event\":\"erase_complete\"");
            write_field_str(out, "batch_id", batch_id);
            write_field_u64(out, "files", *files);
            write_field_u64(out, "bytes", *bytes);
        }
        AuditEvent::Restore {
            batch_id,
            user,
            reason,
            reason_code,
            target,
            ts,
            patient_id,
            count,
        } => {
            out.extend_from_slice(b"{\"event\":\"restore\"");
            write_field_str(out, "batch_id", batch_id);
            write_field_str(out, "user", user);
            write_field_str(out, "reason", reason);
            write_field_u64(out, "reason_code", u64::from(*reason_code));
            write_field_str(out, "target", target);
            write_field_str(out, "ts", ts);
            write_field_str(out, "patient_id", patient_id);
            write_field_u64(out, "count", *count);
        }
        AuditEvent::Reject {
            user,
            reason_code,
            study,
            note_uid,
            count,
            ts,
        } => {
            out.extend_from_slice(b"{\"event\":\"reject\"");
            write_field_str(out, "user", user);
            write_field_u64(out, "reason_code", u64::from(*reason_code));
            write_field_str(out, "study", study);
            if !note_uid.is_empty() {
                write_field_str(out, "note_uid", note_uid);
            }
            write_field_u64(out, "count", *count);
            write_field_str(out, "ts", ts);
        }
        AuditEvent::SecurityPermFix {
            dir,
            mode_before,
            fixed,
            source,
        } => {
            out.extend_from_slice(b"{\"event\":\"security_perm_fix\"");
            write_field_str(out, "dir", dir);
            write_field_u64(out, "mode_before", *mode_before as u64);
            write_field_bool(out, "fixed", *fixed);
            write_field_str(out, "source", source);
        }
    }
    out.push(b'}');
}

/// Append `,"<name>":<str>`.
fn write_field_str(out: &mut Vec<u8>, name: &str, v: &str) {
    out.extend_from_slice(b",\"");
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b"\":");
    push_json_str_loose(out, v);
}

/// Append `,"<name>":<u64>`.
fn write_field_u64(out: &mut Vec<u8>, name: &str, v: u64) {
    out.extend_from_slice(b",\"");
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b"\":");
    push_u64(out, v);
}

/// Append `,"<name>":<bool>`.
fn write_field_bool(out: &mut Vec<u8>, name: &str, v: bool) {
    out.extend_from_slice(b",\"");
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b"\":");
    push_bool(out, v);
}

/// Append-only open with Unix `0600` (Windows: opened plain - ACL is OS-set).
fn new_append(path: &Path) -> std::io::Result<std::fs::File> {
    // Default may be 0666 masked by umask; force 0600 best-effort on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut opt = std::fs::OpenOptions::new();
        opt.create(true).append(true).mode(0o600);
        opt.open(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    }
}

/// Best-effort `0700` on the audit directory (Unix only).
///
/// Observes the mode; on anomaly self-fixes to `0700` and records a
/// `SecurityPermFix` row in the trail itself (best-effort) BEFORE the caller
/// sees the result - so a failed fix (e.g. read-only mount) is preserved as
/// evidence even when startup then aborts. Returns `Ok(true)` when an
/// anomaly was fixed, `Ok(false)` when permissions were already owner-only.
fn observe_and_fix_dir_permissions(dir: &Path, source: &'static str) -> Result<bool, String> {
    #[cfg(target_family = "unix")]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(dir)
            .map_err(|e| format!("Cannot stat audit dir {}: {}", dir.display(), e))?;
        if !meta.is_dir() {
            return Err(format!(
                "Audit path {} is not a directory; refusing to chmod a file",
                dir.display()
            ));
        }
        let mode = meta.permissions().mode();
        if mode & 0o077 == 0 {
            return Ok(false); // already owner-only: nothing to fix or record
        }
        let chmod_res = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        let fixed = chmod_res.is_ok();
        let ev = AuditEvent::SecurityPermFix {
            dir: dir.display().to_string(),
            mode_before: mode & 0o777,
            fixed,
            source,
        };
        // Best-effort: a failed record write must never fail the perm fix,
        // the append, or startup.
        if let Err(rec_err) = append_event_line(dir, &ev) {
            crate::clog_warn!(
                "SECURITY: perm-fix record write failed for {}: {rec_err}",
                dir.display()
            );
        }
        match chmod_res {
            Ok(()) => {
                crate::clog_warn!(
                    "SECURITY: {} had insecure permissions {:o}; fixed to 700.",
                    dir.display(),
                    mode & 0o777
                );
                Ok(true)
            }
            Err(e) => Err(format!(
                "SECURITY: {} has insecure permissions {:o} and chmod 700 failed: {e}. \
                 Run: chmod 700 {}",
                dir.display(),
                mode & 0o777,
                dir.display()
            )),
        }
    }
    #[cfg(not(target_family = "unix"))]
    {
        let _ = (dir, source);
        Ok(false) // Windows: no observable mode, nothing to fix or record
    }
}

/// Startup enforcement: the audit directory must be owner-only (`0700` on
/// Unix) BEFORE the server serves traffic.
///
/// Unix: `create_dir_all`, then observe + self-fix (anomalies recorded in
/// the trail); `Err` only when a non-directory occupies the path or the
/// chmod itself fails (startup aborts - same pattern as the config-file
/// check). Windows: `create_dir_all` only, then a warn-only skip (NTFS ACL
/// model - the asymmetry is intentional).
pub fn ensure_audit_dir_permissions(dir: &Path) -> Result<(), String> {
    #[cfg(target_family = "unix")]
    {
        std::fs::create_dir_all(dir).map_err(|e| {
            if dir.exists() && !dir.is_dir() {
                format!(
                    "Audit path {} exists and is NOT a directory (refusing to chmod a file)",
                    dir.display()
                )
            } else {
                format!("Cannot create audit dir {}: {}", dir.display(), e)
            }
        })?;
        match observe_and_fix_dir_permissions(dir, "startup") {
            Ok(false) => {
                let mode = std::fs::metadata(dir)
                    .map(|m| {
                        use std::os::unix::fs::PermissionsExt;
                        m.permissions().mode() & 0o777
                    })
                    .unwrap_or(0);
                crate::clog_info!("Audit dir permissions OK: {:o}", mode);
                Ok(())
            }
            Ok(true) => Ok(()), // anomaly fixed: WARN + record already emitted
            Err(e) => Err(e),
        }
    }
    #[cfg(not(target_family = "unix"))]
    {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("Cannot create audit dir {}: {}", dir.display(), e))?;
        crate::clog_warn!(
            "Audit dir permission check skipped on this platform. Ensure {} is readable only by the service account (one-time icacls, see clarus.conf).",
            dir.display()
        );
        Ok(())
    }
}

/// Directory for log writes (shared/owned reference plumbing kept minimal).
fn normalize_dir(dir: &Path) -> PathBuf {
    dir.to_path_buf()
}

/// `audit_dir/YYYYMMDD.log` for the current UTC date.
fn today_log_path(dir: &Path) -> PathBuf {
    let ymd = utc_ymd();
    dir.join(format!("{ymd}.log"))
}

/// UTC civil date `YYYYMMDD` from the current `SystemTime`.
pub(crate) fn utc_ymd() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}{month:02}{day:02}")
}

/// Convert days since Unix epoch to (year, month, day) in the civil calendar
/// (Howard Hinnant's `civil_from_days` - no date crate needed, KISS).
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// UTC ISO-8601 second resolution `YYYY-MM-DDTHH:MM:SSZ` (GOST 27789 sec. 7
/// EventDateTime) - built from SystemTime via the civil_from_days path plus
/// HH:MM:SS. Day-granularity (the YYYYMMDD filename) is too weak for a
/// deletion reconstruction; the delete row carries the second.
pub(crate) fn utc_now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let tod = secs % 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_json_line_with_fsync() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The date-prefix assertion follows the event's OWN ts - a hardcoded
        // calendar date breaks the suite at midnight.
        let ev_ts = utc_now_iso();
        let ev = AuditEvent::Delete {
            batch_id: "b1".into(),
            user: "admin".into(),
            reason: "test".into(),
            target: "1.2.3".into(),
            ts: ev_ts.clone(),
            patient_id: "PAT-007".into(),
            count: 2,
            bytes_to_free: 4096,
        };
        append_record(dir.path(), &ev).expect("append");
        append_record(dir.path(), &ev).expect("append second");

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1, "single date file");
        let file_name = entries[0].file_name().to_string_lossy().into_owned();
        assert!(
            file_name.ends_with(".log") && file_name.len() == 12,
            "YYYYMMDD.log, got {file_name}"
        );

        let content = std::fs::read_to_string(dir.path().join(&file_name)).expect("read");
        let lines: Vec<&str> = content.lines().collect();
        // On Unix CI runners tempdirs come up 0777 & umask (0755), so the
        // first append self-fixes the dir and may prepend a
        // security_perm_fix row. The delete lines are the subject here.
        let delete_lines: Vec<&str> = lines
            .iter()
            .filter(|l| l.contains("\"event\":\"delete\""))
            .cloned()
            .collect();
        assert_eq!(delete_lines.len(), 2);
        assert!(delete_lines[0].contains("\"batch_id\":\"b1\""));
        assert!(delete_lines[0].contains("\"bytes_to_free\":4096"));
        assert!(delete_lines[0].contains("\"patient_id\":\"PAT-007\""));
        assert!(
            delete_lines[0].contains(&format!("\"ts\":\"{}", &ev_ts[..10])),
            "ts must carry the event's own UTC date, got {ev_ts}"
        );
        // The ts is a real second-resolution civil UTC stamp.
        let ts_part = delete_lines[0].split("\"ts\":\"").nth(1).expect("ts field");
        let ts = ts_part.split('"').next().expect("ts value");
        assert_eq!(ts.len(), 20, "YYYY-MM-DDTHH:MM:SSZ, got {ts}");
        assert!(ts.ends_with('Z'), "UTC Z suffix, got {ts}");
        assert!(ts.as_bytes()[4] == b'-' && ts.as_bytes()[7] == b'-');
    }

    /// The native `write_audit_event` is byte-exact with the legacy serde
    /// tagged-enum render (captured 19.08) for all variants.
    #[test]
    fn audit_event_json_matches_legacy_render() {
        let cases: [(AuditEvent, &str); 4] = [
            (
                AuditEvent::Delete {
                    batch_id: "b1".into(),
                    user: "admin".into(),
                    reason: "test".into(),
                    target: "1.2.3".into(),
                    ts: "2026-08-28T09:12:03Z".into(),
                    patient_id: "PAT-007".into(),
                    count: 2,
                    bytes_to_free: 4096,
                },
                "{\"event\":\"delete\",\"batch_id\":\"b1\",\"user\":\"admin\",\"reason\":\"test\",\"target\":\"1.2.3\",\"ts\":\"2026-08-28T09:12:03Z\",\"patient_id\":\"PAT-007\",\"count\":2,\"bytes_to_free\":4096}",
            ),
            (
                AuditEvent::DeletePreview {
                    batch_id: "b2".into(),
                    user: "auditor".into(),
                    reason: "preview".into(),
                    target: "1.2.4".into(),
                    ts: "2026-08-28T09:12:04Z".into(),
                    patient_id: "PAT-008".into(),
                    count: 3,
                    bytes_to_free: 500,
                },
                "{\"event\":\"delete_preview\",\"batch_id\":\"b2\",\"user\":\"auditor\",\"reason\":\"preview\",\"target\":\"1.2.4\",\"ts\":\"2026-08-28T09:12:04Z\",\"patient_id\":\"PAT-008\",\"count\":3,\"bytes_to_free\":500}",
            ),
            (
                AuditEvent::EraseComplete {
                    batch_id: "b3".into(),
                    files: 7,
                    bytes: 9000,
                },
                "{\"event\":\"erase_complete\",\"batch_id\":\"b3\",\"files\":7,\"bytes\":9000}",
            ),
            (
                AuditEvent::SecurityPermFix {
                    dir: "/srv/clarus/data/audit".into(),
                    mode_before: 0o755,
                    fixed: true,
                    source: "startup",
                },
                "{\"event\":\"security_perm_fix\",\"dir\":\"/srv/clarus/data/audit\",\"mode_before\":493,\"fixed\":true,\"source\":\"startup\"}",
            ),
        ];
        for (ev, want) in cases {
            let mut out = Vec::new();
            write_audit_event(&mut out, &ev);
            assert_eq!(String::from_utf8(out).unwrap(), want);
        }
    }

    #[test]
    fn civil_date_is_sane() {
        // Unix epoch day 0 -> 1970-01-01.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // A known recent date: 2026-08-14.
        let secs_now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let (y, m, d) = civil_from_days(secs_now / 86_400);
        assert!(y >= 2026, "year not in past, got {y}");
        assert!((1..=12).contains(&m));
        assert!((1..=31).contains(&d));
    }

    /// Directory-permission semantics are Unix-only (chmod); the Windows
    /// behavior (create-only + warn-only skip) is covered by the manual
    /// sanity check in the report.
    #[cfg(unix)]
    mod perms {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn mode_of(p: &std::path::Path) -> u32 {
            std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777
        }

        fn sample_delete() -> AuditEvent {
            AuditEvent::Delete {
                batch_id: "b-perm".into(),
                user: "admin".into(),
                reason: "perm-test".into(),
                target: "1.2.3".into(),
                ts: utc_now_iso(),
                patient_id: "PAT-007".into(),
                count: 1,
                bytes_to_free: 0,
            }
        }

        fn today_content(dir: &std::path::Path) -> String {
            std::fs::read_to_string(today_log_path(dir)).expect("read today log")
        }

        #[test]
        fn ensure_fixes_preexisting_0755_dir() {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
                .expect("chmod 755");
            assert_eq!(mode_of(dir.path()), 0o755);
            ensure_audit_dir_permissions(dir.path()).expect("ensure");
            assert_eq!(mode_of(dir.path()), 0o700, "dir forced to 0700");
            let content = today_content(dir.path());
            assert!(
                content.contains("\"event\":\"security_perm_fix\""),
                "anomaly recorded, got: {content}"
            );
            assert!(content.contains("\"mode_before\":493"), "0o755 = 493");
            assert!(content.contains("\"fixed\":true"));
            assert!(content.contains("\"source\":\"startup\""));
        }

        #[test]
        fn ensure_creates_missing_dir_with_0700() {
            let base = tempfile::tempdir().expect("tempdir");
            let audit = base.path().join("audit");
            ensure_audit_dir_permissions(&audit).expect("ensure");
            assert!(audit.is_dir());
            assert_eq!(mode_of(&audit), 0o700, "created dir is 0700");
        }

        #[test]
        fn ensure_rejects_non_directory_path() {
            let base = tempfile::tempdir().expect("tempdir");
            let occupied = base.path().join("occupied");
            std::fs::write(&occupied, b"x").expect("write file");
            let err = ensure_audit_dir_permissions(&occupied).unwrap_err();
            let lower = err.to_lowercase();
            assert!(
                lower.contains("not a directory"),
                "clear refusal message, got: {err}"
            );
        }

        #[test]
        fn append_records_perm_fix_on_anomaly() {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
                .expect("chmod 755");
            append_record(dir.path(), &sample_delete()).expect("append");
            assert_eq!(mode_of(dir.path()), 0o700, "dir fixed on append path");
            let content = today_content(dir.path());
            assert!(content.contains("\"event\":\"security_perm_fix\""));
            assert!(content.contains("\"source\":\"append\""));
            assert!(content.contains("\"fixed\":true"));
            assert!(content.contains("\"event\":\"delete\""));
            // The fix row precedes the deletion row (evidence of the
            // pre-write state lands first).
            let fix_pos = content.find("security_perm_fix").expect("fix row");
            let del_pos = content.find("\"event\":\"delete\"").expect("delete row");
            assert!(fix_pos < del_pos);
        }

        #[test]
        fn append_no_perm_record_when_0700() {
            // tempfile creates dirs 0777 & umask (0755 on CI runners); make
            // the pre-state explicitly owner-only.
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                .expect("chmod 700");
            assert_eq!(mode_of(dir.path()), 0o700);
            append_record(dir.path(), &sample_delete()).expect("append");
            let content = today_content(dir.path());
            assert!(
                !content.contains("security_perm_fix"),
                "no anomaly row when already 0700, got: {content}"
            );
            assert!(content.contains("\"event\":\"delete\""));
        }
    }
}
