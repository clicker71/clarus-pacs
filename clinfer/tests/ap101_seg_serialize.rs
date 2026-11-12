// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: the SEG writer's allocation gates (isolated binary, inline
//! windows; every fixture is built OUTSIDE the measured window).
//!
//! Two windows, both on the CPU path of one segmentation cycle:
//!
//! - W1: `seg::serialize` over a FIXED two-segment identity (one source
//!   image, one frame per segment - the C.8.20.3.1 shape). The writer
//!   builds one `Vec<u8>` and grows it, so the honest claim is a fixed
//!   budget rather than zero: the gate pins the exact count, so a
//!   per-frame heap growth would fail it. That is the property the owner
//!   rule asks for: the cost scales with the data, not with the number
//!   of allocations.
//! - W2: `abi::schema::parse` over a FIXED `result.json` that declares
//!   two segments with `"overlap": "YES"` (the 20.09 parser branch). The
//!   parse cost is a fixed budget too, so a per-key or per-segment
//!   allocation would fail it.
//!
//! The worker's per-segment plane split is not gated here: it is one
//! `to_vec` per (frame, segment) by design, and its wire behaviour is
//! pinned by `a_two_segment_mask_carries_both_bit_planes` instead.
//!
//! `TestAllocator` counts process-wide, so this binary keeps to the
//! pattern the other ap101 gates use: one thread, and every fixture
//! built with tracking disabled.

use clinfer::abi::schema;
use clinfer::dicomweb::WorkitemCode;
use clinfer::pixels::Geometry;
use clinfer::seg::{SegFrame, SegIdentity, SegSource, serialize};
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

/// `schema::parse` at the default budget.
///
/// These tests count allocations on the SERIALIZER, not on the size check,
/// so the budget is not what they are about; the explicit number keeps them
/// from depending on a build-wide constant.
fn parse(bytes: &[u8]) -> Result<schema::ResultFile, schema::SchemaError> {
    schema::parse(bytes, schema::DEFAULT_MAX_RESULT_BYTES)
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

/// One fixed source image: the measurement must be reproducible call to
/// call, so nothing here varies.
fn source() -> SegSource {
    SegSource {
        sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".to_string(),
        sop_instance_uid: "2.25.1001".to_string(),
        series_uid: "2.25.1002".to_string(),
        position: Some([1.5, 2.5, 0.0]),
        orientation: Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]),
    }
}

fn geometry() -> Geometry {
    Geometry {
        rows: 2,
        cols: 2,
        samples: 1,
        bits_allocated: 1,
        bits_stored: 1,
        frames: 1,
        inverted: false,
        signed: false,
    }
}

/// Two segments over one source image: two frames, one bit plane each
/// (C.8.20.3.1), nested masks declared as overlapping.
fn two_segment_identity() -> SegIdentity {
    let code = schema::Code {
        code: "M-01000".to_string(),
        scheme: "SRT".to_string(),
        meaning: "Morphologically Altered Structure".to_string(),
    };
    SegIdentity {
        sop_instance_uid: "2.25.2001".to_string(),
        study_uid: "2.25.2002".to_string(),
        series_uid: "2.25.2003".to_string(),
        frame_of_reference_uid: Some("2.25.2004".to_string()),
        source_geometry: geometry(),
        pixel_spacing: Some([0.7, 0.7]),
        slice_spacing: Some(2.5),
        algorithm_name: "threshold".to_string(),
        content_label: "Masks".to_string(),
        segments: vec![
            schema::Segment {
                label: "BONE_CORE".to_string(),
                category: code.clone(),
                property_type: code.clone(),
                color: Some([34891, 53479, 50166]),
                algorithm: "AUTOMATIC".to_string(),
            },
            schema::Segment {
                label: "BONE_ALL".to_string(),
                category: code.clone(),
                property_type: code,
                color: Some([57500, 10748, 54273]),
                algorithm: "AUTOMATIC".to_string(),
            },
        ],
        segments_overlap: Some("YES".to_string()),
        frames: vec![
            SegFrame {
                segment_number: 1,
                mask: vec![0b1010_0000, 0b0101_0000],
                source: source(),
            },
            SegFrame {
                segment_number: 2,
                mask: vec![0b0000_0011, 0b0000_1100],
                source: source(),
            },
        ],
        patient_name: Some("Fixture^SEG".to_string()),
        patient_id: Some("SEG-FIXTURE".to_string()),
        patient_id_issuer: Some("SITE-A".to_string()),
        study_id: Some("STUDY-1".to_string()),
        accession_number: Some("ACC-1".to_string()),
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
        series_description: Some("Masks".to_string()),
    }
}

