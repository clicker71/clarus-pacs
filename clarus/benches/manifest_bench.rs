// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Manifest path benchmark against the current v1 CBOR manifest format.
//!
//! Manifest integrity plan P0: the "before" number for the P1 format-v2
//! change. Measures the CURRENT public-API manifest write/read/scan cost so
//! post-P1 we get a before/after. The bench must survive P1 with zero edits -
//! it touches ONLY the public API (`write_study_manifest_full`,
//! `read_manifest`, `read_manifest_study_only`), never raw offsets.
//!
//! Note: there is no header-fast-path scanner in v1 (that arrives in P2); the
//! scan group is the v1-form scan = `read_dir` + `read_manifest` per file.
//!
//! Groups (criterion names `manifest_btree_*`):
//!   - manifest/write_one_study_100 / _500: ONE study, N unique instances
//!     (one series, all in memory) -> `write_study_manifest_full` into a fresh
//!     temp dir per sample. This is the "before" header+write number for P1.
//!   - manifest/read_study_only_100 / _500: N pre-written manifest files
//!     (written via the same write fn in setup) ->
//!     `read_manifest_study_only` for each (the QIDO hotpath).
//!   - manifest/scan_dir_100 / _500: read_dir of the manifest dir +
//!     `read_manifest` over every file (full decode; v1-form scan).
//!
//! Corpus is synthetic in memory; the builder is local to this file
//! (NGX-001 - no new `benches/common` helper, no prod code touched).

use clarus_core::manifest::{read_manifest, read_manifest_study_only};
use clarus_core::manifest_cache;
use clarus_core::stow::{
    InstanceModel, SeriesModel, StudyManifestAttrs, write_study_manifest_full,
};
use clarus_server::handlers::wado::helpers::load_manifest;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Build one study's manifest attributes (the study-level header).
fn study_attrs(study_uid: &str, patient_id: &str, modality: &str) -> StudyManifestAttrs {
    StudyManifestAttrs {
        patient_name: Some("BENCH^PAT".to_string()),
        patient_id: Some(patient_id.to_string()),
        patient_id_issuer: None,
        patient_birth_date: Some("19700101".to_string()),
        patient_sex: Some("O".to_string()),
        study_instance_uid: study_uid.to_string(),
        study_date: Some("20260101".to_string()),
        study_time: None,
        study_description: Some("manifest bench study".to_string()),
        accession_number: None,
        referring_physician: Some("BENCH^RAD".to_string()),
        performing_physician: None,
        name_of_reading_physicians: None,
        institution_name: None,
        requesting_physician: None,
        requested_procedure_description: None,
        responsible_person: None,
        modality: modality.to_string(),
        study_id: Some("BENCH-ID".to_string()),
        num_series: 0,
        num_instances: 0,
        modalities_in_study: None,
        other_patient_ids: None,
    }
}

/// Build a real (non-enum) instance model with a unique SOP UID + BLAKE3.
fn instance_model(sop_uid: &str, i: u32) -> InstanceModel {
    InstanceModel {
        sop_instance_uid: sop_uid.to_string(),
        sop_class_uid: Some("1.2.840.10008.5.1.4.1.1.2".to_string()),
        instance_number: Some(i.to_string()),
        // Deterministic 32-byte CAS address (blake3 slot from the index).
        blake3: [(i.wrapping_mul(31)) as u8; 32],
        storage_tier: clarus_core::manifest::TIER_HOT,
        transfer_syntax_uid: "1.2.840.10008.1.2.1".to_string(),
        file_size: 512 * 1024,
        // The bench writes ordinary images, not derived objects, so there is
        // nothing captured for the deletion cascade to match on.
        referenced_series_uids: Vec::new(),
        referenced_sop_instance_uids: Vec::new(),
        // The bench measures the manifest write/read path, not the frame
        // table, and it has no DICOM bytes to walk - an empty table keeps
        // the pre-existing whole-blob semantics.
        frames: Vec::new(),
    }
}

/// Build one series carrying `n` unique instances.
fn series_with(n: usize, study_uid: &str, series_uid: &str) -> SeriesModel {
    let instances = (0..n)
        .map(|i| {
            instance_model(
                &format!("1.2.840.113619.mbench.{study_uid}.{series_uid}.{i}"),
                i as u32,
            )
        })
        .collect();
    SeriesModel {
        series_instance_uid: series_uid.to_string(),
        modality: "CT".to_string(),
        series_number: Some("1".to_string()),
        series_date: None,
        series_time: None,
        pps_start_date: None,
        pps_start_time: None,
        scheduled_procedure_step_id: None,
        requested_procedure_id: None,
        station_name: None,
        manufacturer: None,
        institutional_department_name: None,
        operators_name: None,
        series_description: Some("bench series".to_string()),
        body_part_examined: None,
        protocol_name: None,
        laterality: None,
        instances,
    }
}

/// A throwaway temp working dir for one sample.
struct SampleDir {
    root: PathBuf,
    manifests: PathBuf,
}

impl SampleDir {
    fn new(tag: &str, sample: u64) -> Self {
        let root = std::env::temp_dir().join(format!(
            "clarus_mbench_{tag}_{}_{}",
            std::process::id(),
            sample
        ));
        let manifests = root.join("manifests");
        std::fs::create_dir_all(&manifests).expect("manifests dir");
        SampleDir { root, manifests }
    }
    fn manifests(&self) -> &Path {
        &self.manifests
    }
}

