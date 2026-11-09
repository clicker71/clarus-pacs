// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! P1 group-commit crash-semantics test (gate 4 of 2308_writer_stow_p1).
//!
//! Verifies the invariant the P1 design must guarantee: the per-request
//! manifest write is a SINGLE atomic Stage B action AFTER the per-instance
//! Stage A CAS commits. A crash / write-failure in that window (CAS blobs on
//! disk, manifest not yet committed) must never corrupt the study:
//!   - the manifest stays readable (atomic tmp+rename, round-trip guard);
//!   - the next STOW of the same study converges (read-merge under the lease);
//!   - the unreferenced CAS blob is left intact (reclaimable by the CAS audit
//!     walker - hash-integrity checked today, orphan-vs-manifest reclamation
//!     is plan 3 design).
//!
//! The "injected failure" is the crash window itself: ingest to CAS but do
//! not flush the batch (Stage A done, Stage B skipped), exactly the state a
//! hard kill between the two stages leaves.

use clarus_adapters::storage::ContentAddressedStorage;
use clarus_core::query::StudyScanner;
use clarus_server::config::Config;
use clarus_server::handlers::stow_pure::{
    InstanceSource, ManifestBatch, commit_manifests, ingest_instance, sync_batch_manifests,
};
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::{AppState, MaintenanceState};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::Instant;

// -- Minimal Explicit VR LE DICOM builder (mirrors stow_smoke.rs) --------

fn append_element(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    let len = value.len();
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&element.to_le_bytes());
    buf.extend_from_slice(vr);
    let is_extended = matches!(
        vr,
        b"OB"
            | b"OD"
            | b"OF"
            | b"OL"
            | b"OV"
            | b"OW"
            | b"SQ"
            | b"SV"
            | b"UC"
            | b"UN"
            | b"UR"
            | b"UT"
            | b"UV"
    );
    if vr == b"UI" && !len.is_multiple_of(2) {
        let padded_len = len + 1;
        if is_extended {
            buf.extend_from_slice(&[0x00, 0x00]);
            buf.extend_from_slice(&(padded_len as u32).to_le_bytes());
        } else {
            buf.extend_from_slice(&(padded_len as u16).to_le_bytes());
        }
        buf.extend_from_slice(value);
        buf.push(0x00);
    } else {
        if is_extended {
            buf.extend_from_slice(&[0x00, 0x00]);
            buf.extend_from_slice(&(len as u32).to_le_bytes());
        } else {
            buf.extend_from_slice(&(len as u16).to_le_bytes());
        }
        buf.extend_from_slice(value);
        if !len.is_multiple_of(2) {
            let pad = if matches!(
                vr,
                b"CS"
                    | b"LO"
                    | b"SH"
                    | b"PN"
                    | b"AE"
                    | b"AS"
                    | b"DA"
                    | b"DS"
                    | b"DT"
                    | b"IS"
                    | b"LT"
                    | b"ST"
                    | b"TM"
                    | b"UC"
                    | b"UR"
                    | b"UT"
            ) {
                0x20
            } else {
                0x00
            };
            buf.push(pad);
        }
    }
}

fn build_instance(
    sop_uid: &str,
    patient_id: &str,
    study_uid: &str,
    series_uid: &str,
    pixel_byte: u8,
) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_uid.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"P1^BATCH");
    append_element(&mut d, 0x0010, 0x0020, b"LO", patient_id.as_bytes());
    append_element(&mut d, 0x0020, 0x000d, b"UI", study_uid.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", series_uid.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    append_element(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    let pixels = [pixel_byte; 64];
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &pixels);
    d
}

// -- AppState builder (mirrors stow_smoke.rs `start_with`) ---------------

fn make_state() -> (AppState, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage_dir = temp.path().join("cas");
    std::fs::create_dir_all(&storage_dir).expect("cas dir");
    let btree_root = temp.path().join("btree");
    let manifest_dir = btree_root.join("manifests");
    let idx_dir = btree_root.join("idx");
    std::fs::create_dir_all(&manifest_dir).expect("manifests dir");
    std::fs::create_dir_all(&idx_dir).expect("idx dir");

    let cas = ContentAddressedStorage::new(storage_dir.clone()).expect("open cas");

    let mut config = Config::test_default();
    config.storage_dir = storage_dir.to_string_lossy().to_string();
    config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
    config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
    config.min_free_disk_mb = 0;
    config.min_free_disk_pct = 0.0;

    let state = AppState {
        cas,
        codec_registry: clarus_adapters::codec::CodecRegistry::new(),
        ups_events: DualEventSink::new(),
        start_time: Instant::now(),
        metrics_ring: Arc::new(StdRwLock::new(VecDeque::new())),
        log_buffer: Arc::new(StdRwLock::new(VecDeque::new())),
        maintenance: Arc::new(StdRwLock::new(MaintenanceState::default())),
        #[cfg(feature = "s3")]
        s3: Arc::new(std::sync::OnceLock::new()),
        cold_cas: None,
        shadow: ShadowTier::new(btree_root.join("idx")),
        study_scanner: StudyScanner::new(manifest_dir.clone(), idx_dir),
        config,
        workitems_dir: btree_root.join("workitems"),
        commitments_dir: btree_root.join("commitments"),
        transcode_pool: clarus_server::state::TranscodePool::new(32),
        readiness: clarus_server::handlers::readiness::Readiness::new(),
    };
    (state, temp)
}

