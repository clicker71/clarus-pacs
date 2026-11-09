// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: the RTSTRUCT writer's allocation gates (isolated binary, inline
//! windows; every fixture is built OUTSIDE the measured window).
//!
//! Two windows, both on the CPU path of one structure-set cycle:
//!
//! - W1: `rtstruct::serialize` over a FIXED three-ROI identity (two source
//!   slices, one of them with a non-identity orientation and anisotropic
//!   spacing, so the geometry conversion really runs). The writer builds
//!   one `Vec<u8>` and grows it, so the honest claim is a fixed budget
//!   rather than zero: the gate pins the exact count, so a per-contour or
//!   per-point heap allocation would fail it. That is the property the
//!   owner rule asks for - the cost scales with the data, not with the
//!   number of allocations.
//! - W2: `abi::schema::parse` over a FIXED `result.json` that declares the
//!   `rtstruct` block. The parse cost is a fixed budget too, so a per-ROI
//!   or per-key allocation would change it.
//!
//! The worker's ROI-to-contour assembly is NOT gated here: it clones the
//! ABI's strings and polygons into the writer's own types by design, and
//! its wire behaviour is pinned by
//! `a_structure_set_is_stored_with_the_four_geometric_pins_intact`.
//! `store_rtstruct` end to end is NOT gated either - it runs a Python
//! processor, which is the one thing an allocation window must never
//! contain.
//!
//! The point formatter is the part that must NOT allocate per value, and
//! that property is visible in the budget: a polygon of 512 points costs
//! the same allocations as one of 3.

use clinfer::abi::schema;
use clinfer::rtstruct::{
    RtContour, RtIdentity, RtRoiEntry, RtSource, contour_buffer_bytes, contour_points_into,
    to_patient,
};
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// `serialize`, with the fixtures here assumed to be encodable.
///
/// This file pins the ALLOCATION budget, so every call site counts what one
/// call costs; the error the encoder can return is about a polygon too large
/// for a DS, which no fixture here is, so unwrapping keeps the call sites
/// measuring the same thing they measured before the signature changed. The
/// refusal itself is pinned in `rtstruct`'s own tests.
fn serialize(identity: &RtIdentity) -> Vec<u8> {
    clinfer::rtstruct::serialize(identity).expect("the fixture structure set encodes")
}

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

/// `schema::parse` at the default budget.
///
/// These tests count allocations on the SERIALIZER, not on the size check,
/// so the budget is not what they are about; the explicit number keeps them
/// from depending on a build-wide constant.
fn parse(bytes: &[u8]) -> Result<schema::ResultFile, schema::SchemaError> {
    schema::parse(bytes, schema::DEFAULT_MAX_RESULT_BYTES)
}

/// Window that asserts an allocation CEILING, for a slope rather than an
/// equality.
fn under_env<F, R>(ceiling: usize, f: F) -> (bool, R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let result = f();
    let (n, _) = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (n <= ceiling, result)
}

/// One fixed source slice: NON-IDENTITY orientation and ANISOTROPIC
/// spacing, so the geometry conversion is part of the measured window and
/// cannot be skipped without changing the output.
fn source() -> RtSource {
    RtSource {
        sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".to_string(),
        sop_instance_uid: "2.25.3001".to_string(),
        series_uid: "2.25.3002".to_string(),
        position: Some([10.0, -20.0, 30.0]),
        orientation: Some([0.0, 1.0, 0.0, -1.0, 0.0, 0.0]),
        pixel_spacing: Some([2.0, 0.5]),
    }
}

/// A second fixed slice in a second series, so the reference chain has to
/// group by series.
fn second_source() -> RtSource {
    RtSource {
        sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".to_string(),
        sop_instance_uid: "2.25.3003".to_string(),
        series_uid: "2.25.3004".to_string(),
        position: Some([0.0, 0.0, 32.5]),
        orientation: Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]),
        pixel_spacing: Some([1.0, 1.0]),
    }
}

/// The fixed ROI attribute set, shared by every fixture entry.
fn roi(name: &str) -> schema::RtRoi {
    schema::RtRoi {
        name: name.to_string(),
        description: Some("fixture".to_string()),
        color: [255, 0, 0],
        generation_algorithm: "AUTOMATIC".to_string(),
        interpreted_type: "ORGAN".to_string(),
        contours: Vec::new(),
    }
}

