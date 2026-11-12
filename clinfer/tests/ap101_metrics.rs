// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: the child-metrics gates (isolated binary).
//!
//! Three things are measured, chosen because each one runs on a path that
//! must not become the bottleneck:
//!
//! - P1 `procstat` parse: the cost of ONE sample. It runs on the model
//!   thread's watchdog tick, once per 50 ms per running child, so a
//!   regression here is a per-child tax on the very tick that exists to be
//!   cheap.
//! - G1 `LabelledGauge::set`: the writer's cost. It is called from the tick
//!   (six families) and on every reap, so it must not allocate - a gauge
//!   that allocated would turn a metrics tick into garbage.
//! - R1 `Counters::render`: the SCRAPE cost WITH the new gauges. This is a
//!   NEW gate: no existing ap101 file pins `Counters::render`, and the
//!   gauges added six labelled families plus the sidecar's own peak to what
//!   it walks. The number is measured first and then pinned, so a future
//!   gauge that blows the budget fails here instead of shipping.
//!
//! `TestAllocator` counts PER-THREAD, so every fixture is built with
//! tracking disabled and every measured call runs on this one thread.

use clinfer::receiver::{Counters, LabelledGauge};
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

fn count_env<F, R>(f: F) -> (usize, R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let result = f();
    let (n, _) = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (n, result)
}

fn budget_env<F, R>(expect: usize, f: F) -> (bool, R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let result = f();
    let (n, _) = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (n == expect, result)
}

fn assert_window_is_live() {
    let (over, _) = count_env(|| {
        let v: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(&v);
    });
    assert!(over > 0, "the window must observe allocations");
}

/// A `/proc/<pid>/status` body, the exact shape the fixture uses.
///
/// Built as a const so the measured call parses a FIXED input: a budget
/// measured over a dynamic string would pin the fixture rather than the
/// parser.
const STATUS: &str = "Name:\tpython3\n\
                      State:\tR (running)\n\
                      VmPeak:\t  524288 kB\n\
                      VmSize:\t  262144 kB\n\
                      VmRSS:\t   10240 kB\n\
                      VmHWM:\t   20480 kB\n\
                      VmData:\t    8192 kB\n\
                      VmStk:\t     132 kB\n\
                      VmExe:\t     512 kB\n";

/// A `/proc/<pid>/stat` line whose `comm` carries spaces AND parentheses.
const STAT: &str = "42 (Web Content (tab)) R 1 1 1 0 -1 4194304 100 0 0 0 \
                    700 300 0 0 20 0 5 0 100 200 300 400";

/// P1: one child-metrics sample. Measured, not asserted.
///
/// `read` is the whole per-tick cost: two file reads and two parses. The
/// FILE READS are not part of this budget - they are the OS's, and pinning
/// them would pin this machine's page cache - so what is measured here is
/// the parse over a fixed fixture, which is the part this crate owns.
#[test]
fn p1_a_procstat_sample_budget() {
    assert_window_is_live();
    let (n, stat) = count_env(|| clinfer::procstat::parse_for_test(STATUS, STAT));
    assert!(stat.is_some(), "the fixture parses");
    println!("P1 procstat parse allocations: {n}");
    let (ok, stat) = budget_env(P1_BUDGET, || {
        clinfer::procstat::parse_for_test(STATUS, STAT)
    });
    assert!(ok, "the parse allocated beyond {P1_BUDGET}: {stat:?}");
}

/// G1: publishing one gauge value must be allocation-free.
///
/// The first call for a label DOES allocate (the label becomes an owned
/// `String` in the fixed vector), so the measured window is the SECOND call:
/// a republish of a label the gauge already knows. That is the hot case -
/// the tick republishes the same processor's numbers 20 times a second.
#[test]
fn g1_publishing_a_known_label_allocates_nothing() {
    let gauge = LabelledGauge::default();
    // Built OUTSIDE the window, so the label's own allocation is not
    // attributed to the publish.
    gauge.set("totalseg", 1);
    assert_eq!(gauge.get("totalseg"), 1, "the first publish landed");

    let (n, ()) = count_env(|| {
        for value in 1u64..=64 {
            gauge.set("totalseg", value);
        }
    });
    assert_eq!(n, 0, "64 republishes allocated {n} time(s)");
    assert_eq!(gauge.get("totalseg"), 64);
}