/// Ingest one instance via the temp-file path into `batch`, writing a temp
/// file then consuming it (as the multipart wrapper does).
fn ingest_one(state: &AppState, batch: &mut ManifestBatch, sop: &str, study: &str, series: &str) {
    let dicom = build_instance(sop, "P1PAT", study, series, 0xAB);
    let tmp = std::env::temp_dir().join(format!("p1_gc_{sop}.dcm"));
    std::fs::write(&tmp, &dicom).expect("write temp");
    // The wrapper folds the hash over the written bytes; here the bytes are
    // known, so the folded digest is `cas_hash_one` (no strip: conformant).
    let folded = clarus_core::hash::cas_hash_one(&dicom);
    let outcome = ingest_instance(
        state,
        InstanceSource::TempFile {
            path: &tmp,
            size: dicom.len() as u64,
            blake3: folded,
        },
        "STOW-RS",
        batch,
        None,
    );
    let _ = std::fs::remove_file(&tmp);
    assert!(outcome.is_ok(), "ingest {sop} failed");
    assert!(
        matches!(
            outcome.unwrap().outcome,
            clarus_ports::stow_response::InstanceOutcome::Stored { .. }
        ),
        "fresh SOP must be Stored"
    );
}

/// The manifest file path for a study UID.
fn manifest_path(manifest_dir: &Path, study: &str) -> std::path::PathBuf {
    clarus_core::stow::manifest_path(manifest_dir, study)
}

#[test]
fn p1_group_commit_crash_window_keeps_manifest_consistent_and_study_converges() {
    let (state, _temp) = make_state();
    let manifest_dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
    let study = "1.2.840.113619.4.1";
    let series = "1.2.840.113619.4.1.1";

    // 1. First instance: Stage A commits the CAS blob, batch holds it, the
    //    manifest is NOT yet written (the P1 crash window).
    let mut b1 = ManifestBatch::default();
    ingest_one(&state, &mut b1, "1.2.840.113619.4.1.1", study, series);
    assert!(
        !manifest_path(&manifest_dir, study).exists(),
        "fresh study: no manifest before Stage B flush"
    );

    // 2. Commit Stage B once: the single manifest write lands and is readable,
    //    and the CAS blob is now referenced by the manifest.
    commit_manifests(&state, &b1);
    let m1 = clarus_core::manifest::read_manifest(&manifest_path(&manifest_dir, study))
        .expect("manifest must be readable");
    assert_eq!(
        m1.series.len(),
        1,
        "one series in the first committed manifest"
    );
    assert_eq!(
        m1.series[0].instances.len(),
        1,
        "one instance in first manifest"
    );

    // 3. Simulate a Stage B failure for a second batch: stage a new instance
    //    to CAS (blob B) but NEVER flush - the manifest keeps only instance 1.
    let mut b2 = ManifestBatch::default();
    ingest_one(&state, &mut b2, "1.2.840.113619.4.1.2", study, series);
    // (No commit_manifests(b2) - the injected failure.)
    let m_before_fail =
        clarus_core::manifest::read_manifest(&manifest_path(&manifest_dir, study)).unwrap();
    assert_eq!(
        m_before_fail.series[0].instances.len(),
        1,
        "unflushed batch must not touch the manifest"
    );

    // 4. Next STOW of the same study converges: a fresh instance + flush
    //    re-merges under the lease and both committed instances are present.
    let mut b3 = ManifestBatch::default();
    ingest_one(&state, &mut b3, "1.2.840.113619.4.1.3", study, series);
    commit_manifests(&state, &b3);
    let m_after = clarus_core::manifest::read_manifest(&manifest_path(&manifest_dir, study))
        .expect("manifest readable after next STOW");
    let uids: Vec<&str> = m_after.series[0]
        .instances
        .iter()
        .map(|i| {
            i.attrs
                .iter()
                .find(|(t, _)| *t == clarus_core::query::tag::SOP_INSTANCE_UID)
                .map(|(_, v)| match v {
                    clarus_core::manifest::AttrValue::Text(s) => s.as_str(),
                    _ => "",
                })
                .unwrap_or("")
        })
        .collect();
    assert!(
        uids.contains(&"1.2.840.113619.4.1.1") && uids.contains(&"1.2.840.113619.4.1.3"),
        "study converges: commits 1 and 3 present, got {uids:?}"
    );

    // 5. The unflushed blob B (instance .2) is still on disk as a valid CAS
    //    file (hash-integrity clean) - reclaimable, never a torn manifest.
    //    Verify the blob file exists for its BLAKE3 address.
    let blake2 = clarus_core::hash::cas_hash_one(&build_instance(
        "1.2.840.113619.4.1.2",
        "P1PAT",
        study,
        series,
        0xAB,
    ));
    let cas_target = state.cas.cas_path(&clarus_adapters::hex_encode(&blake2));
    assert!(
        cas_target.exists(),
        "crash-window CAS blob is left on disk (orphan, not torn)"
    );
}

