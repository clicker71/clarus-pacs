// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! WADO benchmark: DICOMweb retrieve - CBOR manifest lookup -> CAS read.
//!
//! The WADO hot path performs no external-database lookup: production
//! `locate_object` / `plan_series_multipart` / `plan_study_multipart` resolve
//! instances from the per-study CBOR manifest and read the CAS blob addressed
//! by the manifest's BLAKE3 key (`cas_hex_key`). This benchmark mirrors that
//! flow: `read_manifest` -> `find_instance` (or series scan) -> `cas_hex_key` ->
//! `cas.read_all`. SQLite is not touched.
//!
//! The `sqlite-v0.3` baseline measured the pre-cut-over path (SQLite lookup +
//! SHA-256 CAS read), so the comparison is the cut-over's honest before/after.
//!
//! JPEG-LS CT slices are ~1.2 MB each, binary `application/dicom` with no
//! transcoding. This benchmark covers the pixel-data read path (manifest
//! lookup + full CAS read of the `.dcm` blob).

mod common;

use clarus_adapters::ContentAddressedStorage;
use clarus_core::manifest::{
    AttrValue, InstanceRecord, Manifest, SeriesRecord, StudyQido, SysHeader, read_manifest,
    write_manifest,
};
use clarus_ports::dicom::DicomParser;
use clarus_server::handlers::wado::helpers::cas_hex_key;
use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};

/// Transfer syntax of the `CT_*.dcm` corpus (JPEG-LS Lossless). Not measured
/// by the read path - it only populates the manifest field.
const JPEG_LS_TS: &str = "1.2.840.10008.1.2.4.80";

/// Decode a 64-char hex digest into `[u8; 32]`.
fn hex_to_32(s: &str) -> [u8; 32] {
    let b = s.as_bytes();
    assert_eq!(b.len(), 64, "expected 64-char hex CAS key");
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = (b[i * 2] as char).to_digit(16).expect("hex digit") as u8;
        let lo = (b[i * 2 + 1] as char).to_digit(16).expect("hex digit") as u8;
        out[i] = (hi << 4) | lo;
    }
    out
}

fn wado_benchmarks(c: &mut Criterion) {
    // -- Fixture: real CAS + one study manifest, seeded through the genuine
    //    store pipeline (BLAKE3-addressed CAS), mirroring production STOW.
    let base = std::env::temp_dir().join(format!("clarus_wado_bench_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let manifests_dir = base.join("manifests");
    std::fs::create_dir_all(&manifests_dir).expect("create manifests dir");
    let cas = ContentAddressedStorage::new(base.join("store")).expect("init CAS");

    let corpus = common::criterion_dir();
    let files = common::ct_slices(&corpus);
    let count = 128usize.min(files.len());
    assert!(count > 0, "criterion corpus contains no CT_*.dcm files");

    let mut instances: Vec<InstanceRecord> = Vec::with_capacity(count);
    let mut study_uid = String::new();
    let mut series_uid = String::new();
    let mut first_sop = String::new();
    for path in files.iter().take(count) {
        let bytes = common::read_bytes(path);
        let meta = clarus_adapters::dicom_rs::DicomRsParser
            .extract_metadata(&bytes)
            .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        if study_uid.is_empty() {
            study_uid = meta.study_instance_uid.clone();
            series_uid = meta.series_instance_uid.clone();
            first_sop = meta.sop_instance_uid.clone();
        }
        let stored = cas
            .store_dicom(std::io::Cursor::new(bytes), 512 * 1024 * 1024)
            .expect("CAS store during seed");
        instances.push(InstanceRecord {
            attrs: vec![(
                clarus_core::query::tag::SOP_INSTANCE_UID,
                AttrValue::Text(meta.sop_instance_uid),
            )],
            transfer_syntax: JPEG_LS_TS.to_string(),
            storage_tier: clarus_core::manifest::TIER_HOT,
            rejection_reason: 0,
            blake3: hex_to_32(&stored),
            file_size: common::read_bytes(path).len() as u64,
            frames: Vec::new(),
        });
    }

    let manifest = Manifest {
        sys: SysHeader {
            version: 1,
            storage_tier: clarus_core::manifest::TIER_HOT,
            total_pixel_bytes: 0,
            arrived_at: 0,
        },
        study: StudyQido::default(),
        series: vec![SeriesRecord {
            attrs: vec![(
                clarus_core::query::tag::SERIES_INSTANCE_UID,
                AttrValue::Text(series_uid.clone()),
            )],
            instances,
        }],
    };
    let manifest_path = manifests_dir.join(format!("{study_uid}.manifest"));
    write_manifest(&manifest_path, &manifest).expect("write study manifest");

    // -- single_instance --------------------------------------------------
    // Manifest lookup by SOP UID (WADO-RS:
    // /studies/{s}/series/{se}/instances/{i}) + full CAS read of ~1.2 MB.
    {
        let mut group = c.benchmark_group("wado/single_instance");
        group.throughput(Throughput::Bytes(1_200_000));
        group.bench_function("uid_lookup_then_read_all", |b| {
            b.iter(|| {
                let manifest = read_manifest(black_box(&manifest_path)).expect("manifest read");
                let inst = manifest
                    .find_instance(black_box(&first_sop))
                    .expect("find instance");
                let key = cas_hex_key(inst);
                let bytes = cas.read_all(black_box(&key)).expect("cas read");
                black_box(bytes);
            });
        });
        group.finish();
    }

    // -- single_series ----------------------------------------------------
    // All instances of one series: manifest scan filtered by SeriesInstanceUID
    // (mirrors `plan_series_multipart`), then read each blob from the CAS.
    {
        let mut group = c.benchmark_group("wado/single_series");
        group.bench_function("all_series_instances", |b| {
            b.iter(|| {
                let manifest = read_manifest(black_box(&manifest_path)).expect("manifest read");
                let mut n = 0usize;
                for series in &manifest.series {
                    let is_target = series.attrs.iter().any(|(t, v)| {
                        *t == clarus_core::query::tag::SERIES_INSTANCE_UID
                            && matches!(v, AttrValue::Text(s) if s == &series_uid)
                    });
                    if !is_target {
                        continue;
                    }
                    for inst in &series.instances {
                        let key = cas_hex_key(inst);
                        let _ = cas.read_all(black_box(&key)).expect("cas read");
                        n += 1;
                    }
                }
                black_box(n);
            });
        });
        group.finish();
    }

    // -- single_study -----------------------------------------------------
    // Entire study (all series/instances) via one manifest read + CAS reads.
    {
        let mut group = c.benchmark_group("wado/single_study");
        group.bench_function("all_study_instances", |b| {
            b.iter(|| {
                let manifest = read_manifest(black_box(&manifest_path)).expect("manifest read");
                let mut n = 0usize;
                for series in &manifest.series {
                    for inst in &series.instances {
                        let key = cas_hex_key(inst);
                        let _ = cas.read_all(black_box(&key)).expect("cas read");
                        n += 1;
                    }
                }
                black_box(n);
            });
        });
        group.finish();
    }

    drop(cas);
    let _ = std::fs::remove_dir_all(&base);
}

criterion_group!(benches, wado_benchmarks);
criterion_main!(benches);