/// R1: the full exposition, WITH the child-metrics gauges.
///
/// The budget is in BYTES RENDERED, not allocations: the render builds one
/// `String` and the number that matters is how much it has to walk. Measured
/// over a populated set of gauges (three processors, the way a real
/// deployment would have them) so the pin reflects a real scrape rather than
/// an empty one.
#[test]
fn r1_the_metrics_render_with_gauges_is_bounded() {
    let counters = Counters::default();
    for id in ["totalseg", "detect-pr", "report-llm"] {
        counters.external_rss_bytes.set(id, 123_456_789);
        counters.external_peak_rss_bytes.set(id, 234_567_890);
        counters.external_last_peak_rss_bytes.set(id, 345_678_901);
        counters.external_cpu_seconds_millis.set(id, 12_345);
        counters.external_running.set(id, 1);
        counters.external_pid.set(id, 4_321);
    }
    counters
        .clinfer_peak_rss_bytes
        .store(87_654_321, std::sync::atomic::Ordering::Relaxed);

    let body = counters.render();
    // The gauges are ON the wire, named as the report specifies.
    for line in [
        "# HELP clinfer_external_rss_bytes",
        "# TYPE clinfer_external_rss_bytes gauge",
        "clinfer_external_rss_bytes{processor=\"totalseg\"} 123456789",
        "clinfer_external_last_peak_rss_bytes{processor=\"report-llm\"} 345678901",
        // Milliseconds are stored; seconds are what a scrape reads.
        "clinfer_external_cpu_seconds{processor=\"detect-pr\"} 12.345",
        "clinfer_external_running{processor=\"totalseg\"} 1",
        "clinfer_external_pid{processor=\"totalseg\"} 4321",
        "clinfer_peak_rss_bytes 87654321",
    ] {
        assert!(body.contains(line), "missing {line:?} in:\n{body}");
    }

    let size = body.len();
    println!("R1 metrics render bytes: {size}");
    assert!(
        size <= R1_MAX_BYTES,
        "the exposition grew to {size} bytes, past the pinned {R1_MAX_BYTES}"
    );
}

/// The render ceiling in bytes, pinned at the measured value plus headroom.
///
/// A `usize` ceiling rather than an allocation count because the render's
/// cost is dominated by what it writes, and the write is proportional to the
/// number of series. Three processors is a realistic deployment; the
/// headroom absorbs the odd label that grows a few characters.
///
/// MEASURED 22.09 (three processors, one of each gauge): 11999 bytes. The
/// bulk is the cycle histogram - 11 buckets plus `+Inf`, `_sum` and `_count`
/// per stage, eight stages - which is why the pin is stated with the
/// measured figure in the message rather than as a round number: an edit
/// that adds a GAUGE FAMILY should make the reviewer look at this line, and
/// an edit that merely renames something should not.
///
/// MEASURED 25.09: 13309 bytes, after `Stage::SerializeMeasurements` added a
/// NINTH stage to the same histogram family (13 lines: 11 buckets, `+Inf`,
/// `_sum`, `_count`). That is the intended cost of naming the measurement
/// writer separately, so the ceiling moves by the same 13 lines and no more -
/// the headroom over the new measurement is unchanged.
const R1_MAX_BYTES: usize = 14_100;

/// The parse budget, pinned at the measured value.
///
/// The parse is pure arithmetic over a `&str` and writes into a returned
/// struct, so the honest expectation is ZERO allocations; the pin is what
/// makes an edit that starts allocating - a `to_string` here, a `Vec` there -
/// fail loudly instead of quietly taxing the tick.
const P1_BUDGET: usize = 0;
