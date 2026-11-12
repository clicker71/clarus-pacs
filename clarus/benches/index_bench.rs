// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Synthetic micro-benchmarks for the index structures the server relies on,
//! A-D (BLAKE3 keyed indexes, n-gram postings, fuzzy search).
//!
//! Unlike the other suites, this benchmark does NOT exercise current server
//! code. It captures the baseline cost of the fundamental operations those
//! structures rely on, so a before/after comparison is possible:
//!   - index/mm_read:    linear scan of a warm addressable index file
//!   - index/ngram:      sorted-list intersection (2 and 4 posting lists)
//!   - index/bitap:      bit-parallel approximate match, 1 error
//!
//! Metric: ns/operation (criterion default for a single timed call).

mod common;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use std::path::PathBuf;

// -- mmap_read -------------------------------------------------------------
// A future `.idx` file is scanned linearly once its pages are resident. The
// real cost is the byte-walk, which a warm in-memory scan reproduces without
// an mmap dependency. Data layout mirrors tomorrow's design: 8-byte (sort_key,
// offset) records.

/// Build a synthetic `.idx` file with `n` 8-byte records and return its path.
fn write_synth_idx(dir: PathBuf, n: usize, tag: &str) -> PathBuf {
    let path = dir.join(format!("{tag}_{n}.idx"));
    let mut buf = Vec::with_capacity(n * 8);
    for i in 0..n as u64 {
        buf.extend_from_slice(&i.to_le_bytes()); // sort_key
        buf.extend_from_slice(&(i * 8).to_le_bytes()); // byte offset
    }
    std::fs::write(&path, &buf).expect("write synth idx");
    path
}

/// Linear scan: sum every 8th byte, mirroring the walk a query performs.
fn scan_idx(data: &[u8]) -> u64 {
    let mut acc: u64 = 0;
    for chunk in data.chunks_exact(8) {
        let key = u64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        acc = acc.wrapping_add(key);
    }
    acc
}

// -- ngram_intersect -------------------------------------------------------
// Sorted posting lists of study-uid hashes. Intersection is the core of a
// multi-attribute QIDO. 2 lists (hot path) and 4 lists (deep query).

/// Merge-intersect two sorted slices, outputting the common elements.
fn intersect_sorted(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
}

/// Sorted random-ish list with ~m matching elements for deterministic results.
fn sorted_posting(base: u32, n: usize, matches: &[u32]) -> Vec<u32> {
    let mut v: Vec<u32> = Vec::with_capacity(n);
    // Stride by a prime so consecutive lists are offset but sorted.
    for k in 0..(n - matches.len()) {
        v.push(base.wrapping_add((k as u32).wrapping_mul(7)));
    }
    v.extend_from_slice(matches);
    v.sort_unstable();
    v.dedup();
    v
}

// -- bitap_match -----------------------------------------------------------
// Bit-parallel approximate string matching (Wu-Manber). Threshold k=1 error.
// Baseline for the fuzzy PatientName search the future index will serve.