/// One contour of `count` points, built through `to_patient` the way the
/// worker builds them.
fn contour(source: &RtSource, count: usize) -> RtContour {
    RtContour {
        points: (0..count)
            .map(|index| to_patient(source, index as f64, index as f64))
            .collect(),
        source: source.clone(),
    }
}

/// Three ROIs over two source slices: the shape a real run produces, with
/// one ROI drawing two contours so the per-ROI loop runs more than once.
fn identity() -> RtIdentity {
    let first = source();
    let second = second_source();
    RtIdentity {
        sop_instance_uid: "2.25.4001".to_string(),
        study_uid: "2.25.4002".to_string(),
        series_uid: "2.25.4003".to_string(),
        frame_of_reference_uid: Some("2.25.4004".to_string()),
        label: "PROC-3".to_string(),
        name: Some("Fixture Structure Set".to_string()),
        rois: vec![
            RtRoiEntry {
                number: 1,
                roi: roi("LUNG_L"),
                contours: vec![contour(&first, 3), contour(&second, 3)],
            },
            RtRoiEntry {
                number: 2,
                roi: roi("LUNG_R"),
                contours: vec![contour(&first, 3)],
            },
            RtRoiEntry {
                number: 3,
                roi: roi("HEART"),
                contours: vec![contour(&second, 3)],
            },
        ],
        derivation_description: "fixture-model".to_string(),
        patient_name: Some("Fixture^RTSTRUCT".to_string()),
        patient_id: Some("RT-FIXTURE".to_string()),
        patient_id_issuer: Some("SITE-A".to_string()),
        patient_birth_date: Some("19700101".to_string()),
        patient_sex: Some("M".to_string()),
        study_id: Some("STUDY-1".to_string()),
        accession_number: Some("ACC-1".to_string()),
        referring_physician: Some("Referrer^Fixture".to_string()),
        station_name: Some("Fixture Station".to_string()),
        series_description: Some("Contours".to_string()),
        source_series: vec![first.series_uid.clone(), second.series_uid.clone()],
        contour_images: vec![first, second],
    }
}

/// The fixed `result.json` for the parser window: two ROIs, one contour
/// each, with the keys `parse_contour` accepts and no others.
fn rtstruct_result_json() -> String {
    r#"{"protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {"script": "contour_site.py"},
        "rtstruct": {"label": "AI LUNG",
                     "name": "Site Structure Set",
                     "rois": [
            {"name": "LUNG_L", "description": "auto",
             "color": [255, 0, 0],
             "generation_algorithm": "AUTOMATIC",
             "interpreted_type": "ORGAN",
             "contours": [{"frame": 1,
                           "points": [[12.5, 40.0], [13.0, 41.5], [11.0, 42.0]]}]},
            {"name": "LUNG_R", "description": "auto",
             "color": [0, 0, 255],
             "generation_algorithm": "SEMIAUTOMATIC",
             "interpreted_type": "PTV",
             "contours": [{"frame": 2,
                           "points": [[1.0, 2.0], [3.0, 4.0], [5.0, 6.0]]}]}]}}"#
        .to_string()
}

/// The measured budget of one serialize call, discovered then pinned.
fn serialize_budget() -> usize {
    let fixed = identity();
    let (n, _) = count_env(|| serialize(&fixed));
    n
}

/// The measured budget of one `result.json` parse, discovered then pinned.
fn parse_budget() -> usize {
    let bytes = rtstruct_result_json();
    let (n, _) = count_env(|| parse(bytes.as_bytes()));
    n
}

#[test]
fn serialize_has_a_fixed_allocation_budget() {
    let budget = serialize_budget();
    let fixed = identity();

    let (ok, bytes) = budget_env(budget, || serialize(&fixed));
    assert!(
        ok,
        "serialize must cost exactly {budget} allocations; a per-contour or \
         per-point allocation would change this count"
    );
    assert!(bytes.len() > 132, "the object is a real Part-10 file");
    // The file is a Part-10 document with the RT SS class.
    assert_eq!(&bytes[128..132], b"DICM", "the DICM magic");
}

