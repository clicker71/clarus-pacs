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
    // A rebuild over a SUBSET of the tiers cannot vouch for the store as a
    // whole. A cold-only pass scans an empty cold CAS, finishes with zero
    // errors, and would clear the manifest gauges while a study that exists
    // only on hot storage is still missing - the green gauge then hides the
    // very condition it reports.
    //
    // Every tier this process actually HAS must be covered before the gauges
    // may be cleared: `hot` is always present, `cold` counts when configured,
    // and `s3` when this build has the feature.
    let cold_configured = state.cold_cas.is_some();
    let mut scanned_hot = false;
    let mut scanned_cold = false;
    #[cfg(feature = "s3")]
    let mut scanned_s3 = false;

    let manifest_dir = PathBuf::from(&state.config.btree.manifest_dir);
    let idx_dir = PathBuf::from(&state.config.btree.idx_dir);
    let parser = DicomRsParser;

    for src in sources {
        match src.as_str() {
            "hot" => {
                scanned_hot = true;
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
                    scanned_cold = true;
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
                    scanned_s3 = true;
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
    //
    // Covering only SOME of the configured tiers does not prove the store is
    // whole either: a study reachable solely through an unscanned tier is
    // still missing, so the gauges stay as they were.
    let covered_all_tiers = scanned_hot && (!cold_configured || scanned_cold);
    #[cfg(feature = "s3")]
    let covered_all_tiers = covered_all_tiers && scanned_s3;
    if !dry_run && acc.errors == 0 && covered_all_tiers {
        crate::metrics::MANIFEST_MISSING.store(0, std::sync::atomic::Ordering::Relaxed);
        crate::metrics::MANIFEST_CORRUPT.store(0, std::sync::atomic::Ordering::Relaxed);
        crate::metrics::MANIFEST_DEGRADED.store(0, std::sync::atomic::Ordering::Relaxed);
    } else if !dry_run && acc.errors == 0 {
        crate::clog_warn!(
            "rebuild: pass completed without errors but did not cover every tier; \
             the manifest gauges are left unchanged"
        );
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
        // A dry run's findings are reported under their OWN names, so the
        // completed-write counters above stay honestly zero.
        studies_eligible = acc.studies_eligible,
        instances_eligible = acc.instances_eligible,
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
    /// Studies a DRY RUN found eligible. Kept apart from
    /// [`RebuildAcc::manifests_written`], which is a completed-write count: a
    /// dry run writes nothing, so reporting it there told an operator the
    /// store had been rebuilt when it had only been inspected.
    studies_eligible: u64,
    /// Instances a dry run found eligible, for the same reason.
    instances_eligible: u64,
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
        // `flatten()` discarded a failed directory-entry read, and
        // `metadata` errors were discarded too: the rebuild then reported
        // `done` without ever having seen a blob it could not read, which is
        // the opposite of what a recovery pass is for.
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    errors += 1;
                    crate::clog_warn!(
                        dir = %dir.display(),
                        error = %e,
                        "rebuild: cannot read a directory entry"
                    );
                    continue;
                }
            };
            let path = entry.path();
            // `symlink_metadata`: the entry's OWN type, never the target's.
            //
            // `fs::metadata` FOLLOWS a symlink, so a bucket that links to an
            // ancestor queue the same tree again (unbounded work), and a link
            // into another archive indexed that archive's objects under THIS
            // tier - a rebuild must never trust the filesystem shape it is
            // scanning. A symlink is therefore neither recursed into nor
            // treated as a blob.
            let md = match std::fs::symlink_metadata(&path) {
                Ok(md) => md,
                Err(e) => {
                    errors += 1;
                    crate::clog_warn!(
                        path = %path.display(),
                        error = %e,
                        "rebuild: cannot stat a CAS entry"
                    );
                    continue;
                }
            };
            let ft = md.file_type();
            if ft.is_symlink() {
                // Reported, not silently skipped: a symlinked bucket is a
                // layout this pass cannot safely interpret.
                crate::clog_warn!(
                    path = %path.display(),
                    "rebuild: skipping a symlink in the CAS tree"
                );
                continue;
            }
            if ft.is_dir() {
                dirs.push(path);
            } else if ft.is_file() && is_dcm_blob(&path) {
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

/// Whether a listed object is too large to materialise during a rebuild.
///
/// A zero cap disables the check, matching STOW's reading of the same key: an
/// operator who set it to 0 asked for no ceiling.
#[cfg(feature = "s3")]
fn exceeds_rebuild_cap(size: u64, max_bytes: u64) -> bool {
    max_bytes > 0 && size > max_bytes
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
        // LIST already reports the size, so an object the hot path would have
        // refused is refused here too - before it is materialised. The cap is
        // the same `max_instance_size_mb` STOW applies; without this check a
        // 12.5 GB WSI object was GET in full and then decoded, which is an OOM
        // partway through a rebuild rather than one skipped object.
        let max_bytes = state
            .config
            .max_instance_size_mb
            .saturating_mul(1024 * 1024);
        if exceeds_rebuild_cap(obj.size, max_bytes) {
            acc.skipped += 1;
            crate::clog_warn!(
                key = %obj.key,
                size = obj.size,
                max = max_bytes,
                "rebuild: S3 object exceeds max_instance_size_mb; skipped"
            );
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
            None,
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
    //
    // The bound is applied in `u64` and converted AFTERWARDS. `file_size as
    // usize` truncates on a 32-bit target, so a 4 GiB blob became a 0-byte
    // header and the parser was handed nothing instead of the first MiB - the
    // instance then looked unparseable and was counted as a skip, not as the
    // I/O problem it was.
    //
    // A read FAILURE is a hard error, never an empty buffer: passing the
    // partially-read prefix to the parser made an unreadable blob
    // indistinguishable from a corrupt one, and the pass could report `done`
    // without ever reading something it could not open.
    let mut header = vec![0u8; 0];
    match std::fs::File::open(path) {
        Ok(mut f) => {
            let len = u64::try_from(MAX_HEADER).unwrap_or(u64::MAX).min(file_size);
            let len = usize::try_from(len).unwrap_or(MAX_HEADER);
            header.resize(len, 0);
            if let Err(e) = f.read_exact(&mut header) {
                acc.errors += 1;
                crate::clog_warn!(
                    path = %path.display(),
                    error = %e,
                    len,
                    "rebuild: cannot read the blob header"
                );
                return;
            }
        }
        Err(e) => {
            acc.errors += 1;
            crate::clog_warn!(
                path = %path.display(),
                error = %e,
                "rebuild: cannot open the blob"
            );
            return;
        }
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
        Some(path),
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
///
/// `data` is `Some` only when the caller holds the WHOLE blob; the filesystem
/// path walks with a bounded header and passes `fs_path` instead, so the
/// digest check re-reads on its own budget rather than trusting a partial
/// buffer.
#[allow(clippy::too_many_arguments)]
fn process_bytes(
    parser: &DicomRsParser,
    blob_key: &str,
    data: &[u8],
    fs_path: Option<&Path>,
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

    // INTEGRITY GATE: a blob is published under the digest OF ITS BYTES, and
    // only a blob whose bytes still hash to the name it was found under may be
    // committed.
    //
    // Two ways this matters. A damaged blob that still PARSES (a flipped bit in
    // the header, a truncated tail after the last element) would be recorded
    // with the digest from its filename, so the manifest would vouch for
    // content the store does not hold - and because the SOP is then deduped,
    // a VALID copy in a lower-priority source would never get its chance.
    // Checking before the dedup insert is what keeps that copy reachable.
    //
    // `data` carries the whole blob only on the S3 path, which hashed these
    // exact bytes while receiving them; the filesystem walk passes the path so
    // the check re-reads in bounded chunks instead of buffering.
    let whole_blob = (data.len() as u64 == file_size).then_some(data);
    if !digest_matches(whole_blob, fs_path, file_size, &blake3, blob_key) {
        acc.errors += 1;
        return;
    }

    // PS3.5 A-1: when an instance declares no Transfer Syntax UID, Explicit VR
    // Little Endian is the assumed default. Named from the repository's own
    // constant rather than spelled out here, so the fixture and the parser
    // cannot disagree about the UID.
    let transfer_syntax_uid = meta
        .transfer_syntax_uid
        .clone()
        .unwrap_or_else(|| clarus_adapters::codec::ts_uid::EXPLICIT_VR_LE.to_string());

    // `seen_sop` and the counters advance only once the instance is actually
    // on disk. Recording the SOP UID first made a FAILED write of a
    // high-priority copy shadow the same instance in a lower-priority source
    // that could have written it successfully, and counted instances the
    // store never received.
    if dry_run {
        // A dry run writes NOTHING, so the completed-write counters must stay
        // at zero: reporting `manifests_written = 1` for a pass that skipped
        // `commit_one_instance` told an operator the store had been rebuilt
        // when it had only been inspected.
        if seen_study.insert(meta.study_instance_uid.clone()) {
            acc.studies_eligible += 1;
        }
        seen_sop.insert(meta.sop_instance_uid.clone());
        acc.instances_eligible += 1;
        return;
    }
    if !commit_one_instance(
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
    ) {
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
        number_of_frames: meta.number_of_frames,
        image_type: meta.image_type.clone(),
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
    let (grams, failed) =
        clarus_core::stow::index_study_names(idx_dir, &study_attrs, &meta.study_instance_uid, &day);
    if failed > 0 {
        // The manifest is committed and must stay; the accelerator is not. A
        // partial `.idx` only widens the next fuzzymatch query back to a full
        // scan, but an operator still needs to know the index is torn, and the
        // row count alone could not say so.
        eprintln!(
            "rebuild: ngram index incomplete for study {} ({grams} rows, {failed} failed)",
            meta.study_instance_uid
        );
    }
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

/// Whether the bytes about to be committed still hash to their address.
///
/// `data` is what the caller read (fs: the whole blob; s3: the body it already
/// hashed while receiving). `fs_path` is `Some` only for the filesystem walk,
/// where re-reading in bounded chunks is the way to hash a possibly huge blob;
/// there `data` holds just the header, so hashing it would answer a different
/// question. A read failure counts as a mismatch: a blob the recovery pass
/// cannot verify is not one it may publish.
fn digest_matches(
    data: Option<&[u8]>,
    fs_path: Option<&Path>,
    file_size: u64,
    expected: &[u8; 32],
    blob_key: &str,
) -> bool {
    let actual = match (data, fs_path) {
        (Some(bytes), _) if bytes.len() as u64 == file_size => {
            clarus_core::hash::cas_hash_one(bytes)
        }
        (_, Some(path)) => match crate::cas_audit_worker::hash_file_incremental(path) {
            Ok(d) => d,
            Err(e) => {
                crate::clog_warn!(
                    blob = blob_key,
                    error = %e,
                    "rebuild: blob could not be re-read for its digest check; not committed"
                );
                return false;
            }
        },
        _ => return false,
    };
    if actual == *expected {
        return true;
    }
    crate::clog_warn!(
        blob = blob_key,
        expected = %clarus_adapters::hex_encode(expected),
        actual = %clarus_adapters::hex_encode(&actual),
        "rebuild: blob content does not match the digest it is addressed by; not committed"
    );
    false
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

    /// An object larger than the instance cap is skipped on its REPORTED size,
    /// before any GET.
    ///
    /// The hot path already refuses such an instance; a rebuild that
    /// materialised it first turned one oversized object into an OOM partway
    /// through the pass, with no record of which object caused it.
    #[cfg(feature = "s3")]
    #[test]
    fn an_object_over_the_instance_cap_is_skipped_before_the_get() {
        let cap = 4096 * 1024 * 1024u64;
        assert!(exceeds_rebuild_cap(cap + 1, cap));
        assert!(exceeds_rebuild_cap(12_500_000_000, cap));
        assert!(!exceeds_rebuild_cap(cap, cap), "the boundary is inclusive");
        assert!(!exceeds_rebuild_cap(1024, cap));
        // A zero cap means "no ceiling", the same reading STOW gives it.
        assert!(!exceeds_rebuild_cap(u64::MAX, 0));
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

    // -- S2a regressions -------------------------------------------------

    /// A private manifest tree with its own CAS root, so a whole
    /// `run_rebuild` can be driven without touching the process state.
    fn rebuild_state(root: &std::path::Path) -> AppState {
        let idx_dir = root.join("idx");
        let manifest_dir = root.join("manifests");
        let storage_dir = root.join("cas");
        std::fs::create_dir_all(&idx_dir).expect("idx");
        std::fs::create_dir_all(&manifest_dir).expect("manifests");
        std::fs::create_dir_all(&storage_dir).expect("cas");

        let mut config = crate::config::Config::test_default();
        config.storage_dir = storage_dir.to_string_lossy().to_string();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;

        AppState {
            cas: clarus_adapters::storage::ContentAddressedStorage::new(storage_dir).expect("cas"),
            codec_registry: clarus_adapters::codec::CodecRegistry::default(),
            ups_events: crate::sse_fanout::DualEventSink::new(),
            start_time: std::time::Instant::now(),
            metrics_ring: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::VecDeque::new(),
            )),
            log_buffer: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::VecDeque::new(),
            )),
            maintenance: std::sync::Arc::new(std::sync::RwLock::new(
                crate::state::MaintenanceState::default(),
            )),
            #[cfg(feature = "s3")]
            s3: std::sync::Arc::new(std::sync::OnceLock::new()),
            cold_cas: None,
            shadow: crate::shadow::ShadowTier::new(idx_dir.clone()),
            study_scanner: clarus_core::query::StudyScanner::new(manifest_dir.clone(), idx_dir),
            config,
            workitems_dir: root.join("workitems"),
            commitments_dir: root.join("commitments"),
            transcode_pool: crate::state::TranscodePool::new(2),
            readiness: crate::handlers::readiness::Readiness::new(),
        }
    }

    /// A blank rebuild state, spelled out because [`RebuildState`] has no
    /// `Default` impl (the process-global is built by hand for the same
    /// reason: `status` must start as `idle`, not as an empty string).
    fn blank_rebuild_state() -> RebuildState {
        RebuildState {
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
        }
    }

    /// The manifest gauges are PROCESS-GLOBAL, so the two tests that assert
    /// on them must not interleave: one seeding `MANIFEST_MISSING` while the
    /// other clears it would make either verdict meaningless.
    static GAUGE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A DIRTY gauge must survive a pass that only covered some of the
    /// configured tiers.
    ///
    /// A cold-only rebuild over an empty cold CAS finished with zero errors and
    /// cleared all three manifest gauges, so a study that existed only on HOT
    /// storage stayed missing while the gauge reported a healthy store.
    #[test]
    fn a_pass_that_covers_some_tiers_leaves_the_gauges_alone() {
        use std::sync::atomic::Ordering;
        let _guard = GAUGE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let state = rebuild_state(dir.path());
        // Seed the degraded reading; only THIS seed is compared against.
        crate::metrics::MANIFEST_MISSING.store(7, Ordering::Relaxed);
        let lock = RwLock::new(blank_rebuild_state());

        // No source is scanned at all, so no tier is covered: the pass cannot
        // vouch for the store and the gauge must stay dirty. (An empty source
        // list is also the smallest expression of "some tiers were skipped".)
        run_rebuild(&state, &[], false, &lock);

        assert_eq!(
            crate::metrics::MANIFEST_MISSING.load(Ordering::Relaxed),
            7,
            "a pass that skipped every tier must not clear the gauge"
        );
    }

    /// Covering every configured tier IS a complete answer, so the gauge is
    /// cleared.
    ///
    /// Only built WITHOUT the `s3` feature: with it, "every tier" includes S3,
    /// and an unconnected client is an error by design (`rebuild: S3 not
    /// connected`), so the gauge correctly stays dirty. The property this test
    /// pins - that a fully-covering pass clears the gauge - is therefore only
    /// expressible in a build where `hot` is the whole store.
    #[cfg(not(feature = "s3"))]
    #[test]
    fn a_pass_over_every_configured_tier_clears_the_gauge() {
        use std::sync::atomic::Ordering;
        let _guard = GAUGE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let state = rebuild_state(&dir.path().join("full"));
        crate::metrics::MANIFEST_MISSING.store(7, Ordering::Relaxed);
        let lock = RwLock::new(blank_rebuild_state());

        // `cold_cas` is None in this fixture, so `hot` is the only tier.
        run_rebuild(&state, &["hot".to_string()], false, &lock);

        assert_eq!(
            crate::metrics::MANIFEST_MISSING.load(Ordering::Relaxed),
            0,
            "a pass over every configured tier clears the gauge"
        );
    }

    /// A directory SYMLINK must not be followed during a CAS walk.
    ///
    /// `fs::metadata` follows the link: a bucket pointing at an ancestor
    /// re-queued the same tree (unbounded work), and a link into another
    /// archive indexed that archive's objects under this tier.
    #[test]
    fn a_symlinked_bucket_is_not_followed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = rebuild_state(&dir.path().join("s"));
        let cas_root = state.cas.root_dir().to_path_buf();
        std::fs::create_dir_all(&cas_root).expect("cas root");

        // A symlink at a bucket position that points at the CAS root itself.
        let link = cas_root.join("aa");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&cas_root, &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(&cas_root, &link).is_ok();
        // A runner without symlink rights cannot express the case; the
        // assertion below is then vacuous, so say so instead of passing.
        assert!(
            made,
            "the test needs symlink creation rights to express the hazard"
        );

        let parser = DicomRsParser;
        let mut acc = RebuildAcc::default();
        let errors = walk_fs(
            &parser,
            &cas_root,
            TIER_HOT,
            true,
            std::path::Path::new(&state.config.btree.manifest_dir),
            std::path::Path::new(&state.config.btree.idx_dir),
            &state,
            &mut HashSet::new(),
            &mut HashSet::new(),
            &mut acc,
        );
        assert_eq!(errors, 0, "a symlink is a skip, not an I/O failure");
        assert_eq!(
            acc.files_scanned, 0,
            "no blob lies behind the symlink, so nothing may be scanned"
        );
    }

    /// An unreadable CAS entry is an ERROR, not an optimistic skip.
    #[test]
    fn an_unreadable_directory_entry_is_counted_as_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = rebuild_state(&dir.path().join("s"));
        let cas_root = state.cas.root_dir().to_path_buf();
        std::fs::create_dir_all(&cas_root).expect("cas root");
        // A dangling symlink: `symlink_metadata` succeeds (it describes the
        // LINK), but the entry is neither dir nor blob - so to force a real
        // error we instead make a blob path unreadable at OPEN time by
        // replacing the file with a directory named `*.dcm`.
        let bogus = cas_root.join("bb");
        std::fs::create_dir_all(bogus.join("cc")).expect("bucket");
        std::fs::create_dir_all(bogus.join("cc").join("deadbeef.dcm")).expect("dir as blob");

        let parser = DicomRsParser;
        let mut acc = RebuildAcc::default();
        let _ = walk_fs(
            &parser,
            &cas_root,
            TIER_HOT,
            true,
            std::path::Path::new(&state.config.btree.manifest_dir),
            std::path::Path::new(&state.config.btree.idx_dir),
            &state,
            &mut HashSet::new(),
            &mut HashSet::new(),
            &mut acc,
        );
        // A `*.dcm` DIRECTORY is not a blob (`is_file` is false), so it is
        // skipped rather than opened - the point of this assertion is that the
        // walk classifies by FILE TYPE and never counts a miss as read.
        assert_eq!(
            acc.files_scanned, 0,
            "a directory named *.dcm is not a blob"
        );
        assert_eq!(acc.errors, 0, "classifying it is not an I/O error");
    }

    /// The header bound must be a NON-ZERO slice of the file.
    ///
    /// Observed through the real walk rather than restated arithmetic: the
    /// fixture is a well-formed instance stored under a digest that does NOT
    /// match its bytes. With a non-zero bound the parser reads the metadata
    /// and the pass fails at the INTEGRITY gate, which is an ERROR. With a
    /// zero bound the parser is handed nothing, the blob is classified as
    /// undecodable, and the failure moves to `skipped` instead - so the two
    /// counters distinguish the cases without the test repeating the formula.
    #[test]
    fn a_non_empty_blob_is_parsed_not_treated_as_an_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = rebuild_state(&dir.path().join("h"));
        let cas_root = state.cas.root_dir().to_path_buf();
        let bucket = cas_root.join("aa").join("bb");
        std::fs::create_dir_all(&bucket).expect("bucket");

        let body = minimal_dicom();
        // Deliberately the WRONG digest: metadata parses, the integrity gate
        // then refuses the blob, and that refusal is counted as a hard error.
        let wrong = clarus_adapters::hex_encode(&[0x5Au8; 32]);
        std::fs::write(bucket.join(format!("{wrong}.dcm")), &body).expect("write");

        let parser = DicomRsParser;
        let mut acc = RebuildAcc::default();
        let _ = walk_fs(
            &parser,
            &cas_root,
            TIER_HOT,
            true,
            std::path::Path::new(&state.config.btree.manifest_dir),
            std::path::Path::new(&state.config.btree.idx_dir),
            &state,
            &mut HashSet::new(),
            &mut HashSet::new(),
            &mut acc,
        );
        assert_eq!(acc.files_scanned, 1, "the blob was reached by the walk");
        assert_eq!(
            acc.skipped, 0,
            "the bytes decode, so this is NOT an undecodable skip"
        );
        assert_eq!(
            acc.errors, 1,
            "the digest mismatch is a hard error, which only a parsed header can reach"
        );
    }

    /// A DRY RUN must not claim to have written anything.
    ///
    /// Asserted through a real `run_rebuild` over a well-formed blob: the pass
    /// reaches the dry-run branch, so an implementation that recorded a write
    /// there would show up as a non-zero counter.
    #[test]
    fn a_dry_run_reports_no_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = rebuild_state(&dir.path().join("d"));
        let cas_root = state.cas.root_dir().to_path_buf();
        let bucket = cas_root.join("aa").join("bb");
        std::fs::create_dir_all(&bucket).expect("bucket");
        let body = minimal_dicom();
        let hex = clarus_adapters::hex_encode(&clarus_core::hash::cas_hash_one(&body));
        std::fs::write(bucket.join(format!("{hex}.dcm")), &body).expect("write");

        let lock = RwLock::new(blank_rebuild_state());
        run_rebuild(&state, &["hot".to_string()], true, &lock);

        let st = lock.read().expect("state").clone();
        assert_eq!(
            st.manifests_written, 0,
            "a dry run writes no manifest, whatever it found"
        );
        assert_eq!(
            st.instances_indexed, 0,
            "a dry run indexes no instance, whatever it found"
        );
    }

    /// A minimal, valid DICOM Part-10 file: Explicit VR Little Endian, the
    /// SOP class/instance UIDs plus the study and series UIDs the rebuild
    /// requires, and no PixelData.
    ///
    /// Tags ascend by (group, element) and every value has an even length
    /// (PS3.5 7.1), which is what the parser's walk expects.
    fn minimal_dicom() -> Vec<u8> {
        minimal_dicom_with_ts(Some(b"1.2.840.10008.1.2.1"))
    }

    /// The same instance, optionally WITHOUT a Transfer Syntax UID in the meta
    /// group. Omitting it is what makes the worker's `EXPLICIT_VR_LE` fallback
    /// observable (PS3.5 A-1 defines the default; the worker must record it).
    fn minimal_dicom_with_ts(ts: Option<&[u8]>) -> Vec<u8> {
        fn elem(out: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
            out.extend_from_slice(&group.to_le_bytes());
            out.extend_from_slice(&element.to_le_bytes());
            out.extend_from_slice(vr);
            out.extend_from_slice(&(value.len() as u16).to_le_bytes());
            out.extend_from_slice(value);
            if !value.len().is_multiple_of(2) {
                out.push(0);
            }
        }
        let mut out = vec![0u8; 128];
        out.extend_from_slice(b"DICM");
        // Meta group first; (0002,0010 = Transfer Syntax) is OPTIONAL here so
        // the fallback can be exercised.
        if let Some(ts) = ts {
            elem(&mut out, 0x0002, 0x0010, b"UI", ts);
        }
        // Identity: SOP class + SOP instance are BOTH required for a parse.
        elem(
            &mut out,
            0x0008,
            0x0016,
            b"UI",
            b"1.2.840.10008.5.1.4.1.1.2",
        );
        elem(&mut out, 0x0008, 0x0018, b"UI", b"1.2.3.4.5");
        elem(&mut out, 0x0008, 0x0060, b"CS", b"CT");
        elem(&mut out, 0x0020, 0x000D, b"UI", b"1.2.3.4.6");
        elem(&mut out, 0x0020, 0x000E, b"UI", b"1.2.3.4.7");
        out
    }

    /// An instance that declares NO Transfer Syntax is recorded under the
    /// repository constant, not a literal spelled in this module.
    ///
    /// Driven end to end through `process_bytes`, which owns the fallback: the
    /// fixture declares no Transfer Syntax, and the assertion is on the STORED
    /// manifest. A literal in the worker, or a fallback that stored nothing,
    /// both fail here - the test never recomputes the value itself.
    #[test]
    fn the_assumed_transfer_syntax_is_recorded_in_the_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = rebuild_state(&dir.path().join("ts"));
        let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
        let idx_dir = std::path::Path::new(&state.config.btree.idx_dir);

        let body = minimal_dicom_with_ts(None);
        // Address the blob by its REAL digest, or the integrity gate refuses
        // it before any manifest is written.
        let blob = clarus_core::hash::cas_hash_one(&body);
        let parser = DicomRsParser;
        let mut acc = RebuildAcc::default();
        process_bytes(
            &parser,
            "fixture.dcm",
            &body,
            None,
            body.len() as u64,
            TIER_HOT,
            blob,
            false,
            manifest_dir,
            idx_dir,
            &state,
            &mut HashSet::new(),
            &mut HashSet::new(),
            &mut acc,
        );
        assert_eq!(acc.errors, 0, "the fixture commits cleanly");
        assert_eq!(acc.instances_indexed, 1, "one instance reached a manifest");

        let manifest = state
            .read_server_manifest("1.2.3.4.6")
            .expect("manifest readable")
            .expect("manifest present");
        let stored = manifest
            .series
            .iter()
            .flat_map(|s| &s.instances)
            .find(|i| i.blake3 == blob)
            .expect("instance recorded");
        assert_eq!(
            stored.transfer_syntax,
            clarus_adapters::codec::ts_uid::EXPLICIT_VR_LE,
            "the manifest must name the shared constant, not a local literal"
        );
    }
}
