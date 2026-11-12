// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! QIDO benchmark: DICOMweb search against the B-tree tier.
//!
//! swapped the QIDO study/series/instance searches to the manifest
//! B-tree tier (`StudyScanner::search` for studies; `read_manifest` +
//! `search_series` / `search_instances` + the byte-exact streaming writers for
//! series/instances). This benchmark therefore measures the **real**
//! path - CBOR manifest scan + filter + JSON stream - not the SQLite fallback.
//!
//! The corpus is a deterministic, in-memory population of studies (each with
//! a few series and instances) committed to disk as CBOR manifests exactly as
//! the STOW producer would, then indexed via `reconcile_idx` and served by a
//! `StudyScanner` - the same objects the server builds at startup.
//!
//! Benchmarks (criterion, absolute medians reported in the write-up):
//!   - study/hot_date_name    StudyDate + PatientName study scan + stream
//!   - study/warm_modality    Modality-only study scan + stream
//!   - study/cold_physician   ReferringPhysician wildcard (empty) scan,
//!     cold vs ready RAM index (the F5 pair)
//!   - series/desc_wildcard   manifest open + search_series + write_series_json
//!   - instances/sop_uid      search_instances + write_instance_json

mod common;
use common::criterion_dir;

use clarus_core::manifest::{InstanceRecord, SeriesRecord, read_manifest};
use clarus_core::query::json::{write_instance_json, write_series_json, write_study_json};
use clarus_core::query::series::{self, InstanceFilter, SeriesFilter};
use clarus_core::query::{QueryFilter, StudyScanner};
use clarus_core::stow::{
    InstanceModel, SeriesModel, StudyManifestAttrs, write_study_manifest_full,
};
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use std::path::PathBuf;