/// Bitap fuzzy match (Wu-Manber, k errors). Returns true if `text` contains
/// `pat` with at most `k` substitutions/insertions/deletions - the operation
/// a fuzzy PatientName index would run per candidate.
fn bitap_match(text: &[u8], pat: &[u8], k: usize) -> bool {
    if pat.is_empty() {
        return true;
    }
    if pat.len() > 64 {
        // Bitap is 64-bit; longer patterns fall back to false in this baseline
        // harness (production would chunk). Not reachable with our inputs.
        return false;
    }
    // Bitmask per byte: bit i set if that byte appears at position i in `pat`.
    let mut mask = [0u64; 256];
    for (i, &c) in pat.iter().enumerate() {
        mask[c as usize] |= 1u64 << i;
    }
    // R[d] tracks the match-state for error budget d. R[d] evolves as
    //   R[d] = ((R[d] | M) << 1) & (R[d-1] << 1 | 1) & ...  (approximate form)
    let full = (1u64 << pat.len()) - 1;
    let mut prev = vec![0u64; k + 1];
    let mut cur = vec![0u64; k + 1];
    // Classic Wu-Manber: R[d] seeded with the first d bits set, which lets the
    // deletion path consume up to d leading characters for free.
    for (d, slot) in prev.iter_mut().enumerate() {
        *slot = ((1u64 << d) - 1) | 1;
    }

    for &c in text {
        let m = mask[c as usize];
        // k+1 rows; sub/ins/del combine into the bitmask update.
        cur[0] = ((prev[0] | m) << 1) & full;
        for d in 1..=k {
            let sub = ((prev[d] | m) << 1) & full;
            let ins = cur[d - 1];
            let del = prev[d - 1];
            // On any of the three, a cleared bit in bit (len-1) means matched.
            cur[d] = sub & ins & del;
        }
        if cur[0] & (1u64 << (pat.len() - 1)) == 0
            || (k > 0 && cur[k] & (1u64 << (pat.len() - 1)) == 0)
        {
            return true;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    false
}

fn index_benchmarks(c: &mut Criterion) {
    let dir = std::env::temp_dir().join(format!("clarus_idx_bench_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);

    // -- mmap_read --------------------------------------------------------
    {
        let mut group = c.benchmark_group("index/mm_read");
        for &n in &[1_000usize, 10_000, 100_000] {
            let path = write_synth_idx(dir.clone(), n, "scan");
            let data = std::fs::read(&path).expect("read synth idx");
            group.bench_function(format!("scan_{n}"), |ben| {
                ben.iter(|| black_box(scan_idx(black_box(&data))));
            });
        }
        group.finish();
    }

    // -- ngram_intersect --------------------------------------------------
    {
        let mut group = c.benchmark_group("index/ngram");
        let common_matches: Vec<u32> = (1..=20u32).collect();
        for &n in &[100usize, 1_000, 10_000] {
            let a = sorted_posting(100, n, &common_matches);
            let b = sorted_posting(200, n, &common_matches);
            let c_list = sorted_posting(300, n, &common_matches);
            let d_list = sorted_posting(400, n, &common_matches);
            let mut out = Vec::with_capacity(common_matches.len());
            group.bench_function(format!("intersect2_{n}"), |ben| {
                ben.iter(|| {
                    intersect_sorted(black_box(&a), black_box(&b), &mut out);
                    black_box(&out);
                });
            });
            group.bench_function(format!("intersect4_{n}"), |ben| {
                ben.iter(|| {
                    let mut t1 = Vec::with_capacity(out.capacity());
                    intersect_sorted(black_box(&a), black_box(&b), &mut t1);
                    let mut t2 = Vec::with_capacity(out.capacity());
                    intersect_sorted(&t1, black_box(&c_list), &mut t2);
                    intersect_sorted(&t2, black_box(&d_list), &mut out);
                    black_box(&out);
                });
            });
        }
        group.finish();
    }

    // -- bitap_match ------------------------------------------------------
    {
        let mut group = c.benchmark_group("index/bitap");
        // Realistic patient-name alphabet; the text is a concatenated registry
        // row, and the pattern is a 6-20 char surname with one injected typo.
        let names: [&[u8]; 8] = [
            b"SOLGALOV",
            b"IVANOVA",
            b"PETROV",
            b"SIDOROV",
            b"KUZNETSOVA",
            b"VOLKOVA",
            b"POPOV",
            b"SMIRNOV",
        ];
        let mut text = Vec::with_capacity(256);
        for n in &names {
            text.extend_from_slice(n);
            text.push(b'^');
        }
        // One-error variant: "SOLDALOV" is one substitution from "SOLGALOV".
        let pat_typo = b"SOLDALOV";
        group.bench_function("match1err_8x14char", |b| {
            b.iter(|| black_box(bitap_match(black_box(&text), black_box(pat_typo), 1)));
        });
        group.finish();
    }

    let _ = std::fs::remove_dir_all(&dir);
}

criterion_group!(benches, index_benchmarks);
criterion_main!(benches);
