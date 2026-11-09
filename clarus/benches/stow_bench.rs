// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! STOW ingest benchmark against the B-tree manifest/shadow tier.
//!
//! Criterion benchmark for the STOW path (the earlier one was
//! removed together with DbActor). Measures the CURRENT STOW write path BEFORE
//! P1 group-commit, so the O(N^2) per-study manifest merge gets a before/
//! after number. Defender (AV) is out of scope - that is the A/B harness' job.
//!
//! The prod multipart path is: `dispatch_streams.rs` -> `ingest_instance(
//! state, InstanceSource::TempFile{path,size}, "STOW-RS")`. `ingest_tempfile`
//! is the full per-instance cost: `blake3_of_file` -> atomic rename into CAS ->
//! `decide_insert_result` (sop_owner read) -> shadow `dual_write` ->
//! `write_study_manifest_full` (read-merge-write + roundtrip re-read, O(N^2)
//! bytes per study) -> `on_study_committed` (RAM upsert). No fsync on the path.
//!
//! Scenarios:
//!   - stow/btree/ingest_one_study_100 / _500: one study, N unique instances,
//!     each via a temp file + `ingest_tempfile` (fresh store per sample).
//!     Catches the O(N^2) manifest merge - the main "before" number for P1.
//!   - stow/btree/ingest_many_studies_100: 1 instance x 100 studies. Isolates
//!     the fixed per-study cost as a control slice.
//!
//! Corpus: synthetic minimal Explicit VR LE DICOM built in memory (same
//! builder shape as `tests/stow_smoke.rs`). Synthetic is deliberate here -
//! no `D:\criterion` dependency; no new helper in `benches/common/` (NGX-001).

use clarus_adapters::storage::ContentAddressedStorage;
use clarus_core::query::StudyScanner;
use clarus_server::config::Config;
use clarus_server::handlers::stow_pure::{
    InstanceSource, ManifestBatch, commit_manifests, ingest_instance, sync_batch_manifests,
};
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::{AppState, MaintenanceState};
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::Instant;

// -- Synthetic minimal Explicit VR LE DICOM (copied from stow_smoke) -----

/// Append an Explicit VR DICOM element (PS3.5 7.1).
#[allow(clippy::too_many_lines)]
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

/// Build a minimal Explicit VR LE DICOM instance with a fixed identity and
/// non-empty pixel data. SOP UID and InstanceNumber are unique per instance.
#[allow(clippy::too_many_arguments)]
fn build_instance(
    sop_instance_uid: &str,
    patient_id: &str,
    study_uid: &str,
    series_uid: &str,
    instance_number: u32,
    pixel_byte: u8,
) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_instance_uid.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"BENCH^PAT");
    append_element(&mut d, 0x0010, 0x0020, b"LO", patient_id.as_bytes());
    append_element(&mut d, 0x0020, 0x000d, b"UI", study_uid.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", series_uid.as_bytes());
    append_element(
        &mut d,
        0x0020,
        0x0013,
        b"IS",
        format!("{instance_number}").as_bytes(),
    );
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0106, b"US", &0u16.to_le_bytes());
    let pixels = [pixel_byte; 64];
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &pixels);
    d
}

// -- AppState builder (mirrors tests/stow_smoke.rs `start_with`) ---------

/// Build a fresh in-memory AppState rooted under `root` (a live TempDir that
/// the caller keeps alive for the duration of the ingest).
fn make_state(root: &Path) -> AppState {
    let storage_dir = root.join("cas");
    std::fs::create_dir_all(&storage_dir).expect("cas dir");
    let btree_root = root.join("btree");
    let manifest_dir = btree_root.join("manifests");
    let idx_dir = btree_root.join("idx");
    std::fs::create_dir_all(&manifest_dir).expect("manifests dir");
    std::fs::create_dir_all(&idx_dir).expect("idx dir");

    let cas = ContentAddressedStorage::new(storage_dir.clone()).expect("open cas");

    let mut config = Config::test_default();
    config.storage_dir = storage_dir.to_string_lossy().to_string();
    config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
    config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
    // No preflight, no size/free-disk interference.
    config.min_free_disk_mb = 0;
    config.min_free_disk_pct = 0.0;

    AppState {
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
    }
}

