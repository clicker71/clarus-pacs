// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Raw content-hash throughput A/B: BLAKE3 vs SHA-256 on the CT corpus.
//!
//! The clean number behind the "why BLAKE3" claim: the same bytes, the same
//! machine, one hasher per group, nothing else in the timed loop. This bench
//! deliberately bypasses the CAS store path (no file IO, no rename), so the
//! figures are hashing throughput only.
//!
//! SHA-256 runs in pure software (RustCrypto `sha2`, no `asm` feature): this
//! box (Xeon E5-2690 v4, Broadwell-EP) has no SHA-NI, so software is the only
//! SHA-256 available here. BLAKE3 runs with its runtime-detected SIMD
//! (AVX2/SSE4.1). `sha2` is a dev-dependency (in prod it is gated behind
//! `s3`) and is never linked into the server binary.

mod common;

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use sha2::{Digest, Sha256};
use std::path::Path;

/// One buffer of a few CT slices (~8 MiB): large enough to amortise call
/// overhead, small enough to stay resident in cache. Pre-loaded once, outside
/// every timed loop.
fn payload(dir: &Path) -> Vec<u8> {
    let files = common::ct_slices(dir);
    let mut buf = Vec::new();
    let want = 8 * 1024 * 1024usize;
    for f in files {
        if buf.len() >= want {
            break;
        }
        buf.extend_from_slice(&common::read_bytes(&f));
    }
    buf
}

fn hash_benchmarks(c: &mut Criterion) {
    let data = payload(&common::criterion_dir());
    let len = data.len() as u64;

    let mut g = c.benchmark_group("hash/blake3");
    g.throughput(Throughput::Bytes(len));
    g.bench_function("one_buffer", |b| {
        b.iter(|| {
            let mut h = blake3::Hasher::new();
            h.update(black_box(data.as_slice()));
            black_box(h.finalize())
        })
    });
    g.finish();

    let mut g = c.benchmark_group("hash/sha256_soft");
    g.throughput(Throughput::Bytes(len));
    g.bench_function("one_buffer", |b| {
        b.iter(|| {
            let mut h = Sha256::new();
            h.update(black_box(data.as_slice()));
            black_box(h.finalize())
        })
    });
    g.finish();
}

criterion_group!(benches, hash_benchmarks);
criterion_main!(benches);
