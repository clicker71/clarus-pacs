// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Golden byte-exact fixtures for the typed DICOM XML writer.
//!
//! Freezes the output of `clarus_core::query::xml` (the typed writer over
//! manifest `attrs`) so the always-on `qido-xml` path cannot drift the wire
//! format. Fixtures are loaded with `include_str!` and compared byte-exact.
//!
//! re-baseline: the XML goldens mirror the B-tree JSON writer's tag set
//! (not the SQLite-era Value models). `00201206` (NumberOfStudyRelatedSeries)
//! and `00201208` (NumberOfStudyRelatedInstances) are REQUIRED (R) study
//! attributes per PS3.18 Table 10.6.3-3 (2026c) and are emitted from the
//! RAM-only derived counts. `00080005` (SpecificCharacterSet) is ALWAYS
//! emitted first at the study/series levels. The
//! old `study.xml` Value-based fixture is cross-checked on the overlapping tags
//! only.

#![forbid(unsafe_code)]

mod common;

use clarus_core::manifest::{AttrValue, InstanceRecord, SeriesRecord, StudyQido};
use clarus_core::query::tag;

fn doc(body: &[u8]) -> String {
    String::from_utf8(body.to_vec()).expect("XML golden is UTF-8")
}

/// Assert the study XML is byte-exact with the committed golden fixture.
/// `CLARUS_FIXTURE_REGEN=1` re-freezes the fixture from the current writer
/// output and exits the test green (one-shot re-baseline, like smoke_qido).
fn check_fixture(name: &str, actual: &str) {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("qido")
        .join(name);
    if std::env::var("CLARUS_FIXTURE_REGEN").is_ok() {
        std::fs::write(&path, actual.as_bytes())
            .unwrap_or_else(|e| panic!("regen write {}: {e}", path.display()));
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("fixture missing: {}", path.display()));
    assert_eq!(actual, expected, "fixture drift in {name}");
}

fn write_doc(attrs: impl FnOnce(&mut [u8]) -> usize) -> String {
    let mut body = Vec::new();
    clarus_core::query::xml::write_xml_doc_start(&mut body);
    let mut buf = [0u8; 4096];
    let n = attrs(&mut buf);
    body.extend_from_slice(&buf[..n]);
    clarus_core::query::xml::write_xml_doc_end(&mut body);
    doc(&body)
}

/// The B-tree `StudyQido` mirror of the SQLite-era `fixed_study()`, with PN
/// decomposition (`ИВАНОВ^ИВАН`) and an empty study description.
fn fixed_study() -> StudyQido {
    StudyQido {
        attrs: vec![
            (tag::STUDY_DATE, AttrValue::UInt(20230515)),
            (tag::MODALITY, AttrValue::Text("MR".into())),
            (
                tag::REFERRING_PHYSICIAN,
                AttrValue::Text("ПЕТРОВ^АЛЕКСЕЙ".into()),
            ),
            (tag::STUDY_DESCRIPTION, AttrValue::Text("".into())),
            (tag::PATIENT_NAME, AttrValue::Text("ИВАНОВ^ИВАН".into())),
            (tag::PATIENT_ID, AttrValue::Text("PT-77".into())),
            (tag::PATIENT_SEX, AttrValue::Text("O".into())),
            (
                tag::STUDY_INSTANCE_UID,
                AttrValue::Text("1.2.840.113619.2.55.3.1.1.77".into()),
            ),
            (tag::STUDY_ID, AttrValue::Text("S77".into())),
        ],
        ..Default::default()
    }
}

fn mk_inst() -> InstanceRecord {
    InstanceRecord {
        attrs: vec![(
            tag::SOP_INSTANCE_UID,
            AttrValue::Text("1.2.840.113619.2.55.3.1.1.77.1.1".into()),
        )],
        transfer_syntax: String::new(),
        storage_tier: 1,
        rejection_reason: 0,
        blake3: [0u8; 32],
        file_size: 0,
        frames: Vec::new(),
    }
}

fn fixed_series() -> SeriesRecord {
    SeriesRecord {
        attrs: vec![
            (tag::SERIES_NUMBER, AttrValue::Text("2".into())),
            (tag::MODALITY, AttrValue::Text("MR".into())),
            (
                tag::SERIES_INSTANCE_UID,
                AttrValue::Text("1.2.840.113619.2.55.3.1.1.77.1".into()),
            ),
            (tag::SERIES_DATE, AttrValue::UInt(20230515)),
            (tag::SERIES_TIME, AttrValue::Text("103100.000".into())),
            (tag::STATION_NAME, AttrValue::Text("MR_ROOM1".into())),
            (tag::SERIES_DESCRIPTION, AttrValue::Text("T2 Axial".into())),
            (tag::BODY_PART_EXAMINED, AttrValue::Text("BRAIN".into())),
        ],
        instances: vec![mk_inst(), mk_inst(), mk_inst()],
    }
}

