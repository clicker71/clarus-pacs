// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! `cas -> manifests` rebuild worker - restores the server's sight.
//!
//! The integrity-scan half (`cas_audit_worker`) covers
//! this module closes the **rebuild** half. If `manifests/` (plus `idx/` and
//! `ngrams/`) is lost or emptied while CAS is still alive, the server is
//! "blind" - it cannot serve QIDO/WADO because the study manifests are the
//! query engine's source of truth. `rebuild_worker` re-derives the manifests
//! from the stored DICOM blobs on demand (`POST /admin/v1/recovery/rebuild`).
//!
//! ## Design invariants (explicit in the commit / chronical)
//! 1. **Lease** - every manifest write goes through
//!    `stow::write_study_manifest_rebuild`, which internally acquires the
//!    per-study coordinator lease (no read-merge / tombstone races with
//!    concurrent STOW/erase) and ENRICHES an existing manifest (missing
//!    capture tags added, existing values and `arrived_at` preserved).
//! 2. **NEVER deletes CAS blobs** - rebuild is additive only. The CAS blobs
//!    are the source of truth; a rebuild merely re-materialises the manifests.
//! 3. **Idempotent** - a valid existing manifest is merged into (read-merge),
//!    a corrupt one is overwritten, a missing one is created.
//! 4. **Honest tiers** - hot blobs -> `TIER_HOT`, cold -> `TIER_COLD`,
//!    s3 -> `TIER_S3` (from the source, never guessed).
//! 5. **Explicit sources** - the caller passes an explicit, ordered list
//!    (`s3` > `cold` > `hot` priority). A misconfigured source errors loudly
//!    rather than silently dropping a tier's data.
//! 6. **Dedup by SOPInstanceUID** - the first (highest-priority) occurrence
//!    of an instance wins; later duplicates are skipped.
//!
//! ## What rebuild does NOT restore (report must list these)
//! - Server-arrival ordering (`arrived_at` is stamped to rebuild time, not
//!   the original STOW time).
//! - Coercion history embedded in `(0400,0561)` audit copies - standalone
//!   audit-copy fragments (no usable UIDs) are skipped.
//! - Private / derived fields the original STOW carried but that are not part
//!   of the STOW capture-set (`InstanceMeta`).

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use clarus_adapters::dicom_rs::DicomRsParser;
#[cfg(feature = "s3")]
use clarus_core::manifest::TIER_S3;
use clarus_core::manifest::{TIER_COLD, TIER_HOT};
use clarus_ports::dicom::{DicomParser, InstanceMeta};

use crate::handlers::admin::pure::RebuildState;
use crate::state::AppState;

/// Upper bound for the DICOM header read (mirrors STOW's `max_part_header`).
/// One million bytes is enough for all identity/meta tags while never loading
/// a full pixel buffer in the recovery path.
const MAX_HEADER: usize = 1 << 20;