/// Ingest `n` unique instances into ONE fresh study via temp-files.
/// Captures the whole-study ingest cost including the O(N^2) manifest merge.
fn ingest_one_study(n: usize, iter_id: u64) {
    let src_root = std::env::temp_dir().join(format!("clarus_stowbench_{}_{}", iter_id, n));
    let _ = std::fs::remove_dir_all(&src_root);
    std::fs::create_dir_all(&src_root).expect("body dir");

    let body_root = src_root.join("body");
    std::fs::create_dir_all(&body_root).expect("body dir");

    let store_root = src_root.join("store");
    std::fs::create_dir_all(&store_root).expect("store dir");

    let state = make_state(&store_root);
    let study_uid = format!("1.2.840.113619.stowbench.{iter_id}.study");
    let series_uid = format!("1.2.840.113619.stowbench.{iter_id}.series");
    let patient_id = format!("PT-{iter_id}");

    let mut batch = ManifestBatch::default();
    for i in 0..n {
        let sop_uid = format!("1.2.840.113619.stowbench.{iter_id}.{i}");
        let dicom = build_instance(
            &sop_uid,
            &patient_id,
            &study_uid,
            &series_uid,
            i as u32,
            0xAB,
        );
        let path = body_root.join(format!("{i}.tmp"));
        std::fs::write(&path, &dicom).expect("write temp file");
        let folded = clarus_core::hash::cas_hash_one(&dicom);
        let outcome = ingest_instance(
            &state,
            InstanceSource::TempFile {
                path: &path,
                size: dicom.len() as u64,
                blake3: folded,
            },
            "STOW-RS",
            &mut batch,
            None,
        );
        assert!(outcome.is_ok(), "ingest {i} failed");
    }
    // P1 group commit: the single per-study manifest write for this request.
    commit_manifests(&state, &batch);
    // P2 durability: sync the written manifests (mirrors the prod call site).
    sync_batch_manifests(&state, &batch);

    let _ = std::fs::remove_dir_all(&src_root);
}

/// Ingest one instance into `n` fresh studies. Isolates fixed per-study cost.
fn ingest_many_studies(n: usize, iter_id: u64) {
    let src_root = std::env::temp_dir().join(format!("clarus_stowbench_{}_many", iter_id));
    let _ = std::fs::remove_dir_all(&src_root);
    std::fs::create_dir_all(&src_root).expect("body dir");

    let body_root = src_root.join("body");
    std::fs::create_dir_all(&body_root).expect("body dir");

    let store_root = src_root.join("store");
    std::fs::create_dir_all(&store_root).expect("store dir");

    let state = make_state(&store_root);
    let patient_id = format!("PT-{iter_id}");

    let mut batch = ManifestBatch::default();
    for s in 0..n {
        let study_uid = format!("1.2.840.113619.stowbench.{iter_id}.study.{s}");
        let series_uid = format!("1.2.840.113619.stowbench.{iter_id}.series.{s}");
        let sop_uid = format!("1.2.840.113619.stowbench.{iter_id}.{s}.0");
        let dicom = build_instance(&sop_uid, &patient_id, &study_uid, &series_uid, 0, 0xCD);
        let path = body_root.join(format!("{s}.tmp"));
        std::fs::write(&path, &dicom).expect("write temp file");
        let folded = clarus_core::hash::cas_hash_one(&dicom);
        let outcome = ingest_instance(
            &state,
            InstanceSource::TempFile {
                path: &path,
                size: dicom.len() as u64,
                blake3: folded,
            },
            "STOW-RS",
            &mut batch,
            None,
        );
        assert!(outcome.is_ok(), "ingest {s} failed");
    }
    // P1 group commit: one manifest write per study for this request.
    commit_manifests(&state, &batch);
    // P2 durability: sync the written manifests (mirrors the prod call site).
    sync_batch_manifests(&state, &batch);

    let _ = std::fs::remove_dir_all(&src_root);
}

fn stow_benchmarks(c: &mut Criterion) {
    // Scenario 1: one study, N instances (the O(N^2) manifest merge).
    for &n in &[100usize, 500] {
        let counter = AtomicU64::new(0);
        let mut group = c.benchmark_group(format!("stow/btree/ingest_one_study_{n}"));
        group.sample_size(10);
        group.bench_function(format!("one_study_{n}"), |b| {
            b.iter(|| {
                let iter_id = counter.fetch_add(1, Ordering::Relaxed);
                ingest_one_study(n, iter_id);
                black_box(());
            });
        });
        group.finish();
    }

    // Scenario 2: 1 instance x 100 studies (fixed per-study cost).
    {
        let counter = AtomicU64::new(0);
        let mut group = c.benchmark_group("stow/btree/ingest_many_studies_100");
        group.sample_size(10);
        group.bench_function("one_per_study_100", |b| {
            b.iter(|| {
                let iter_id = counter.fetch_add(1, Ordering::Relaxed);
                ingest_many_studies(100, iter_id);
                black_box(());
            });
        });
        group.finish();
    }
}

criterion_group!(benches, stow_benchmarks);
criterion_main!(benches);
