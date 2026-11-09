// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Criterion benchmark for `bitap_match` against the 1000-name dictionary
//! (`tests/data/names.csv`).
//!
//! Each measured subject runs the whole dictionary through one fuzzymatch
//! query - the same shape as a `fuzzymatch=true` PatientName scan over all
//! candidates, which is the per-candidate cost QIDO pays in the hot path.
//!
//! Run: `cargo bench --bench fuzzymatch_bench`

use clarus_core::query::bitap::bitap_match;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use std::fs;

/// Load the `name` column from `path` (a CSV with a header row).
fn load_dictionary(path: &str) -> Vec<String> {
    fs::read_to_string(path)
        .expect("names.csv")
        .lines()
        .skip(1) // header
        .filter(|l| !l.is_empty())
        .map(|l| l.split(',').next().unwrap_or("").to_string())
        .collect()
}

fn bench_fuzzymatch(c: &mut Criterion) {
    let names = load_dictionary("tests/data/names.csv");
    assert_eq!(names.len(), 1000, "dictionary must hold 1000 names");

    // One query per name category: exact, typo, prefix, 1-char, separator.
    // The typo subject must differ from its exact twin in exactly one respect,
    // and that respect has to be the typo itself. `Иванов` was measured twice:
    // once labelled "exact" and once labelled "typo substitution" with the
    // correction written into the LABEL instead of into the query, so the two
    // rows differed by nothing and the bench reported the same number twice -
    // a comparison that cannot show what a typo costs.
    let queries = vec![
        ("Иванов", "exact cyrillic"),
        ("Ивонов", "typo substitution о→а"),
        ("Иван", "prefix many matches"),
        ("Smith", "exact latin"),
        ("Smyth", "typo latin"),
        ("张", "cjk 1-char unigram"),
        ("Li", "short name"),
        ("Иванов^Иван", "DICOM PN separator"),
    ];

    for (query, desc) in &queries {
        c.bench_function(&format!("fuzzymatch/{desc}"), |b| {
            b.iter(|| {
                names
                    .iter()
                    .filter(|name| bitap_match(black_box(query), name))
                    .count()
            });
        });
    }
}

criterion_group!(benches, bench_fuzzymatch);
criterion_main!(benches);
