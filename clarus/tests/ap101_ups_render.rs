// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: UPS workitem JSON render allocation gate (isolated binary).
//!
//! PS3.18 11.5 retrieve and 11.9 search both answer with the workitem JSON
//! this writer produces, so it runs on every UPS read - the hottest path the
//! UPS-RS surface has. The gate pins the cost of the RENDER itself, with the
//! output buffer built OUTSIDE the window: the question is not "is a response
//! body free" (it is not) but "does rendering into a caller-owned buffer cost
//! anything besides the bytes it copies".
//!
//! The render is byte-oriented on purpose - it appends to a `Vec<u8>` and
//! formats nothing - so the budget is 0 and stays 0. A change that started
//! building a `String` per attribute, or that formatted a tag instead of
//! copying its bytes, fails here.
//!
//! The Referenced Request Sequence (0040,A370) is the newest emitter (PS3.4
//! Table CC.2.5-3 carries the study and the accession there), and it is the
//! one this gate exercises: a workitem carrying BOTH values takes the longest
//! path through the writer.

use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// Window that returns the allocation count and the closure's result.
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

/// A workitem carrying every attribute the render can emit, so the window
/// covers the longest possible path through the writer - including the
/// Referenced Request Sequence with both nested elements.
fn full_workitem() -> clarus_ports::models::WorkItem {
    clarus_ports::models::WorkItem {
        id: Some(7),
        workitem_uid: "1.2.826.0.1.3680043.10.1234.8101".to_string(),
        workflow_state: "SCHEDULED".to_string(),
        station_aet: Some("CT_SCANNER1".to_string()),
        station_name: Some("CT_ROOM_3".to_string()),
        procedure_step_start_date: Some("20230515".to_string()),
        procedure_step_start_time: Some("103000.125".to_string()),
        patient_name: Some("ИВАНОВ^ИВАН".to_string()),
        patient_id: Some("PT-77".to_string()),
        accession_number: Some("ACC-77".to_string()),
        requesting_physician: Some("ПЕТРОВ^АЛЕКСЕЙ".to_string()),
        modality: Some("CT".to_string()),
        sps_priority: Some("HIGH".to_string()),
        transaction_uid: None,
        created_at: None,
        updated_at: None,
        step_label: Some("CAD".to_string()),
        input_readiness: Some("READY".to_string()),
        sps_start_datetime: Some("20230515T103000".to_string()),
        input_study_uid: Some("1.2.826.0.1.3680043.10.9999.1".to_string()),
        reason_for_cancellation: None,
        cancellation_datetime: None,
        patient_id_issuer: Some("SITE-A".to_string()),
        scheduled_workitem_code: Some(clarus_ports::models::WorkItemCode {
            value: "CT-ORGAN-SEG".to_string(),
            scheme: "99CLARUS".to_string(),
            meaning: "CT organ segmentation".to_string(),
        }),
        output_information: None,
        performed_workitem_code: None,
        performed_station_name_code: None,
        requested_procedure_id: Some("RP-1".to_string()),
        // The performed report is the deepest nesting the writer can emit, so
        // the fixture fills it: the gate has to cover the (0074,1216) item,
        // not just the shorter path that skips it.
        performed_start_datetime: Some("20230515T103000".to_string()),
        performed_end_datetime: Some("20230515T103412".to_string()),
        // A stored (0074,1210) element. The render must EXTEND the output
        // with these bytes, never re-escape them: `push_json_str` would
        // allocate a second copy, which the gate below forbids.
        scheduled_processing_parameters: Some(
            r#"{"Value":[{"0040A040":{"vr":"CS","Value":["TEXT"]},"0040A160":{"vr":"UT","Value":["window 400/40"]}}],"vr":"SQ"}"#.to_string(),
        ),
        reason_for_requested_procedure: Some("rule out pneumonia".to_string()),
    }
}

