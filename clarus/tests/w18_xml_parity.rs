// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! W18: XML/JSON render parity for the UPS workitem (m16) and ONE XML document
//! for a multi-record MWL answer (m14).
//!
//! Both are conformance properties, not formatting preferences: an XML message
//! has exactly one declaration and one root (XML 1.0 2.1/2.8), and PS3.4
//! Table CC.2.5-3 types (0010,0021) 1C - required whenever a Patient ID is
//! present. A renderer that omits the element states a different workitem from
//! the JSON one, and a body with two roots is not parseable at all.

#![forbid(unsafe_code)]

use clarus_ports::models::{OutputReference, WorkItem, WorkItemCode};

fn code(value: &str, scheme: &str, meaning: &str) -> WorkItemCode {
    WorkItemCode {
        value: value.to_string(),
        scheme: scheme.to_string(),
        meaning: meaning.to_string(),
    }
}

/// A workitem carrying every attribute the three m16 elements need.
fn full_workitem() -> WorkItem {
    WorkItem {
        id: Some(1),
        workitem_uid: "1.2.826.0.1.3680043.10.1234.9001".to_string(),
        workflow_state: "COMPLETED".to_string(),
        station_aet: Some("AI_STATION".to_string()),
        station_name: Some("AI_ROOM".to_string()),
        procedure_step_start_date: Some("20261023".to_string()),
        procedure_step_start_time: Some("090000".to_string()),
        patient_name: Some("IVANOV^IVAN".to_string()),
        patient_id: Some("PT-1".to_string()),
        // The issuer the site declares. Type 1C, so it MUST travel with a
        // Patient ID.
        patient_id_issuer: Some("ISSUER-A".to_string()),
        accession_number: Some("ACC-1".to_string()),
        requesting_physician: None,
        modality: None,
        sps_priority: Some("HIGH".to_string()),
        transaction_uid: None,
        created_at: None,
        updated_at: None,
        step_label: Some("CAD".to_string()),
        input_readiness: Some("READY".to_string()),
        sps_start_datetime: Some("20261023T090000".to_string()),
        input_study_uid: Some("1.2.826.0.1.3680043.10.9999.1".to_string()),
        reason_for_cancellation: None,
        cancellation_datetime: None,
        // The requested workitem code: AIW-I 4.84.4.1.2.1 has the performer
        // echo it back, so the requester has to state it.
        scheduled_workitem_code: Some(code("CXR-AI", "99CLARUS", "Chest X-ray AI")),
        output_information: Some(vec![OutputReference {
            sop_class_uid: "1.2.840.10008.5.1.4.1.1.88.22".to_string(),
            series_instance_uid: "1.2.826.0.1.3680043.10.9999.1.2".to_string(),
            sop_instance_uid: "1.2.826.0.1.3680043.10.9999.1.2.1".to_string(),
        }]),
        performed_workitem_code: Some(code("CXR-AI", "99CLARUS", "Chest X-ray AI")),
        performed_station_name_code: Some(code("AI_STATION", "99CLARUS", "AI station")),
        requested_procedure_id: Some("RP-1".to_string()),
        performed_start_datetime: Some("20261023T091000".to_string()),
        performed_end_datetime: Some("20261023T091500".to_string()),
        // The opaque parameters element and the nested reason. Both have to
        // SURVIVE the XML render round-trip: the read path must not silently
        // drop what the JSON renderer returns.
        scheduled_processing_parameters: Some(
            r#"{"Value":[{"0040A040":{"vr":"CS","Value":["TEXT"]}}],"vr":"SQ"}"#.to_string(),
        ),
        reason_for_requested_procedure: Some("rule out pneumonia".to_string()),
    }
}

/// (0010,0021) is emitted exactly when (0010,0020) is - the same presence rule
/// the JSON renderer applies, because PS3.4 types it 1C.
#[test]
fn w18_xml_emits_the_issuer_only_with_a_patient_id() {
    let xml = clarus_server::handlers::ups::xml::workitem_to_xml(&full_workitem());
    assert!(
        xml.contains("<DicomAttribute tag=\"00100021\" vr=\"LO\" keyword=\"\">"),
        "an issuer travels with the patient it belongs to: {xml}"
    );
    assert!(xml.contains("<Value number=\"1\">ISSUER-A</Value>"));

    // No Patient ID -> no issuer element at all.
    let mut wi = full_workitem();
    wi.patient_id = None;
    wi.patient_id_issuer = Some("ISSUER-A".to_string());
    let xml = clarus_server::handlers::ups::xml::workitem_to_xml(&wi);
    assert!(
        !xml.contains("00100021"),
        "an issuer must not describe a patient the workitem does not name: {xml}"
    );
}