/// The fixed `result.json` for the parser window: two segments, one mask
/// file, and the 20.09 `"overlap": "YES"` branch.
fn seg_result_json() -> String {
    r#"{"protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {"script": "threshold_seg_site.py"},
        "seg": {"segments": [
            {"label": "BONE_CORE",
             "category": {"code": "M-01000", "scheme": "SRT",
                          "meaning": "Morphologically Altered Structure"},
             "type": {"code": "M-01000", "scheme": "SRT",
                      "meaning": "Morphologically Altered Structure"},
             "color": [34891, 53479, 50166]},
            {"label": "BONE_ALL",
             "category": {"code": "M-01000", "scheme": "SRT",
                          "meaning": "Morphologically Altered Structure"},
             "type": {"code": "M-01000", "scheme": "SRT",
                      "meaning": "Morphologically Altered Structure"},
             "color": [57500, 10748, 54273]}],
         "overlap": "YES",
         "files": [{"file": "mask-0000.raw", "frame": 1}]}}"#
        .to_string()
}

/// The measured budget of one serialize call, discovered then pinned.
fn serialize_budget() -> usize {
    let identity = two_segment_identity();
    let (n, _) = count_env(|| serialize(&identity));
    n
}

/// The measured budget of one `result.json` parse, discovered then pinned.
fn parse_budget() -> usize {
    let bytes = seg_result_json();
    let (n, _) = count_env(|| parse(bytes.as_bytes()));
    n
}

#[test]
fn serialize_has_a_fixed_allocation_budget() {
    let budget = serialize_budget();
    let identity = two_segment_identity();

    let (ok, bytes) = budget_env(budget, || serialize(&identity));
    assert!(
        ok,
        "serialize must cost exactly {budget} allocations; a per-frame heap \
         growth would change this count"
    );
    assert!(bytes.len() > 132, "the object is a real Part-10 file");
}

#[test]
fn the_segment_count_does_not_grow_the_budget_per_frame() {
    // The property behind the budget: more segments add bytes, not one
    // allocation per frame. A writer that allocated per frame (a
    // `format!` label, a per-frame `Vec`) would fail here. The output
    // buffer doubles as it fills, so the count stays logarithmic - the
    // gate is a slope, not an equality.
    let one = two_segment_identity();
    let mut many = two_segment_identity();
    // Same source, eight more segments: 10 frames in total.
    let last = many.segments[1].clone();
    for _ in 0..8 {
        many.segments.push(last.clone());
        many.frames.push(SegFrame {
            segment_number: 9,
            mask: vec![0b1111_0000, 0b0000_1111],
            source: source(),
        });
    }

    let (small, small_bytes) = count_env(|| serialize(&one));
    let (large, large_bytes) = count_env(|| serialize(&many));

    assert!(
        small_bytes.len() < large_bytes.len(),
        "more segments must produce more bytes"
    );
    const SLACK: usize = 6;
    assert!(
        large <= small + SLACK,
        "the allocation count must not grow per segment: {small} allocations \
         for 2 segments vs {large} for 10"
    );
}

#[test]
fn the_schema_parser_has_a_fixed_allocation_budget() {
    let budget = parse_budget();
    let bytes = seg_result_json();

    let (ok, parsed) = budget_env(budget, || parse(bytes.as_bytes()));
    assert!(
        ok,
        "parsing the seg result must cost exactly {budget} allocations; a \
         per-key or per-segment allocation would change this count"
    );
    assert!(parsed.is_ok(), "the fixture parses");
}

#[test]
fn the_overlap_declaration_reaches_the_seg_result() {
    let bytes = seg_result_json();
    let parsed = parse(bytes.as_bytes()).expect("the fixture parses");
    let Some(seg) = &parsed.seg else {
        panic!("the fixture declares a seg block");
    };
    assert_eq!(
        seg.overlap.as_deref(),
        Some("YES"),
        "the producer's overlap declaration is parsed, not dropped"
    );
    assert_eq!(seg.segments.len(), 2, "two declared segments");
}