/// Rendering a full workitem into a caller-owned buffer allocates NOTHING.
///
/// The buffer is reserved OUTSIDE the window, so the gate measures the writer
/// and not the allocation the caller chose to make. That is the honest split:
/// a response body has to live somewhere, and where it lives is the caller's
/// decision; what this pins is that the writer itself is pure byte copying.
#[test]
fn rendering_a_workitem_json_allocates_nothing() {
    let wi = full_workitem();
    // Reserve the buffer before the window opens: growth would be a caller
    // cost, and the render is given enough room so it never grows.
    let mut out: Vec<u8> = Vec::with_capacity(2048);

    let (n, len) = count_env(|| {
        clarus_server::handlers::ups::json::write_workitem_json(&mut out, &wi);
        out.len()
    });

    assert!(
        len > 0,
        "the render produced bytes, so the window measured real work"
    );
    assert_eq!(
        n, 0,
        "the JSON writer must allocate ZERO: it appends bytes and formats nothing \
         (got {n} allocations)"
    );

    // The window is live: an allocating body inside it must be caught, or a
    // zero result would prove nothing.
    let (over, _) = count_env(|| {
        let v: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(&v);
    });
    assert!(over > 0, "the window must observe allocations");
}

/// The same cost twice: the budget is a property of the writer, not of a
/// lucky first run.
#[test]
fn the_render_cost_is_stable() {
    let wi = full_workitem();
    let mut out: Vec<u8> = Vec::with_capacity(2048);
    let (first, _) = count_env(|| {
        out.clear();
        clarus_server::handlers::ups::json::write_workitem_json(&mut out, &wi);
    });
    let (second, _) = count_env(|| {
        out.clear();
        clarus_server::handlers::ups::json::write_workitem_json(&mut out, &wi);
    });
    assert_eq!(first, second, "the render cost must be deterministic");
}

/// The Referenced Request Sequence is EMITTED with both values, so the zero
/// budget above is not met by a writer that quietly stopped rendering the
/// sequence - the failure mode that would make a green gate a lie.
#[test]
fn the_render_emits_the_referenced_request_sequence() {
    let wi = full_workitem();
    let mut out: Vec<u8> = Vec::with_capacity(2048);
    clarus_server::handlers::ups::json::write_workitem_json(&mut out, &wi);
    let text = std::str::from_utf8(&out).expect("the render is UTF-8");

    assert!(
        text.contains("0040A370"),
        "the Referenced Request Sequence is present: {text}"
    );
    assert!(
        text.contains("\"00080050\":{\"Value\":[\"ACC-77\"],\"vr\":\"SH\"}"),
        "carrying the accession: {text}"
    );
    assert!(
        text.contains("\"0020000D\":{\"Value\":[\"1.2.826.0.1.3680043.10.9999.1\"]"),
        "and the study: {text}"
    );
    // The bare duplicates are gone, so the sequence is the ONLY carriage.
    assert!(
        !text.contains("\"00080050\":{\"Value\":[\"ACC-77\"],\"vr\":\"SH\"},\"00080060\""),
        "no bare top-level accession: {text}"
    );
}

/// A workitem with NEITHER the study nor the accession nor the requested
/// procedure ID nor the reason skips the sequence: the shortest path is
/// cheaper than the longest, and neither allocates.
#[test]
fn a_workitem_without_a_study_still_allocates_nothing() {
    let mut wi = full_workitem();
    wi.accession_number = None;
    wi.input_study_uid = None;
    wi.requested_procedure_id = None;
    wi.reason_for_requested_procedure = None;
    let mut out: Vec<u8> = Vec::with_capacity(2048);

    let (n, len) = count_env(|| {
        clarus_server::handlers::ups::json::write_workitem_json(&mut out, &wi);
        out.len()
    });
    assert!(len > 0, "the render produced bytes");
    assert_eq!(n, 0, "the skip path is zero-alloc as well (got {n})");
    assert!(
        !std::str::from_utf8(&out)
            .expect("UTF-8")
            .contains("0040A370"),
        "no sequence when neither value is known"
    );
}