/// Write a deterministic study population to CBOR manifests and index it.
fn build_btree_fixture(count: usize) -> (StudyScanner, PathBuf) {
    let base = std::env::temp_dir().join(format!("clarus_qidobench_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let manifests = base.join("manifests");
    let idx = base.join("idx");
    std::fs::create_dir_all(&manifests).expect("create manifests dir");

    let _ = criterion_dir(); // keep the shared corpus-common module wired

    for i in 0..count {
        let n = i % 100;
        let mut study = StudyManifestAttrs {
            patient_name: Some(format!("BENCH^{n:04}")),
            patient_id: Some(format!("PT-{n:04}")),
            study_instance_uid: format!("1.2.840.113619.bench.{i}"),
            study_date: Some(format!("2023{:02}{:02}", n % 9 + 1, n % 28 + 1)),
            accession_number: Some(format!("ACC-{i}")),
            referring_physician: Some(String::new()),
            modality: if i % 3 == 0 { "CT".into() } else { "MR".into() },
            study_description: Some(format!("Study {i}")),
            ..Default::default()
        };
        let mut series = Vec::new();
        for s in 0..i % 3 + 1 {
            let sn = series.len();
            let mut insts = Vec::new();
            for k in 0..(i + 1) % 4 + 1 {
                insts.push(InstanceModel {
                    sop_instance_uid: format!("1.2.840.113619.bench.{i}.{s}.{k}"),
                    sop_class_uid: Some("1.2.840.10008.5.1.4.1.1.4".into()),
                    instance_number: Some(format!("{k}")),
                    blake3: [0u8; 32],
                    storage_tier: 1,
                    transfer_syntax_uid: String::new(),
                    file_size: 0,
                    referenced_series_uids: Vec::new(),
                    referenced_sop_instance_uids: Vec::new(),
                    // The QIDO bench measures query, not frame chunking.
                    frames: Vec::new(),
                });
            }
            series.push(SeriesModel {
                series_instance_uid: format!("1.2.840.113619.bench.{i}.{s}"),
                modality: if i % 3 == 0 { "CT".into() } else { "MR".into() },
                series_number: Some(format!("{sn}")),
                series_date: Some(format!("2023{:02}{:02}", n % 9 + 1, n % 28 + 1)),
                series_time: Some(format!("{sn:04}00.000")),
                pps_start_date: None,
                pps_start_time: None,
                scheduled_procedure_step_id: None,
                requested_procedure_id: None,
                station_name: None,
                manufacturer: None,
                institutional_department_name: None,
                operators_name: None,
                series_description: Some(format!("Series {s} of {i}")),
                body_part_examined: Some("HEAD".into()),
                protocol_name: None,
                laterality: None,
                instances: insts,
            });
        }
        write_study_manifest_full(&manifests, &mut study, &series).expect("write b-tree manifest");
    }

    clarus_core::recovery::reconcile_idx(&manifests, &idx).expect("reconcile idx");
    let scanner = StudyScanner::new(manifests.clone(), idx);
    (scanner, manifests)
}

/// A representative hot study filter (StudyDate + PatientName).
fn build_hot_filter() -> QueryFilter {
    QueryFilter {
        patient_name: Some("BENCH*".to_string()),
        study_date_from: Some("20230101".to_string()),
        study_date_to: Some("20231231".to_string()),
        ..Default::default()
    }
}

fn qido_benchmarks(c: &mut Criterion) {
    let (scanner, manifests_dir) = build_btree_fixture(128);

    // Warm the date/modality accelerators once (reconcile_idx did the cold build).
    let warm = build_hot_filter();
    black_box(scanner.search(&warm).expect("warm study search").0);

    // study / hot: StudyDate + PatientName scan + stream
    {
        let filter = build_hot_filter();
        let mut group = c.benchmark_group("qido/study_hot_date_name");
        let mut scratch = [0u8; 4096];
        group.bench_function("scan_and_stream", |b| {
            b.iter(|| {
                let (res, _total) = scanner.search(black_box(&filter)).expect("study search");
                let mut total = 0usize;
                for r in &res {
                    total += write_study_json(&mut scratch, &r.qido, None, None).expect("fits");
                }
                total
            });
        });
        group.finish();
    }

    // study / warm: Modality-only scan + stream
    {
        let filter = QueryFilter {
            patient_name: None,
            modality: Some("CT".to_string()),
            ..Default::default()
        };
        let mut group = c.benchmark_group("qido/study_warm_modality");
        let mut scratch = [0u8; 4096];
        group.bench_function("scan_and_stream", |b| {
            b.iter(|| {
                let (res, _total) = scanner.search(black_box(&filter)).expect("study search");
                let mut total = 0usize;
                for r in &res {
                    total += write_study_json(&mut scratch, &r.qido, None, None).expect("fits");
                }
                total
            });
        });
        group.finish();
    }

    // study / cold: ReferringPhysicianName wildcard (empty corpus matches none)
    // - `cold_scan` = cold full manifest scan, no RAM index attached.
    // - `ready_index` = the same query answered from a built StudyQidoIndex.
    //
    // This is the F5 pair: the RAM QIDO index was gated for correctness
    // (`ready()`, scan.rs fallback) but its win was never measured. The
    // `ready()` assert below is what makes the second number honest - without
    // it a build race would silently benchmark the cold path twice.
    {
        let filter = QueryFilter {
            referring_physician: Some("*".to_string()),
            ..Default::default()
        };

        // Cold: the scanner built by `build_btree_fixture` has no index.
        assert!(
            scanner.index().is_none(),
            "cold scanner must have no attached index"
        );

        let mut indexed = scanner.clone();
        indexed.attach_index(clarus_core::query::StudyQidoIndex::build_from_dir(
            &manifests_dir,
        ));
        assert!(
            indexed.index().map(|i| i.ready()).unwrap_or(false),
            "attached index must be ready before measuring the index path"
        );

        let mut group = c.benchmark_group("qido/study_cold_physician");
        group.bench_function("cold_scan", |b| {
            b.iter(|| black_box(scanner.search(black_box(&filter)).expect("study search").0));
        });
        group.bench_function("ready_index", |b| {
            b.iter(|| black_box(indexed.search(black_box(&filter)).expect("study search").0));
        });
        group.finish();
    }

    // series: open a study manifest, search_series, stream
    {
        let study_uid = "1.2.840.113619.bench.0";
        let path = manifests_dir.join(format!("{study_uid}.manifest"));
        let manifest = read_manifest(&path).expect("read manifest");
        let filter = SeriesFilter {
            series_description: Some("Series*".to_string()),
            ..Default::default()
        };
        let mut out: Vec<&SeriesRecord> = Vec::new();
        let mut group = c.benchmark_group("qido/series_desc_wildcard");
        let mut scratch = [0u8; 4096];
        group.bench_function("search_and_stream", |b| {
            b.iter(|| {
                series::search_series(black_box(&manifest), black_box(&filter), &mut out);
                let mut total = 0usize;
                for r in &out {
                    total += write_series_json(&mut scratch, r, None, None, None).expect("fits");
                }
                total
            });
        });
        group.finish();
    }

    // instances: search_instances under one series, stream
    {
        let series_uid = "1.2.840.113619.bench.0.0";
        let study_uid = "1.2.840.113619.bench.0";
        let path = manifests_dir.join(format!("{study_uid}.manifest"));
        let manifest = read_manifest(&path).expect("read manifest");
        let filter = InstanceFilter {
            sop_instance_uid: Some("1.2.840.113619.bench.0.0.0".to_string()),
            sop_class_uid: None,
            instance_number: None,
        };
        let mut out: Vec<&InstanceRecord> = Vec::new();
        let mut group = c.benchmark_group("qido/instances_sop_uid");
        let mut scratch = [0u8; 4096];
        group.bench_function("search_and_stream", |b| {
            b.iter(|| {
                series::search_instances(
                    black_box(&manifest),
                    black_box(series_uid),
                    black_box(&filter),
                    &mut out,
                    false,
                );
                let mut total = 0usize;
                for r in &out {
                    total +=
                        write_instance_json(&mut scratch, r, study_uid, series_uid, None, None)
                            .expect("fits");
                }
                total
            });
        });
        group.finish();
    }
}

criterion_group!(benches, qido_benchmarks);
criterion_main!(benches);