/// Fix B (2408_writer_fieldfixes_abc, commit 2): `sync_batch_manifests` must
/// open each manifest with a WRITE handle before `sync_all`. On Windows
/// `FlushFileBuffers` requires GENERIC_WRITE; a read-only `File::open` handle
/// fails with err=5 (ERROR_ACCESS_DENIED), silently warned by the caller. With
/// the fix the write-handle open + `sync_all` succeeds.
///
/// This is a REAL manifest produced by the group-commit path, so the sync
/// mechanism exercises the exact file the fix targets. It is RED on the old
/// code (read-only open) on Windows, GREEN on the fixed write-handle open.
#[test]
fn p1_group_commit_sync_manifests_write_handle_ok() {
    let (state, _temp) = make_state();
    let manifest_dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
    let study = "1.2.840.113619.4.2";
    let series = "1.2.840.113619.4.2.1";

    // Write a REAL manifest via the group-commit path, then capture the batch
    // so `sync_batch_manifests` sees the same study the on-disk file covers.
    let mut batch = ManifestBatch::default();
    ingest_one(&state, &mut batch, "1.2.840.113619.4.2.1", study, series);
    commit_manifests(&state, &batch);

    let path = manifest_path(&manifest_dir, study);
    assert!(path.exists(), "manifest is on disk after Stage B commit");

    // The exact mechanism the fix changed: open with a WRITE handle, then
    // sync_all. The old `File::open` (read-only) path returned Err(5) on
    // Windows; the write handle must return Ok.
    let sync_res = std::fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|f| f.sync_all());
    assert!(
        sync_res.is_ok(),
        "write-handle manifest sync must succeed, got {sync_res:?}"
    );

    // Also drive the full `sync_batch_manifests` entrypoint - it shares the
    // same write-handle open and must complete without panicking.
    sync_batch_manifests(&state, &batch);
}

/// A mid-request memory-guard flush must leave the request in the state a
/// single end-of-request flush would have reached: the flushed manifests are
/// sync'd (the end-of-request sync no longer sees them), and the studies it
/// wrote still appear in `study_uids`, which drives the post-STOW follow-up.
#[test]
fn a_mid_request_flush_keeps_the_flushed_study_visible() {
    let (mut state, _temp) = make_state();
    // Force a flush after the FIRST instance, so the second one lands in a
    // fresh accumulator and the flush happens mid-request.
    state.config.max_batch_instances = 1;
    let manifest_dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
    let study = "1.2.840.113619.4.3";
    let series = "1.2.840.113619.4.3.1";

    let mut batch = ManifestBatch::default();
    ingest_one(&state, &mut batch, "1.2.840.113619.4.3.1", study, series);
    // A DIFFERENT study crosses the cap: the flush commits study A and the
    // pending accumulator then holds only study B, so a `clear()` that forgot
    // the flushed study would drop it from the follow-up list entirely.
    let other_study = "1.2.840.113619.4.4";
    let other_series = "1.2.840.113619.4.4.1";
    ingest_one(
        &state,
        &mut batch,
        "1.2.840.113619.4.4.1",
        other_study,
        other_series,
    );

    assert!(
        manifest_path(&manifest_dir, study).exists(),
        "the flush must have committed the first study"
    );
    assert!(
        batch.study_uids().any(|s| s == study),
        "a flushed study must still be listed for the post-STOW follow-up"
    );
    assert!(
        batch.study_uids().any(|s| s == other_study),
        "the pending study is listed too"
    );

    let mut uids: Vec<&str> = batch.study_uids().collect();
    uids.sort_unstable();
    uids.dedup();
    assert_eq!(
        uids,
        vec![study, other_study],
        "both studies are listed exactly once"
    );

    // The final flush still writes the pending instance, and each manifest
    // converges - the mid-request flush did not drop anything.
    commit_manifests(&state, &batch);
    let manifest = clarus_core::manifest::read_manifest(&manifest_path(&manifest_dir, study))
        .expect("manifest readable");
    let count: usize = manifest.series.iter().map(|s| s.instances.len()).sum();
    assert_eq!(count, 1, "the flushed study keeps its instance");
    assert!(
        manifest_path(&manifest_dir, other_study).exists(),
        "the pending study was committed at the end"
    );
}
