// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! P1 group-commit series grouping regression (field fix A, 24.08).
//!
//! Field incident: one 432-instance study split into 84 series rows (1x349 +
//! 83x1) in the serialized QIDO. Root cause: `batch_add` pushed one
//! `SeriesModel` per ACCEPTED instance onto `StudyBatch.series` (ungrouped),
//! and on a fresh study `write_study_manifest_full` maps each model to a
//! manifest series row 1:1 - one series became N rows. `commit_manifests`
//! now consolidates by SeriesInstanceUID (`group_series_by_uid`). These tests
//! assert the invariant that would have caught the bug: N instances of ONE
//! series commit to ONE manifest series row of N instances; 2 series x M
//! instances commit to exactly 2 rows.

use clarus_adapters::storage::ContentAddressedStorage;
use clarus_core::query::StudyScanner;
use clarus_server::config::Config;
use clarus_server::handlers::stow_pure::{
    InstanceSource, ManifestBatch, commit_manifests, ingest_instance,
};
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::{AppState, MaintenanceState};
use std::collections::VecDeque;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::Instant;

// -- Minimal Explicit VR LE DICOM builder (mirrors stow_p1_groupcommit.rs) --

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
            buf.push(0x20);
        }
    }
}

fn build_instance(sop_uid: &str, study_uid: &str, series_uid: &str, pixel_byte: u8) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_uid.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"A^GROUPPAT");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"AGRP");
    append_element(&mut d, 0x0020, 0x000d, b"UI", study_uid.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", series_uid.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    append_element(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &[pixel_byte; 64]);
    d
}

// -- AppState builder (mirrors stow_p1_groupcommit.rs) ---------------------

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

/// Ingest one instance via the temp-file path into `batch` and assert Stored.
fn ingest_one(state: &AppState, batch: &mut ManifestBatch, sop: &str, study: &str, series: &str) {
    let dicom = build_instance(sop, study, series, 0xAB);
    let tmp = std::env::temp_dir().join(format!("ser_group_{sop}.dcm"));
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

/// Collect the SOP UIDs found in `manifest.series[0].instances`.
fn series_sop_uids(manifest: &clarus_core::manifest::Manifest) -> Vec<String> {
    manifest.series[0]
        .instances
        .iter()
        .filter_map(|i| {
            i.attrs
                .iter()
                .find(|(t, _)| *t == clarus_core::query::tag::SOP_INSTANCE_UID)
        })
        .filter_map(|(_, v)| match v {
            clarus_core::manifest::AttrValue::Text(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn p1_series_grouping_one_series_commits_one_row() {
    let (state, _temp) = make_state();
    let manifest_dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
    let study = "1.2.840.113619.3.1";
    let series = "1.2.840.113619.3.1.1";

    // 4 unique instances of ONE series in one batch.
    let mut batch = ManifestBatch::default();
    for i in 0..4 {
        ingest_one(
            &state,
            &mut batch,
            &format!("1.2.840.113619.3.1.1.{i}"),
            study,
            series,
        );
    }
    commit_manifests(&state, &batch);

    let m = clarus_core::manifest::read_manifest(&clarus_core::stow::manifest_path(
        &manifest_dir,
        study,
    ))
    .expect("manifest readable");
    assert_eq!(
        m.series.len(),
        1,
        "N instances of ONE series must commit to exactly ONE series row, got {}",
        m.series.len()
    );
    let uids = series_sop_uids(&m);
    assert_eq!(
        uids.len(),
        4,
        "the single row must carry all 4 instances, got {uids:?}"
    );
    for i in 0..4 {
        assert!(
            uids.contains(&format!("1.2.840.113619.3.1.1.{i}")),
            "row must contain sop .{i}, got {uids:?}"
        );
    }
}

#[test]
fn p1_series_grouping_two_series_commits_two_rows() {
    let (state, _temp) = make_state();
    let manifest_dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
    let study = "1.2.840.113619.3.2";
    let series_a = "1.2.840.113619.3.2.1";
    let series_b = "1.2.840.113619.3.2.2";

    // 2 series x 2 instances each in one batch.
    let mut batch = ManifestBatch::default();
    ingest_one(
        &state,
        &mut batch,
        "1.2.840.113619.3.2.1.0",
        study,
        series_a,
    );
    ingest_one(
        &state,
        &mut batch,
        "1.2.840.113619.3.2.1.1",
        study,
        series_a,
    );
    ingest_one(
        &state,
        &mut batch,
        "1.2.840.113619.3.2.2.0",
        study,
        series_b,
    );
    ingest_one(
        &state,
        &mut batch,
        "1.2.840.113619.3.2.2.1",
        study,
        series_b,
    );
    commit_manifests(&state, &batch);

    let m = clarus_core::manifest::read_manifest(&clarus_core::stow::manifest_path(
        &manifest_dir,
        study,
    ))
    .expect("manifest readable");
    assert_eq!(
        m.series.len(),
        2,
        "2 distinct series must commit to exactly 2 rows, got {}",
        m.series.len()
    );
    // Total instances across both rows = 4 (the grouping must not lose any).
    let total: usize = m.series.iter().map(|s| s.instances.len()).sum();
    assert_eq!(
        total, 4,
        "both series' instances preserved, got total {total}"
    );
}