/// (0040,4018) ScheduledWorkitemCodeSequence and (0074,1216) UnifiedProcedure-
/// StepPerformedProcedureSequence are emitted, and the performer report lives
/// INSIDE the (0074,1216) Item - PS3.3 Table C.30.3-1 puts it there and
/// nowhere else, so an emitted-sibling rendering states a different workitem.
#[test]
fn w18_xml_emits_the_workitem_code_and_the_performed_report() {
    let xml = clarus_server::handlers::ups::xml::workitem_to_xml(&full_workitem());

    assert!(
        xml.contains("tag=\"00404018\" vr=\"SQ\" keyword=\"ScheduledWorkitemCodeSequence\""),
        "the requested code must reach the XML render: {xml}"
    );

    const SEQ_OPEN: &str = "<DicomAttribute tag=\"00741216\" vr=\"SQ\" keyword=\"UnifiedProcedureStepPerformedProcedureSequence\">";
    let start = xml
        .find(SEQ_OPEN)
        .expect("the performed procedure sequence is present");
    // The sequence closes with the item that belongs to it; slicing between
    // the opener and the first close is what "inside the item" means textually,
    // because the children are written between them with no nesting of their
    // own beyond their own Item.
    let body_start = start + SEQ_OPEN.len();
    let body_end = xml[body_start..]
        .find("</DicomAttribute></NativeDicomModel>")
        .map(|p| body_start + p)
        .expect("the sequence closes before the document does");
    let item = &xml[body_start..body_end];

    assert!(
        item.starts_with("<Item number=\"1\">"),
        "the report is wrapped in an Item: {item}"
    );
    for (tag, what) in [
        ("00404019", "performed workitem code"),
        ("00404028", "performed station code"),
        ("00404033", "output information"),
        ("00404050", "performed start"),
        ("00404051", "performed end"),
    ] {
        assert!(
            item.contains(&format!("tag=\"{tag}\"")),
            "the {what} ({tag}) must be a CHILD of the sequence item: {item}"
        );
    }
    // Nothing of the report escaped to the model root.
    let outside = format!("{}{}", &xml[..start], &xml[body_end..]);
    assert!(
        !outside.contains("00404019"),
        "no part of the report lives outside the sequence: {outside}"
    );
}

/// A workitem with no report at all emits no (0074,1216): an item with no
/// children would say "a performance was reported and it states nothing".
#[test]
fn w18_xml_omits_the_performed_sequence_when_nothing_was_reported() {
    let mut wi = full_workitem();
    wi.output_information = None;
    wi.performed_workitem_code = None;
    wi.performed_station_name_code = None;
    wi.performed_start_datetime = None;
    wi.performed_end_datetime = None;
    let xml = clarus_server::handlers::ups::xml::workitem_to_xml(&wi);
    assert!(
        !xml.contains("00741216"),
        "no report means no sequence: {xml}"
    );
}

/// (0074,1210) Scheduled Processing Parameters Sequence and (0040,1002)
/// Reason for the Requested Procedure survive the XML round-trip: both are
/// read-path attributes, and a renderer that dropped either would hand the
/// `application/dicom+xml` reader a workitem missing facts the JSON renderer
/// states.
#[test]
fn w18_xml_round_trips_the_parameters_and_the_reason() {
    let xml = clarus_server::handlers::ups::xml::workitem_to_xml(&full_workitem());

    // (0074,1210) carries the stored Content Item subtree through, nested in
    // its own Item.
    assert!(
        xml.contains(
            "<DicomAttribute tag=\"00741210\" vr=\"SQ\" \
             keyword=\"ScheduledProcessingParametersSequence\"><Item number=\"1\">\
             <DicomAttribute tag=\"0040A040\" vr=\"CS\" keyword=\"\">\
             <Value number=\"1\">TEXT</Value></DicomAttribute>"
        ),
        "the parameters subtree survives the round-trip: {xml}"
    );

    // (0040,1002) is nested inside (0040,A370), the one carriage the JSON
    // renderer uses for it.
    assert!(
        xml.contains(
            "<DicomAttribute tag=\"00401002\" vr=\"LO\" keyword=\"\">\
             <Value number=\"1\">rule out pneumonia</Value></DicomAttribute>"
        ),
        "the reason survives the round-trip: {xml}"
    );
}

// m14 (one XML document per MWL answer) is asserted in the module that renders
// it, `handlers::mwl_pure::tests::xml_answer_is_one_document_for_many_records`,
// because `sps_to_xml` is private by design.
