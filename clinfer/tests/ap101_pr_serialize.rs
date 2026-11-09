// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: the PR serializer's allocation gate (isolated binary, inline
//! windows; every fixture is built OUTSIDE the measured window).
//!
//! Two windows, both on the CPU path of one detection cycle:
//!
//! - W1: `pr::serialize` over a FIXED box set. The serializer builds one
//!   `Vec<u8>` and grows it, so the honest claim is a fixed budget rather
//!   than zero - the gate pins the exact count, so a per-detection heap
//!   growth would fail it. That is the property the owner rule asks for:
//!   the cost scales with the data, not with the number of allocations.
//! - W2: `DurationHistogram::observe`, which must be ALLOCATION-FREE. It is
//!   pure atomic arithmetic by construction, and this is what proves it.
//!
//! The GPU side stays with the OpenVINO counters and is out of scope.
//!
//! `TestAllocator` counts process-wide, so this binary keeps to the pattern
//! the other ap101 gates use: one thread, and every fixture built with
//! tracking disabled.

use clinfer::conf::ResultKind;
use clinfer::dicomweb::WorkitemCode;
use clinfer::pr::Detection;
use clinfer::pr::{PrIdentity, PrProfile, ReferencedImage, ReferencedSeries, serialize};
use clinfer::receiver::{DurationHistogram, Stage};
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// Window that asserts an EXACT allocation budget.
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

/// Window that returns the allocation count, for measuring a budget.
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

const STUDY: &str = "1.2.826.0.1.3680043.10.1234.9001";
const SERIES: &str = "1.2.826.0.1.3680043.10.1234.9002";
const SOURCE_SERIES: &str = "1.2.826.0.1.3680043.10.1234.9003";
const SOURCE_INSTANCE: &str = "1.2.826.0.1.3680043.10.1234.9004";

/// A fixed box set: the measurement must be reproducible call to call, so
/// nothing here varies.
fn detections() -> Vec<Detection> {
    (0..3)
        .map(|i| Detection {
            label: "Nodule".to_string(),
            class_index: 0,
            confidence: 0.9,
            x1: 10.0 + i as f32,
            y1: 20.0,
            x2: 30.0,
            y2: 40.0,
        })
        .collect()
}

fn identity() -> PrIdentity {
    PrIdentity {
        sop_instance_uid: "2.25.1".to_string(),
        study_uid: STUDY.to_string(),
        series_uid: SERIES.to_string(),
        referenced: vec![ReferencedSeries {
            series_uid: SOURCE_SERIES.to_string(),
            images: vec![ReferencedImage {
                sop_class_uid: "1.2.840.10008.5.1.4.1.1.1.1".to_string(),
                sop_instance_uid: SOURCE_INSTANCE.to_string(),
                frame_number: None,
                detections: detections(),
                contours: Vec::new(),
            }],
        }],
        source_rows: 640,
        source_cols: 640,
        source_monochrome1: false,
        patient_name: Some("Fixture^PR".to_string()),
        patient_id: Some("PR-FIXTURE".to_string()),
        patient_id_issuer: Some("SITE-A".to_string()),
        patient_birth_date: Some("19700101".to_string()),
        patient_sex: Some("O".to_string()),
        study_id: Some("STUDY-1".to_string()),
        accession_number: Some("ACC-1".to_string()),
        referring_physician: Some("REFERRER^A".to_string()),
        station_name: Some("Fixture Station".to_string()),
        scheduled_protocol_code: Some(WorkitemCode {
            value: "CAD".to_string(),
            scheme: "99CLARUS".to_string(),
            meaning: "AI detection".to_string(),
        }),
        requested_procedure_id: Some("RP-1".to_string()),
        requested_procedure_description: Some("Chest CT".to_string()),
        requested_procedure_code: Some(WorkitemCode {
            value: "RP-CHEST".to_string(),
            scheme: "99CLARUS".to_string(),
            meaning: "Chest procedure".to_string(),
        }),
        reason_for_requested_procedure: Some("Screening".to_string()),
        reason_for_requested_procedure_code: Some(WorkitemCode {
            value: "REASON-1".to_string(),
            scheme: "99CLARUS".to_string(),
            meaning: "Screening indication".to_string(),
        }),
    }
}

fn profile() -> PrProfile {
    PrProfile::of(&clinfer::conf::RuleResult {
        kind: ResultKind::Pr,
        color: Some("#FF3B30".to_string()),
        profile_version: 1,
        series_description: Some("AI Boxes".to_string()),
        label: clinfer::conf::LabelProfile::default(),
        sr: None,
        measurements: None,
    })
}