fn fixed_instance() -> InstanceRecord {
    InstanceRecord {
        attrs: vec![
            (
                tag::SOP_CLASS_UID,
                AttrValue::Text("1.2.840.10008.5.1.4.1.1.4".into()),
            ),
            (
                tag::SOP_INSTANCE_UID,
                AttrValue::Text("1.2.840.113619.2.55.3.1.1.77.1.1".into()),
            ),
            (tag::INSTANCE_NUMBER, AttrValue::Text("1".into())),
        ],
        transfer_syntax: String::new(),
        storage_tier: 1,
        rejection_reason: 0,
        blake3: [0u8; 32],
        file_size: 0,
        frames: Vec::new(),
    }
}

#[test]
fn study_xml_byte_exact() {
    let study = fixed_study();
    let xml = write_doc(|buf| {
        clarus_core::query::xml::write_study_attrs(buf, &study, None, None).expect("fits")
    });
    check_fixture("study.xml", &xml);

    // Key-set baseline intersect data. `fixed_study()` stores no
    // StudyTime / AccessionNumber / PatientBirthDate and its derived
    // modalities aggregate is None - all legitimately omitted - so the
    // expectation drops them; the derived counts are always emitted.
    let expected = [
        0x0008_0020,
        0x0008_0090,
        0x0010_0010,
        0x0010_0020,
        0x0010_0040,
        0x0020_000D,
        0x0020_0010,
        0x0020_1206,
        0x0020_1208,
    ];
    common::assert_baseline(&common::xml_tag_set(xml.as_bytes()), &expected);
}

#[test]
fn series_xml_byte_exact() {
    let series = fixed_series();
    let xml = write_doc(|buf| {
        clarus_core::query::xml::write_series_attrs(buf, &series, None, None, None).expect("fits")
    });
    check_fixture("series.xml", &xml);

    // Key-set check: all four mandatory series tags are present in the data.
    common::assert_baseline(
        &common::xml_tag_set(xml.as_bytes()),
        &common::MANDATORY_SERIES,
    );
}

#[test]
fn instance_xml_byte_exact() {
    let instance = fixed_instance();
    let xml = write_doc(|buf| {
        clarus_core::query::xml::write_instance_attrs(
            buf,
            &instance,
            "1.2.840.113619.2.55.3.1.1.77",
            "1.2.840.113619.2.55.3.1.1.77.1",
            None,
            None,
        )
        .expect("fits")
    });
    check_fixture("instance.xml", &xml);

    // Key-set check: all five mandatory instance tags are present (UIDs
    // injected from context, the rest from the data).
    common::assert_baseline(
        &common::xml_tag_set(xml.as_bytes()),
        &common::MANDATORY_INSTANCE,
    );
}

/// Regression net: filtered XML golden, study level. The requested
/// non-mandatory StudyDescription (empty in `fixed_study()`) is emitted
/// alongside the mandatory baseline - byte-pinned + key-set asserted.
#[test]
fn study_xml_filtered_golden() {
    let study = fixed_study();
    let xml = write_doc(|buf| {
        clarus_core::query::xml::write_study_attrs(buf, &study, Some("00081030"), None)
            .expect("fits")
    });
    check_fixture("study_f00081030.xml", &xml);
    let tags = common::xml_tag_set(xml.as_bytes());
    assert!(
        tags.contains(&0x0008_1030),
        "requested StudyDescription must be emitted"
    );
    let expected = [
        0x0008_0020,
        0x0008_0090,
        0x0010_0010,
        0x0010_0020,
        0x0010_0040,
        0x0020_000D,
        0x0020_0010,
        0x0020_1206,
        0x0020_1208,
    ];
    common::assert_baseline(&tags, &expected);
}

/// Regression net: filtered XML golden, series level (requested
/// non-mandatory SeriesDescription + mandatory baseline).
#[test]
fn series_xml_filtered_golden() {
    let series = fixed_series();
    let xml = write_doc(|buf| {
        clarus_core::query::xml::write_series_attrs(buf, &series, Some("0008103E"), None, None)
            .expect("fits")
    });
    check_fixture("series_f0008103e.xml", &xml);
    let tags = common::xml_tag_set(xml.as_bytes());
    assert!(
        tags.contains(&0x0008_103E),
        "requested SeriesDescription must be emitted"
    );
    common::assert_baseline(&tags, &common::MANDATORY_SERIES);
}