impl Drop for SampleDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Write ONE study manifest with `n` instances in a fresh temp dir.
fn write_one_study(n: usize, sample: u64) {
    let d = SampleDir::new("w", sample);
    let study_uid = format!("1.2.840.113619.mbench.w.{sample}");
    let mut attrs = study_attrs(&study_uid, &format!("PT-{sample}"), "CT");
    let series = series_with(n, &study_uid, &format!("{study_uid}.series"));
    let path = write_study_manifest_full(d.manifests(), &mut attrs, std::slice::from_ref(&series))
        .expect("write manifest");
    black_box(path);
}

/// Pre-write `n` manifest files (one study per file, `inst` instances each).
fn prewrite_studies(n: usize, inst: usize, sample: u64) -> SampleDir {
    let d = SampleDir::new("r", sample);
    for i in 0..n {
        let study_uid = format!("1.2.840.113619.mbench.r.{sample}.{i}");
        let mut attrs = study_attrs(&study_uid, &format!("PT-{sample}-{i}"), "CT");
        let series = series_with(inst, &study_uid, &format!("{study_uid}.s"));
        write_study_manifest_full(d.manifests(), &mut attrs, std::slice::from_ref(&series))
            .expect("write manifest");
    }
    d
}

/// Group 2: `read_manifest_study_only` over every pre-written file.
fn read_study_only(n: usize, sample: u64) {
    let d = prewrite_studies(n, 1, sample);
    for i in 0..n {
        let p = d
            .manifests()
            .join(format!("1.2.840.113619.mbench.r.{sample}.{i}.manifest"));
        black_box(read_manifest_study_only(&p).expect("read"));
    }
}

/// Group 3: read_dir + full `read_manifest` for every file (v1-form scan).
fn scan_dir(n: usize, sample: u64) {
    let d = prewrite_studies(n, 1, sample);
    let mut files: Vec<PathBuf> = Vec::with_capacity(n);
    for e in std::fs::read_dir(d.manifests()).expect("read_dir") {
        let e = e.expect("dir entry");
        if e.path().extension().and_then(|x| x.to_str()) == Some("manifest") {
            files.push(e.path());
        }
    }
    assert_eq!(files.len(), n, "expected {n} manifest files");
    for p in &files {
        black_box(read_manifest(p).expect("read"));
    }
}

/// Extra group 4 (F4): the decoded-manifest cache, hit vs miss.
///
/// `hit` = `load_manifest` with the entry already resident (an `Arc::clone`,
/// no decode). `miss` = the same call with the entry removed first, so it
/// decodes the CBOR manifest from disk and re-inserts. The cache is a
/// process-global static keyed by manifests dir + study UID, so both
/// functions MUST use the same study UID and dir; `miss` invalidates the slot
/// through the public writer entry point (`begin_write`), which takes the
/// entry - the same thing a commit does.
///
/// The pair is the before/after number the cache shipped without: the
/// difference is the decode cost the cache removes from every WADO/QIDO read.
fn cache_hit_vs_miss(c: &mut Criterion) {
    let d = prewrite_studies(1, 1, 0);
    let study = "1.2.840.113619.mbench.r.0.0";
    let dir = d.manifests().to_path_buf();

    // Prime the cache, then confirm the warm read is served from it: the
    // decoded study must carry the instance we wrote.
    black_box(load_manifest(&dir, study).expect("prime"));
    assert!(
        load_manifest(&dir, study)
            .expect("warm")
            .study
            .instance_count
            == 1,
        "warm load must return the primed study (1 instance written)"
    );

    let mut group = c.benchmark_group("manifest/cache_hit_vs_miss");
    group.bench_function("hit", |b| {
        b.iter(|| black_box(load_manifest(&dir, study).expect("hit load")));
    });
    group.bench_function("miss", |b| {
        b.iter(|| {
            // Invalidate: the writer entry point removes the cached entry,
            // so the measured load decodes from disk and re-inserts.
            let (_, taken) = manifest_cache::begin_write(study);
            black_box(taken);
            black_box(load_manifest(&dir, study).expect("miss load"))
        });
    });
    group.finish();
}

fn manifest_benchmarks(c: &mut Criterion) {
    let counter = AtomicU64::new(0);
    for &n in &[100usize, 500] {
        // Group 1: one study, N instances -> single manifest write.
        {
            let mut group = c.benchmark_group(format!("manifest/write_one_study_{n}"));
            group.sample_size(10);
            group.bench_function(format!("write_one_study_{n}"), |b| {
                b.iter(|| {
                    let sample = counter.fetch_add(1, Ordering::Relaxed);
                    write_one_study(n, sample);
                    black_box(());
                });
            });
            group.finish();
        }
        // Group 2: N files -> read_manifest_study_only each.
        {
            let mut group = c.benchmark_group(format!("manifest/read_study_only_{n}"));
            group.sample_size(10);
            group.bench_function(format!("read_study_only_{n}"), |b| {
                b.iter(|| {
                    let sample = counter.fetch_add(1, Ordering::Relaxed);
                    read_study_only(n, sample);
                    black_box(());
                });
            });
            group.finish();
        }
        // Group 3: N files -> read_dir + full read_manifest each.
        {
            let mut group = c.benchmark_group(format!("manifest/scan_dir_{n}"));
            group.sample_size(10);
            group.bench_function(format!("scan_dir_{n}"), |b| {
                b.iter(|| {
                    let sample = counter.fetch_add(1, Ordering::Relaxed);
                    scan_dir(n, sample);
                    black_box(());
                });
            });
            group.finish();
        }
    }
    cache_hit_vs_miss(c);
}

criterion_group!(benches, manifest_benchmarks);
criterion_main!(benches);
