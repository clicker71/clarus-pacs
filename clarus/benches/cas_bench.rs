// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! CAS benchmark: content-addressed storage (SHA-256, 1-level, streaming).
//!
//! Baseline benchmark for `crates/clarus-adapters/src/storage/mod.rs`.
//! swaps SHA-256 -> BLAKE3 and 1-level -> 2-level directories; these numbers are
//! the reference point for that comparison.
//!
//! Metrics: MB/s (via `Throughput::Bytes`), P50/P99 latency (criterion default).
//!
//! Write-path note: `store_dicom` deduplicates by SHA-256. To measure the real
//! write path (hash + atomic rename) rather than the AlreadyExists dedup fast
//! path, every sequential-iteration call mutates a single byte of the payload
//! via a monotonically increasing counter, guaranteeing a fresh hash. The
//! storage root itself is created once, outside the timed region.

mod common;

use clarus_adapters::ContentAddressedStorage;
use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Return `payload` with its first 8 bytes XOR-ed against a global counter.
/// Guarantees a unique content hash per call so stores never dedup.
fn uniquify(mut payload: Vec<u8>, salt: u64) -> Vec<u8> {
    if payload.len() < 8 {
        payload.extend_from_slice(&salt.to_le_bytes());
        return payload;
    }
    let bytes = salt.to_le_bytes();
    for (i, b) in bytes.iter().enumerate() {
        payload[i] ^= b;
    }
    payload
}

fn cas_benchmarks(c: &mut Criterion) {
    let counter = AtomicU64::new(0);
    let corpus = common::criterion_dir();
    let root: PathBuf =
        std::env::temp_dir().join(format!("clarus_cas_bench_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cas = ContentAddressedStorage::new(root.clone());
    let cas = cas.expect("cas init");

    let ct_files = common::ct_slices(&corpus);
    let samples: Vec<Vec<u8>> = ct_files
        .iter()
        .step_by(4)
        .map(|p| common::read_bytes(p))
        .collect();
    if samples.is_empty() {
        panic!("criterion corpus contains no CT_*.dcm files");
    }

    // -- put_single ------------------------------------------------------
    // Store 1 DICOM slice. Unique payload per iteration keeps the write path
    // honest across repeats; throughput is per-file MB.
    {
        let base = samples[0].clone();
        let len = base.len() as u64;
        let mut group = c.benchmark_group("cas/put_single");
        group.throughput(Throughput::Bytes(len));
        group.bench_function("sha256_1file", |b| {
            b.iter(|| {
                let salt = counter.fetch_add(1, Ordering::Relaxed);
                let payload = black_box(uniquify(base.clone(), salt));
                let hash = cas.store_dicom(std::io::Cursor::new(payload), 512 * 1024 * 1024);
                black_box(hash.expect("store must succeed"));
            });
        });
        group.finish();
    }

    // -- put_batch_100 ---------------------------------------------------
    // 100 sequential stores per iteration. Bytes-per-iteration = sum of 100
    // files, giving a true MB/s figure for the batch.
    {
        let mut batch: Vec<Vec<u8>> = Vec::with_capacity(100);
        for i in 0..100 {
            batch.push(samples[i % samples.len()].clone());
        }
        let total_bytes: u64 = batch.iter().map(|b| b.len() as u64).sum();
        let mut group = c.benchmark_group("cas/put_batch_100");
        group.throughput(Throughput::Bytes(total_bytes));
        group.bench_function("sequential_100", |b| {
            b.iter(|| {
                let salt_base = counter.fetch_add(1, Ordering::Relaxed);
                for (i, payload) in batch.iter().enumerate() {
                    let payload = black_box(uniquify(payload.clone(), salt_base * 100 + i as u64));
                    let hash = cas.store_dicom(std::io::Cursor::new(payload), 512 * 1024 * 1024);
                    black_box(hash.expect("store must succeed"));
                }
            });
        });
        group.finish();
    }

    // -- get_exists ------------------------------------------------------
    // Read back a stored file: open (stat) + full read. Prepared once; the
    // timed body re-reads the same stable hash so the OS page cache is warm,
    // matching steady-state WADO-read behaviour.
    {
        let payload = samples[2].clone();
        let store_payload = payload.clone();
        let hash = cas
            .store_dicom(std::io::Cursor::new(store_payload), 512 * 1024 * 1024)
            .expect("store");
        let len = payload.len() as u64;
        let mut group = c.benchmark_group("cas/get_exists");
        group.throughput(Throughput::Bytes(len));
        // read_all = full in-memory read (WADO full-object / transcoding path).
        group.bench_function("read_all", |b| {
            b.iter(|| {
                let bytes = cas.read_all(&hash);
                black_box(bytes.expect("read must succeed"));
            });
        });
        group.finish();
    }

    // -- get_missing -----------------------------------------------------
    // Confirm-absence probe: a well-formed hex hash that does not exist.
    // `0...01` is a 64-char hex digest; `read_dicom` resolves it, hits ENOENT,
    // and maps it to StorageError::NotFound. Cold-path lookup cost.
    {
        let missing = format!("{:0>63}1", "");
        let mut group = c.benchmark_group("cas/get_missing");
        group.bench_function("stat_enotfound", |b| {
            b.iter(|| {
                let res = cas.read_dicom(black_box(&missing));
                // A missing well-formed hash must surface as NotFound - this
                // both consumes the must_use Result and validates the probe.
                assert!(res.is_err());
                black_box(res.is_err());
            });
        });
        group.finish();
    }

    drop(cas);
    let _ = std::fs::remove_dir_all(&root);
}

criterion_group!(benches, cas_benchmarks);
criterion_main!(benches);