/// Regression net: filtered XML golden, instance level. The requested
/// StudyDate (00080020) does not belong to the instance level, so the
/// response is the pure mandatory baseline - pinning the filtered path even
/// when the requested tag has no instance attribute.
#[test]
fn instance_xml_filtered_golden() {
    let instance = fixed_instance();
    let xml = write_doc(|buf| {
        clarus_core::query::xml::write_instance_attrs(
            buf,
            &instance,
            "1.2.840.113619.2.55.3.1.1.77",
            "1.2.840.113619.2.55.3.1.1.77.1",
            Some("00080020"),
            None,
        )
        .expect("fits")
    });
    check_fixture("instance_f00080020.xml", &xml);
    let tags = common::xml_tag_set(xml.as_bytes());
    assert!(
        !tags.contains(&0x0008_0020),
        "StudyDate must not leak into an instance response"
    );
    common::assert_baseline(&tags, &common::MANDATORY_INSTANCE);
}

/// A fixed [`WorkItem`] exercising the UPS attributes, PN decomposition
/// (Cyrillic), and the state/identity presence rules.
///
/// The Modality is `None`: UPS defines no such attribute (PS3.4 Table
/// CC.2.5-3), so a workitem with no input study in scope - which is what this
/// fixture models - carries none, and the render omits (0008,0060).
///
/// The study and the accession are rendered ONLY inside (0040,A370)
/// ReferencedRequestSequence (PS3.4 Table CC.2.5-3), never as bare top-level
/// elements: the XML writer states the same two facts, in the same carriage,
/// as the JSON writer.
fn fixed_workitem() -> clarus_ports::models::WorkItem {
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
        // No input study in scope -> no Modality -> the element is omitted.
        modality: None,
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
        patient_id_issuer: None,
        scheduled_workitem_code: None,
        // The performed report and the output list are Update-time
        // attributes (PS3.18 11.6); the golden captures the SCHEDULED shape,
        // which has neither.
        output_information: None,
        performed_workitem_code: None,
        performed_station_name_code: None,
        requested_procedure_id: None,
        // The golden captures the SCHEDULED shape, in which no performer has
        // reported, so both datetimes are absent.
        performed_start_datetime: None,
        performed_end_datetime: None,
        // The XML render does not carry an arbitrary SQ through (documented
        // as out of scope), so this fixture stays without parameters: the
        // golden document is what it is.
        scheduled_processing_parameters: None,
        reason_for_requested_procedure: None,
    }
}

/// UPS workitem XML is byte-exact with the frozen `workitem.xml` golden,
/// which was captured from the shipped typed renderer - the writer must not
/// drift it.
#[test]
fn workitem_xml_byte_exact() {
    let wi = fixed_workitem();
    let xml = clarus_server::handlers::ups::xml::workitem_to_xml(&wi);
    assert_eq!(xml, include_str!("fixtures/qido/workitem.xml"));
}

/// The SCHEDULED shape of [`fixed_workitem`] plus a stored (0074,1210)
/// parameters element and a (0040,1002) reason nested in (0040,A370) - the
/// two attributes the typed writer has to carry through rather than drop on
/// a read path.
fn fixed_workitem_with_parameters_and_reason() -> clarus_ports::models::WorkItem {
    let mut wi = fixed_workitem();
    wi.scheduled_processing_parameters = Some(
        r#"{"Value":[{"0040A040":{"vr":"CS","Value":["TEXT"]},"0040A160":{"vr":"UT","Value":["window 400/40"]}}],"vr":"SQ"}"#
            .to_string(),
    );
    wi.reason_for_requested_procedure = Some("rule out pneumonia".to_string());
    wi
}

/// The typed writer emits (0074,1210) and (0040,1002) when the record carries
/// them; the frozen golden is the byte-exact shape of both.
#[test]
fn workitem_xml_byte_exact_with_parameters_and_reason() {
    let wi = fixed_workitem_with_parameters_and_reason();
    let xml = clarus_server::handlers::ups::xml::workitem_to_xml(&wi);
    check_fixture("workitem_params.xml", &xml);
}