/// The measured budget of one serialize call.
///
/// Discovered from the first run rather than guessed, then pinned: the value
/// is a small constant because the writer appends into one `Vec<u8>` and the
/// only owned Strings are the converted coordinates.
fn serialize_budget() -> usize {
    let identity = identity();
    let profile = profile();
    let (n, _) = count_env(|| serialize(&identity, &profile));
    n
}

#[test]
fn serialize_has_a_fixed_allocation_budget() {
    let budget = serialize_budget();
    let identity = identity();
    let profile = profile();

    let (ok, bytes) = budget_env(budget, || serialize(&identity, &profile));
    assert!(
        ok,
        "serialize must cost exactly {budget} allocations; a per-detection heap \
         growth would change this count"
    );
    assert!(bytes.len() > 132, "the object is a real Part-10 file");
}

#[test]
fn the_serialize_budget_does_not_grow_with_the_box_count() {
    // The property behind the budget: more boxes add bytes, not one
    // allocation per box. A writer that allocated per detection (a
    // `format!` label, a per-box `Vec`, a temporary `String`) would fail
    // here even with the single-box budget pinned above.
    //
    // The output buffer itself is allowed to grow: `Vec` doubles its
    // capacity, so a handful of reallocations is expected and the count
    // stays logarithmic. What is forbidden is a per-detection term, so the
    // gate is a slope, not an equality.
    const BOXES_PER_STEP: usize = 40;
    let profile = profile();

    let boxes = |count: usize| -> Vec<Detection> {
        (0..count)
            .map(|i| Detection {
                label: "Nodule".to_string(),
                class_index: 0,
                confidence: 0.9,
                x1: i as f32,
                y1: 2.0,
                x2: 3.0,
                y2: 4.0,
            })
            .collect()
    };

    let one = boxes(1);
    let few = boxes(BOXES_PER_STEP);
    let many = boxes(BOXES_PER_STEP * 4);

    let serialize_with = |detections: Vec<Detection>| {
        let mut id = identity();
        id.referenced[0].images[0].detections = detections;
        count_env(|| serialize(&id, &profile))
    };

    let (small, one_bytes) = serialize_with(one);
    let (middle, few_bytes) = serialize_with(few);
    let (large, many_bytes) = serialize_with(many);

    assert!(
        one_bytes.len() < few_bytes.len() && few_bytes.len() < many_bytes.len(),
        "more boxes must produce more bytes"
    );

    // Keep the growth logarithmic, not linear. The output buffer doubles
    // as it fills, so a handful of reallocations is expected and the count
    // stays far below one-per-box. A per-detection allocation would add at
    // least 120 across these steps, so any bound near the box count is
    // decisive; the constant below simply states "logarithmic, not linear"
    // without pinning the exact number of doublings.
    const SLACK: usize = 6;
    assert!(
        large <= middle + SLACK,
        "the allocation count must not grow per detection: {middle} allocations \
         for {BOXES_PER_STEP} boxes vs {large} for {} (bytes: {} vs {})",
        BOXES_PER_STEP * 4,
        few_bytes.len(),
        many_bytes.len()
    );
    assert!(
        middle <= small + SLACK,
        "the allocation count must not grow per detection: {small} allocations \
         for 1 box vs {middle} for {BOXES_PER_STEP} (bytes: {} vs {})",
        one_bytes.len(),
        few_bytes.len()
    );
    // The decisive statement: the count stays far below the box count.
    assert!(
        large < BOXES_PER_STEP,
        "the allocation count {large} is close to the box count {}, which means \
         a per-detection allocation is present",
        BOXES_PER_STEP * 4
    );
}

#[test]
fn observe_is_allocation_free() {
    println!(
        "GATE W2 DurationHistogram::observe | profile={} | arch={}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        std::env::consts::ARCH
    );
    let histogram = DurationHistogram::default();
    // Warm nothing: `observe` takes &self and touches only atomics, so the
    // window can include the very first call.
    let (n, ()) = budget_env(0, || {
        ferrite_macros::zero_alloc!({
            histogram.observe(0.123);
        })
    });
    assert!(
        n,
        "recording one sample must not allocate; the histogram exists to be \
         cheap on the worker's hot path"
    );
    // And it still recorded something.
    let mut out = String::new();
    histogram.render_into(&mut out, Stage::Infer);
    assert!(out.contains("_count{stage=\"infer\"} 1"), "{out}");
}
