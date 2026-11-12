// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Criterion benchmark for the wildcard walk's worst case.
//!
//! `SeriesDescription = *zzz` against text of `a` repeated: the leading `*`
//! has to swallow one more scalar per retry, so every position of the value is
//! visited and the per-step cost is paid once per byte. Any per-step work that
//! looks at the REST of the value therefore makes the whole match quadratic in
//! the value length - which is what a whole-tail `str::from_utf8` probe did.
//!
//! `SeriesDescription`, `PatientName`, `OperatorsName`, `ProtocolName`,
//! `StationName` and `Manufacturer` are all user-supplied and all reach this
//! walk, so this is the cost one QIDO request can impose on a core.
//!
//! Run: `cargo bench --bench glob_worst_case_bench`
//!
//! Expected shape after the window fix: time grows roughly LINEARLY with the
//! value length. Before it, the growth exponent measured about 1.73 between
//! 4 KiB and 16 KiB, extrapolating to tens of seconds for a 1 MiB value.
//!
//! The subject is `series::glob_match_bytes` - the walk the manifest matchers
//! actually use, whose per-step scalar read is the cost this pins. The
//! allocating `filter::glob_match` is a different matcher and would measure
//! nothing.

use clarus_core::query::glob_match_bytes as glob_match;
use criterion::{Criterion, black_box, criterion_group, criterion_main};

fn bench_glob_worst_case(c: &mut Criterion) {
    let mut group = c.benchmark_group("glob_worst_case");
    for n in [1024usize, 4096, 16_384] {
        // `a` repeated then a non-matching tail, so the star retries at every
        // position and the match fails only at the end.
        let mut text = "a".repeat(n);
        text.push('!');
        group.bench_with_input(
            criterion::BenchmarkId::from_parameter(n),
            &text,
            |b, text| {
                b.iter(|| {
                    assert!(
                        !glob_match(black_box("*zzz"), black_box(text)),
                        "the fixture must not match, so the walk runs to the end"
                    );
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_glob_worst_case);
criterion_main!(benches);