/// Run one rebuild pass over the requested sources.
///
/// `sources` is the explicit, priority-ordered list (`s3` > `cold` > `hot`)
/// produced by `admin_recovery_rebuild`. `dry_run` scans + reports but writes
/// nothing. `state_lock` is the shared `REBUILD_STATE` (production: the
/// process-global; tests: a local lock). Blobs are never moved or deleted.
pub fn run_rebuild(
    state: &AppState,
    sources: &[String],
    dry_run: bool,
    state_lock: &RwLock<RebuildState>,
) {
    let mut acc = RebuildAcc::default();
    let mut seen_sop: HashSet<String> = HashSet::new();
    let mut seen_study: HashSet<String> = HashSet::new();

    let manifest_dir = PathBuf::from(&state.config.btree.manifest_dir);
    let idx_dir = PathBuf::from(&state.config.btree.idx_dir);
    let parser = DicomRsParser;

    for src in sources {
        match src.as_str() {
            "hot" => {
                acc.errors += walk_fs(
                    &parser,
                    state.cas.root_dir(),
                    TIER_HOT,
                    dry_run,
                    &manifest_dir,
                    &idx_dir,
                    state,
                    &mut seen_sop,
                    &mut seen_study,
                    &mut acc,
                );
            }
            "cold" => {
                if let Some(cold) = &state.cold_cas {
                    acc.errors += walk_fs(
                        &parser,
                        cold.root_dir(),
                        TIER_COLD,
                        dry_run,
                        &manifest_dir,
                        &idx_dir,
                        state,
                        &mut seen_sop,
                        &mut seen_study,
                        &mut acc,
                    );
                } else {
                    // Pure-side validation should have rejected this before we
                    // are ever spawned; keep the guard for defence in depth.
                    acc.errors += 1;
                    crate::clog_error!("rebuild: cold source requested but not configured");
                }
            }
            "s3" => {
                #[cfg(feature = "s3")]
                {
                    acc.errors += walk_s3(
                        &parser,
                        state,
                        dry_run,
                        &manifest_dir,
                        &idx_dir,
                        &mut seen_sop,
                        &mut seen_study,
                        &mut acc,
                    );
                }
                #[cfg(not(feature = "s3"))]
                {
                    acc.errors += 1;
                    crate::clog_error!(
                        "rebuild: s3 source requested but this build has no s3 feature"
                    );
                }
            }
            other => {
                acc.errors += 1;
                crate::clog_warn!(source = %other, "rebuild: unknown source name");
            }
        }
    }

    // A real (non-dry) rebuild that finished WITHOUT errors is the cure for the
    // degraded signal: manifests are now present, so clear the flags. A pass
    // that skipped studies for write failures has NOT restored anything, and
    // clearing then would hide a still-degraded store behind a green gauge.
    if !dry_run && acc.errors == 0 {
        crate::metrics::MANIFEST_MISSING.store(0, std::sync::atomic::Ordering::Relaxed);
        crate::metrics::MANIFEST_CORRUPT.store(0, std::sync::atomic::Ordering::Relaxed);
        crate::metrics::MANIFEST_DEGRADED.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    let (status, finished) = (
        if acc.errors > 0 {
            format!("error: {} error(s) during rebuild", acc.errors)
        } else {
            "done".to_string()
        },
        crate::handlers::admin::pure::chrono_now(),
    );
    {
        let mut st = state_lock.write().expect("Rebuild state lock poisoned");
        st.status = status;
        st.files_scanned = acc.files_scanned;
        st.manifests_written = acc.manifests_written;
        st.instances_indexed = acc.instances_indexed;
        st.skipped = acc.skipped;
        st.errors = acc.errors;
        st.finished_at = Some(finished);
    }

    crate::clog_info!(
        source_list = %sources.join(","),
        dry_run = dry_run,
        scanned = acc.files_scanned,
        studies = acc.manifests_written,
        instances = acc.instances_indexed,
        skipped = acc.skipped,
        errors = acc.errors,
        "rebuild complete"
    );
}

/// Running counters for one rebuild pass (written to `RebuildState` at end).
#[derive(Default)]
struct RebuildAcc {
    files_scanned: u64,
    manifests_written: u64,
    instances_indexed: u64,
    skipped: u64,
    errors: u64,
}

/// Recursively walk a file-system CAS root (`{root}/{xx}/{yy}/{hash}.dcm` plus
/// any legacy single-level buckets), rebuild each valid instance. Returns the
/// number of hard errors encountered.
#[allow(clippy::too_many_arguments)]
fn walk_fs(
    parser: &DicomRsParser,
    root: &Path,
    tier: u8,
    dry_run: bool,
    manifest_dir: &Path,
    idx_dir: &Path,
    state: &AppState,
    seen_sop: &mut HashSet<String>,
    seen_study: &mut HashSet<String>,
    acc: &mut RebuildAcc,
) -> u64 {
    let mut errors = 0u64;
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                errors += 1;
                crate::clog_warn!(
                    dir = %dir.display(),
                    error = %e,
                    "rebuild: cannot read source dir"
                );
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let md = match std::fs::metadata(&path) {
                Ok(md) => md,
                Err(_) => continue,
            };
            if md.is_dir() {
                dirs.push(path);
            } else if md.is_file() && is_dcm_blob(&path) {
                acc.files_scanned += 1;
                process_blob(
                    parser,
                    &path,
                    tier,
                    md.len(),
                    dry_run,
                    manifest_dir,
                    idx_dir,
                    state,
                    seen_sop,
                    seen_study,
                    acc,
                );
            }
        }
    }
    errors
}