/// The study and the accession live ONLY inside (0040,A370): the bare
/// top-level (0008,0050) and (0020,000D) are gone, and the Referenced Request
/// Sequence is the carriage the task requester reads (PS3.4 Table CC.2.5-3).
///
/// Both tags can appear NESTED (and the study also appears nested inside
/// (0040,4021)), so a bare element is identified by what PRECEDES it: a
/// top-level element directly follows another top-level element, while a
/// nested one follows the `<Item number="1">` opening of its sequence.
#[test]
fn workitem_xml_carries_the_referenced_request_sequence() {
    let wi = fixed_workitem();
    let xml = clarus_server::handlers::ups::xml::workitem_to_xml(&wi);

    // Every occurrence of the accession is inside a sequence opening: the
    // opening is the immediately preceding text run.
    let seq_open = "keyword=\"ReferencedRequestSequence\"><Item number=\"1\">";
    let mut search_from = 0;
    let mut accession_hits = 0;
    while let Some(at) = xml[search_from..].find("<DicomAttribute tag=\"00080050\"") {
        let abs = search_from + at;
        accession_hits += 1;
        assert!(
            xml[..abs].ends_with(seq_open),
            "every (0008,0050) is the FIRST element of the sequence item: {xml}"
        );
        search_from = abs + 1;
    }
    assert_eq!(accession_hits, 1, "exactly one accession element: {xml}");

    // (0020,000D) appears exactly twice - once nested in (0040,4021), once in
    // (0040,A370) - and never as a bare top-level element. Both occurrences
    // open an `<Item>` body or follow another element inside one.
    let study_hits = xml.matches("<DicomAttribute tag=\"0020000D\"").count();
    assert_eq!(
        study_hits, 2,
        "the study appears in the two sequences only, never bare: {xml}"
    );

    // The Referenced Request Sequence is present, with both nested elements
    // in ascending tag order.
    let expected = "<DicomAttribute tag=\"0040A370\" vr=\"SQ\" \
                    keyword=\"ReferencedRequestSequence\"><Item number=\"1\">\
                    <DicomAttribute tag=\"00080050\" vr=\"SH\" keyword=\"\">\
                    <Value number=\"1\">ACC-77</Value></DicomAttribute>\
                    <DicomAttribute tag=\"0020000D\" vr=\"UI\" keyword=\"\">\
                    <Value number=\"1\">1.2.826.0.1.3680043.10.9999.1</Value>\
                    </DicomAttribute></Item></DicomAttribute>";
    assert!(
        xml.contains(expected),
        "the sequence carries study + accession in ascending tag order: {xml}"
    );

    // It sits between (0040,4052) and (0074,1000), the JSON render's position.
    let seq_at = xml.find("0040A370").expect("the sequence is present");
    let state_at = xml.find("00741000").expect("the state is present");
    let cancel_dt_at = xml.find("00404052");
    assert!(
        cancel_dt_at.is_none() || cancel_dt_at.unwrap() < seq_at,
        "the sequence follows (0040,4052) when that element is present"
    );
    assert!(
        seq_at < state_at,
        "and precedes (0074,1000), the ascending-tag-order position"
    );

    // (0040,4021) InputInformationSequence is unchanged and still names the
    // study: the two sequences are not alternatives.
    assert!(
        xml.contains("keyword=\"InputInformationSequence\""),
        "the Input Information Sequence stays: {xml}"
    );
}

/// A workitem that knows only the STUDY emits the sequence with the study
/// alone; one that knows only the ACCESSION emits it with the accession
/// alone; one that knows neither emits NO sequence.
#[test]
fn workitem_xml_referenced_request_emits_what_is_known() {
    // Study only.
    let mut wi = fixed_workitem();
    wi.accession_number = None;
    let xml = clarus_server::handlers::ups::xml::workitem_to_xml(&wi);
    assert!(
        xml.contains(
            "<DicomAttribute tag=\"0040A370\" vr=\"SQ\" keyword=\"ReferencedRequestSequence\">\
             <Item number=\"1\"><DicomAttribute tag=\"0020000D\" vr=\"UI\" keyword=\"\">"
        ),
        "study-only sequence: {xml}"
    );
    assert!(
        !xml.contains("<DicomAttribute tag=\"00080050\""),
        "and no accession element: {xml}"
    );

    // Accession only.
    let mut wi = fixed_workitem();
    wi.input_study_uid = None;
    let xml = clarus_server::handlers::ups::xml::workitem_to_xml(&wi);
    assert!(
        xml.contains(
            "<DicomAttribute tag=\"0040A370\" vr=\"SQ\" keyword=\"ReferencedRequestSequence\">\
             <Item number=\"1\"><DicomAttribute tag=\"00080050\" vr=\"SH\" keyword=\"\">\
             <Value number=\"1\">ACC-77</Value></DicomAttribute></Item></DicomAttribute>"
        ),
        "accession-only sequence: {xml}"
    );
    assert!(
        !xml.contains("<DicomAttribute tag=\"0020000D\""),
        "and no study element at all: {xml}"
    );

    // Neither: no sequence, and no Input Information Sequence either.
    let mut wi = fixed_workitem();
    wi.accession_number = None;
    wi.input_study_uid = None;
    let xml = clarus_server::handlers::ups::xml::workitem_to_xml(&wi);
    assert!(
        !xml.contains("0040A370"),
        "neither value -> no Referenced Request Sequence: {xml}"
    );
    assert!(
        !xml.contains("00404021"),
        "and no Input Information Sequence without a study: {xml}"
    );
}