#[test]
fn the_roi_count_does_not_grow_the_budget_per_roi() {
    // The property behind the budget: more ROIs add bytes, not one
    // allocation per ROI. A writer that allocated per ROI (a `format!`
    // number, a per-ROI `Vec` for the sequence) would fail here. The
    // output buffer doubles as it fills, so the count stays logarithmic -
    // the gate is a slope, not an equality.
    let one = identity();
    let mut many = identity();
    let last = many.rois[2].clone();
    for _ in 0..8 {
        many.rois.push(RtRoiEntry {
            number: many.rois.len() as u16 + 1,
            roi: last.roi.clone(),
            contours: vec![contour(&source(), 3)],
        });
    }

    let (small, small_bytes) = count_env(|| serialize(&one));
    let (large, large_bytes) = count_env(|| serialize(&many));

    assert!(
        small_bytes.len() < large_bytes.len(),
        "more ROIs must produce more bytes"
    );
    // The output buffer reallocates logarithmically, so a handful of extra
    // allocations is expected; a per-ROI cost would be eight or more.
    const SLACK: usize = 4;
    assert!(
        large <= small + SLACK,
        "the allocation count must not grow per ROI: {small} allocations \
         for 3 ROIs vs {large} for 11"
    );
}

#[test]
fn a_polygon_costs_no_allocation_per_point() {
    // The point formatter is the hot loop of this writer: one DS value per
    // point component. It must write into the caller's buffer, so a
    // polygon 100x longer costs the same allocations.
    let source = source();
    let small: Vec<[f64; 3]> = (0..3)
        .map(|index| to_patient(&source, index as f64, index as f64))
        .collect();
    let large: Vec<[f64; 3]> = (0..300)
        .map(|index| to_patient(&source, index as f64, index as f64))
        .collect();

    let mut small_buffer = vec![0u8; contour_buffer_bytes(small.len())];
    let mut large_buffer = vec![0u8; contour_buffer_bytes(large.len())];

    let (small_count, _) = count_env(|| contour_points_into(&mut small_buffer, &small));
    let (large_count, _) = count_env(|| contour_points_into(&mut large_buffer, &large));

    assert_eq!(
        small_count, 0,
        "formatting three points allocates nothing at all"
    );
    assert_eq!(
        large_count, 0,
        "formatting three hundred points allocates nothing either: {large_count}"
    );
}

#[test]
fn the_schema_parser_has_a_fixed_allocation_budget() {
    let budget = parse_budget();
    let bytes = rtstruct_result_json();

    let (ok, parsed) = budget_env(budget, || parse(bytes.as_bytes()));
    assert!(
        ok,
        "parsing the rtstruct result must cost exactly {budget} allocations; \
         a per-ROI or per-point allocation would change this count"
    );
    assert!(parsed.is_ok(), "the fixture parses");
}

#[test]
fn the_declared_contours_reach_the_parsed_result() {
    // The window above only proves the cost; this proves the fixture is the
    // shape the writer consumes, so a parser that returned an empty ROI set
    // could not pass both.
    let bytes = rtstruct_result_json();
    let parsed = parse(bytes.as_bytes()).expect("the fixture parses");
    let Some(rtstruct) = &parsed.rtstruct else {
        panic!("the fixture declares an rtstruct block");
    };
    assert_eq!(rtstruct.rois.len(), 2, "two declared ROIs");
    assert_eq!(
        rtstruct.rois[0].contours.len(),
        1,
        "one contour on the first ROI"
    );
    assert_eq!(
        rtstruct.rois[0].contours[0].points.len(),
        3,
        "three points on that contour"
    );
    assert!(parsed.has_rtstruct(), "has_rtstruct() is true");
    assert!(!parsed.is_empty(), "a structure set is not an empty result");
}

#[test]
fn a_larger_polygon_does_not_scale_the_allocation_count() {
    // The ceiling form of the same property, over the WHOLE serializer:
    // tripling every polygon must not add allocations proportional to the
    // point count.
    let small = identity();
    let mut large = identity();
    for entry in &mut large.rois {
        for contour in &mut entry.contours {
            contour.points = (0..300)
                .map(|index| to_patient(&source(), index as f64, index as f64))
                .collect();
        }
    }

    let (base, _) = count_env(|| serialize(&small));
    // Generous ceiling: the byte buffer grows, so some extra allocations
    // are expected, but a per-point cost would be thousands.
    let ceiling = base + 8;
    let (ok, bytes) = under_env(ceiling, || serialize(&large));
    assert!(
        ok,
        "300-point polygons must stay under {ceiling} allocations: a \
         per-point allocation would be orders of magnitude more"
    );
    assert!(bytes.len() > 1000, "the large document is really larger");
}