/// Walk the S3 source (LIST-by-prefix + GET each object), rebuild each valid
/// instance. Returns the number of hard errors. `s3_key()` produces the
/// unified `{prefix}{xx}/{yy}/{hash}.dcm` key.
#[cfg(feature = "s3")]
#[allow(clippy::too_many_arguments)]
fn walk_s3(
    parser: &DicomRsParser,
    state: &AppState,
    dry_run: bool,
    manifest_dir: &Path,
    idx_dir: &Path,
    seen_sop: &mut HashSet<String>,
    seen_study: &mut HashSet<String>,
    acc: &mut RebuildAcc,
) -> u64 {
    use clarus_adapters::storage::s3::ObjectStore;
    let Some(s3) = state.s3.get() else {
        crate::clog_error!("rebuild: S3 not connected");
        return 1;
    };
    // List the whole bucket (no prefix) - the unified geometry is
    // `{prefix}{xx}/{yy}/{hash}.dcm`; we filter `.dcm` suffixes below, so the
    // configured `prefix` needs no special-casing here.
    let objects = match s3.list_objects("", None) {
        Ok(o) => o,
        Err(e) => {
            crate::clog_warn!(error = %e, "rebuild: S3 LIST failed");
            return 1;
        }
    };
    let mut errors = 0u64;
    for obj in objects {
        if !obj.key.ends_with(".dcm") {
            continue;
        }
        let data = match s3.get_object(&obj.key) {
            Ok(d) => d,
            Err(e) => {
                errors += 1;
                crate::clog_warn!(key = %obj.key, error = %e, "rebuild: S3 GET failed");
                continue;
            }
        };
        acc.files_scanned += 1;
        let blake3 = match key_blake3(&obj.key) {
            Some(b) => b,
            None => {
                acc.skipped += 1;
                crate::clog_warn!(key = %obj.key, "rebuild: object key has no BLAKE3 segment; skipped");
                continue;
            }
        };
        process_bytes(
            parser,
            &obj.key,
            data.as_ref(),
            obj.size,
            TIER_S3,
            blake3,
            dry_run,
            manifest_dir,
            idx_dir,
            state,
            seen_sop,
            seen_study,
            acc,
        );
    }
    errors
}

/// Parse a BLAKE3 hex segment out of an S3 object key (last `/{hash}.dcm`
/// path element - the unified `{prefix}{xx}/{yy}/{hash}.dcm` geometry).
#[cfg(feature = "s3")]
fn key_blake3(key: &str) -> Option<[u8; 32]> {
    let stem = key.rsplit('/').next()?.strip_suffix(".dcm")?;
    if stem.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in stem.as_bytes().chunks(2).enumerate() {
        out[i] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Some(out)
}

/// Process one on-disk `.dcm` blob: extract metadata, dedup by SOP UID,
/// then (if not a dry run) write/merge the study manifest + ngrams + RAM index.
#[allow(clippy::too_many_arguments)]
fn process_blob(
    parser: &DicomRsParser,
    path: &Path,
    tier: u8,
    file_size: u64,
    dry_run: bool,
    manifest_dir: &Path,
    idx_dir: &Path,
    state: &AppState,
    seen_sop: &mut HashSet<String>,
    seen_study: &mut HashSet<String>,
    acc: &mut RebuildAcc,
) {
    // Bounded header read (mirrors STOW `read_dicom_header`): enough for all
    // non-PixelData metadata without loading a full pixel buffer.
    let mut header = vec![0u8; 0];
    if let Ok(mut f) = std::fs::File::open(path) {
        let len = MAX_HEADER.min(file_size as usize);
        header.resize(len, 0);
        let _ = f.read_exact(&mut header);
    }
    let blake3 = match blob_blake3(path) {
        Some(b) => b,
        None => {
            acc.skipped += 1;
            crate::clog_warn!(path = %path.display(), "rebuild: blob has no BLAKE3 filename; skipped");
            return;
        }
    };
    process_bytes(
        parser,
        path_str(path),
        &header,
        file_size,
        tier,
        blake3,
        dry_run,
        manifest_dir,
        idx_dir,
        state,
        seen_sop,
        seen_study,
        acc,
    );
}

/// The byte-level core shared by the fs and s3 paths. `blob_key` is used only
/// for logging (the fs path passes the path; the s3 path passes the object key).
#[allow(clippy::too_many_arguments)]
fn process_bytes(
    parser: &DicomRsParser,
    blob_key: &str,
    data: &[u8],
    file_size: u64,
    tier: u8,
    blake3: [u8; 32],
    dry_run: bool,
    manifest_dir: &Path,
    idx_dir: &Path,
    state: &AppState,
    seen_sop: &mut HashSet<String>,
    seen_study: &mut HashSet<String>,
    acc: &mut RebuildAcc,
) {
    let meta = match parser.extract_metadata(data) {
        Ok(m) => m,
        Err(e) => {
            acc.skipped += 1;
            crate::clog_warn!(blob = blob_key, error = %e, "rebuild: undecodable blob; skipped");
            return;
        }
    };

    // Skip blobs with missing required UIDs - including standalone
    // (0400,0561) audit-copy fragments that carry no usable identity.
    if meta.sop_instance_uid.is_empty()
        || meta.study_instance_uid.is_empty()
        || meta.series_instance_uid.is_empty()
    {
        acc.skipped += 1;
        crate::clog_warn!(
            blob = blob_key,
            sop = %meta.sop_instance_uid,
            "rebuild: blob missing required UIDs (possibly an audit copy); skipped"
        );
        return;
    }

    // Dedup by SOPInstanceUID: the first (highest-priority) occurrence wins.
    if seen_sop.contains(&meta.sop_instance_uid) {
        acc.skipped += 1;
        return;
    }

    let transfer_syntax_uid = meta
        .transfer_syntax_uid
        .clone()
        .unwrap_or_else(|| "1.2.840.10008.1.2.1".to_string());

    // `seen_sop` and the counters advance only once the instance is actually
    // on disk. Recording the SOP UID first made a FAILED write of a
    // high-priority copy shadow the same instance in a lower-priority source
    // that could have written it successfully, and counted instances the
    // store never received.
    if !dry_run
        && !commit_one_instance(
            &meta,
            blob_key,
            data,
            blake3,
            file_size,
            tier,
            &transfer_syntax_uid,
            manifest_dir,
            idx_dir,
            state,
        )
    {
        acc.errors += 1;
        return;
    }
    seen_sop.insert(meta.sop_instance_uid.clone());

    // Count this study on first sight.
    if seen_study.insert(meta.study_instance_uid.clone()) {
        acc.manifests_written += 1;
    }
    acc.instances_indexed += 1;
}

/// Write (or merge) a single instance into its study manifest + ngrams, and
/// reflect it in the RAM study index. Mirrors the STOW commit path
/// (`stow_pure.rs` `InsertResult::Inserted` arm).
/// `write_study_manifest_rebuild` acquires the per-study lease,
/// round-trip-validates the write, and ENRICHES an existing manifest with
/// missing capture tags (existing values and `arrived_at` preserved).
#[allow(clippy::too_many_arguments)]
fn commit_one_instance(
    meta: &InstanceMeta,
    blob_key: &str,
    dicom_bytes: &[u8],
    blake3: [u8; 32],
    file_size: u64,
    tier: u8,
    transfer_syntax_uid: &str,
    manifest_dir: &Path,
    idx_dir: &Path,
    state: &AppState,
) -> bool {
    // blake3 is derived from the CAS filename; the rebuild worker never
    // recomputes it (the filename IS the blake3 address).
    let mut study_attrs = clarus_core::stow::StudyManifestAttrs {
        patient_name: Some(meta.patient_name.clone()),
        patient_id: Some(meta.patient_id.clone()),
        patient_id_issuer: meta.patient_id_issuer.clone(),
        patient_birth_date: meta.patient_birth_date.clone(),
        patient_sex: meta.patient_sex.clone(),
        study_instance_uid: meta.study_instance_uid.clone(),
        study_date: meta.study_date.clone(),
        study_time: meta.study_time.clone(),
        study_description: meta.study_description.clone(),
        accession_number: meta.accession_number.clone(),
        referring_physician: meta.referring_physician.clone(),
        performing_physician: meta.performing_physician.clone(),
        name_of_reading_physicians: meta.name_of_reading_physicians.clone(),
        institution_name: meta.institution_name.clone(),
        requesting_physician: meta.requesting_physician.clone(),
        requested_procedure_description: meta.requested_procedure_description.clone(),
        responsible_person: meta.responsible_person.clone(),
        other_patient_ids: (!meta.other_patient_ids.is_empty())
            .then(|| meta.other_patient_ids.join(",")),
        modality: meta.modality.clone(),
        study_id: meta.study_id.clone(),
        num_series: 0,
        num_instances: 0,
        modalities_in_study: None,
    };
    // Series instance UID for the instance -> series manifest. When a normal
    // instance is a re-write referencing a series, use its own UID.
    let series_instance_uid = meta.series_instance_uid.clone();
    let instance = clarus_core::stow::InstanceModel {
        sop_instance_uid: meta.sop_instance_uid.clone(),
        sop_class_uid: meta.sop_class_uid.clone(),
        instance_number: meta.instance_number.map(|n| n.to_string()),
        blake3,
        storage_tier: tier,
        transfer_syntax_uid: transfer_syntax_uid.to_string(),
        file_size,
        referenced_series_uids: meta.referenced_series_uids.clone(),
        referenced_sop_instance_uids: meta.referenced_sop_instance_uids.clone(),
        // Rebuild re-derives the frame table from the stored blob so an
        // instance written before the table existed gains it on rebuild,
        // exactly like the other re-derivable capture fields.
        frames: crate::handlers::stow_pure::frame_records(dicom_bytes),
    };
    let series = [clarus_core::stow::SeriesModel {
        series_instance_uid,
        modality: meta.modality.clone(),
        series_number: meta.series_number.map(|n| n.to_string()),
        series_date: meta.series_date.clone(),
        series_time: meta.series_time.clone(),
        pps_start_date: meta.pps_start_date.clone(),
        pps_start_time: meta.pps_start_time.clone(),
        scheduled_procedure_step_id: meta.scheduled_procedure_step_id.clone(),
        requested_procedure_id: meta.requested_procedure_id.clone(),
        station_name: meta.station_name.clone(),
        manufacturer: meta.manufacturer.clone(),
        institutional_department_name: meta.institutional_department_name.clone(),
        operators_name: meta.operators_name.clone(),
        series_description: meta.series_description.clone(),
        body_part_examined: meta.body_part_examined.clone(),
        protocol_name: meta.protocol_name.clone(),
        laterality: meta.laterality.clone(),
        instances: vec![instance],
    }];
    if let Err(e) =
        clarus_core::stow::write_study_manifest_rebuild(manifest_dir, &mut study_attrs, &series)
    {
        crate::clog_warn!(
            error = %e,
            blob = blob_key,
            study = %meta.study_instance_uid,
            "rebuild: study manifest write failed; study skipped"
        );
        return false;
    }
    // reflect the (re)built study in the RAM query index.
    state
        .study_scanner
        .on_study_committed(&meta.study_instance_uid, &study_attrs);
    // ngrams for fuzzymatch QIDO (patient + referring physician), best-effort,
    // into today's bucket (or the study date if present).
    let day: String = match meta.study_date.as_deref() {
        Some(d) if d.len() == 8 => d.to_string(),
        _ => today_yyyymmdd(),
    };
    clarus_core::stow::index_study_names(idx_dir, &study_attrs, &meta.study_instance_uid, &day);
    true
}

/// Lowercase-hex decode of a 64-char BLAKE3 filename stem into `[u8; 32]`.
fn blob_blake3(path: &Path) -> Option<[u8; 32]> {
    let stem = path.file_stem()?.to_str()?;
    if stem.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in stem.as_bytes().chunks(2).enumerate() {
        out[i] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn is_dcm_blob(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "dcm")
}

fn path_str(p: &Path) -> &str {
    p.to_str().unwrap_or("<non-utf8>")
}

/// Current UTC date as `YYYYMMDD`.
fn today_yyyymmdd() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}{m:02}{d:02}")
}

/// Howard Hinnant's `civil_from_days` - converts a days-since-epoch count to a
/// `(year, month, day)` civil date (Gregorian). Pure integer, no chrono.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_nibble_accepts_all_cases() {
        assert_eq!(hex_nibble(b'0'), Some(0));
        assert_eq!(hex_nibble(b'9'), Some(9));
        assert_eq!(hex_nibble(b'a'), Some(10));
        assert_eq!(hex_nibble(b'f'), Some(15));
        assert_eq!(hex_nibble(b'A'), Some(10));
        assert_eq!(hex_nibble(b'F'), Some(15));
        assert_eq!(hex_nibble(b'g'), None);
        assert_eq!(hex_nibble(b':'), None);
    }

    #[test]
    fn blob_blake3_decodes_64_hex_filestem() {
        // 32 zero bytes -> 64 '0' chars.
        let zeros = [0u8; 32];
        let hex = clarus_adapters::hex_encode(&zeros);
        let p = std::path::Path::new("xx/yy").join(format!("{hex}.dcm"));
        assert_eq!(blob_blake3(&p), Some(zeros));

        // Short stem -> None (not a blake3 address).
        let bad = std::path::Path::new("abcd.dcm");
        assert_eq!(blob_blake3(bad), None);
        // Non-hex stem -> None.
        let bad2 = std::path::Path::new("zzzz").join("x".repeat(64));
        assert!(blob_blake3(&bad2).is_none());
    }

    #[test]
    fn cas_filenames_roundtrip_blake3() {
        let digest = [7u8; 32];
        let hex = clarus_adapters::hex_encode(&digest);
        let p = std::path::PathBuf::from("aa/bb").join(format!("{hex}.dcm"));
        assert_eq!(blob_blake3(&p), Some(digest));
        // And the reverse: hex encoding a decoded digest reproduces the stem.
        assert_eq!(clarus_adapters::hex_encode(&blob_blake3(&p).unwrap()), hex);
    }

    #[cfg(feature = "s3")]
    #[test]
    fn key_blake3_parses_unified_s3_key() {
        let digest = [0xabu8; 32];
        let hex = clarus_adapters::hex_encode(&digest);
        // Unified S3 key = prefix + canonical cas_rel_path (forward slashes).
        let key = format!("store/{}", clarus_adapters::storage::cas_rel_path(&hex));
        assert_eq!(key_blake3(&key), Some(digest));
        assert_eq!(key_blake3("store/other/file.dcm"), None);
        assert_eq!(key_blake3("store/"), None);
    }

    #[test]
    fn is_dcm_blob_matches_suffix() {
        let dcm = std::path::PathBuf::from("a.dcm");
        assert!(is_dcm_blob(&dcm));
        assert!(!is_dcm_blob(&std::path::PathBuf::from("a.dcm.bak")));
        assert!(!is_dcm_blob(&std::path::PathBuf::from("a.txt")));
    }

    #[test]
    fn civil_from_days_known_dates() {
        // 1970-01-01 == day 0.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2024-01-01: days from epoch (a known constant).
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // 2000-02-29 is a leap day (day 11016); the day after is 2000-03-01.
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
    }

    #[test]
    fn today_yyyymmdd_is_8_digits() {
        let t = today_yyyymmdd();
        assert_eq!(t.len(), 8);
        assert!(t.bytes().all(|b| b.is_ascii_digit()));
    }
}
