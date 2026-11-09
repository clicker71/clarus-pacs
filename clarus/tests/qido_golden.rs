// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Golden byte-exact fixtures for the QIDO-RS pure serializers.
//!
//! Freezes the serialization output of `qido_json` /
//! `qido_xml` so `qido_pure.rs` cannot drift the
//! wire format. Fixtures are loaded with `include_str!` and compared
//! byte-exact (`assert_eq!`). They are the SAME files checked by the HTTP
//! smoke test (`smoke_qido.rs`) - neither commit may modify them.

#![forbid(unsafe_code)]

mod common;

/// A fixed `StudyQido` exercising Cyrillic PN, all VRs, an empty string value,
/// and omitted fields (represented as absent attrs). Mirrors the former
/// serde-era `fixed_study` (2.7, variant A: the serde reference oracle
/// was removed; the native B-tree renderer's output, captured as `study.json`,
/// is the frozen contract).
fn fixed_study_qido() -> clarus_core::manifest::StudyQido {
    use clarus_core::manifest::AttrValue;
    let text = |v: &str| AttrValue::Text(v.to_string());
    clarus_core::manifest::StudyQido {
        attrs: vec![
            (0x0008_0020, text("20230515")),                     // StudyDate
            (0x0008_0030, text("103000.125")),                   // StudyTime
            (0x0008_0050, text("ACC-77")),                       // AccessionNumber
            (0x0008_0060, text("MR")),                           // Modality
            (0x0008_0061, text("MR,CT")),                        // ModalitiesInStudy
            (0x0008_0090, text("ПЕТРОВ^АЛЕКСЕЙ")),               // ReferringPhysician
            (0x0008_1030, text("")),                             // StudyDescription (empty)
            (0x0010_0010, text("ИВАНОВ^ИВАН")),                  // PatientName
            (0x0010_0020, text("PT-77")),                        // PatientID
            (0x0010_0040, text("O")),                            // PatientSex
            (0x0020_000D, text("1.2.840.113619.2.55.3.1.1.77")), // StudyInstanceUID
            (0x0020_0010, text("S77")),                          // StudyID
        ],
        // Derived counts (RAM-only, mirror the serde-era fields num_series/num_instances).
        series_count: 0,
        instance_count: 3,
        modalities_in_study: Some("MR,CT".into()),
    }
}

/// Native B-tree study writer is byte-identical to the frozen golden.
#[test]
fn btree_study_json_byte_exact() {
    let mut buf = [0u8; 4096];
    let n = clarus_core::query::json::write_study_json(&mut buf, &fixed_study_qido(), None, None)
        .expect("fits");
    let actual = std::str::from_utf8(&buf[..n]).expect("UTF-8");
    assert_eq!(actual, include_str!("fixtures/qido/study.json"));

    // Key-set baseline intersect data. `fixed_study_qido()` stores no
    // PatientBirthDate (0010,0030) - legitimately omitted - so the expectation
    // drops it; the derived counts (00201206/00201208) and ModalitiesInStudy
    // (00080061, from the Some("MR,CT") aggregate) are always emitted.
    let expected = [
        0x0008_0020,
        0x0008_0030,
        0x0008_0050,
        0x0008_0061,
        0x0008_0090,
        0x0010_0010,
        0x0010_0020,
        0x0010_0040,
        0x0020_000D,
        0x0020_0010,
        0x0020_1206,
        0x0020_1208,
    ];
    common::assert_baseline(&common::json_key_set(&buf[..n]), &expected);
}

/// The native B-tree series writer (`write_series_json`) is byte-identical to
/// the frozen golden. Three instances are attached so `00201209`
/// (NumberOfSeriesRelatedInstances) is 3.
#[test]
fn btree_series_json_byte_exact() {
    use clarus_core::manifest::{AttrValue, InstanceRecord, SeriesRecord};
    use clarus_core::query::tag;

    let mk_inst = || InstanceRecord {
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
    };
    let series = SeriesRecord {
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
    };
    let mut buf = [0u8; 1024];
    let n = clarus_core::query::json::write_series_json(&mut buf, &series, None, None, None)
        .expect("fits");
    let actual = std::str::from_utf8(&buf[..n]).expect("UTF-8");
    assert_eq!(actual, include_str!("fixtures/qido/series.json"));

    // Key-set check: all four mandatory series tags are present in the data
    // (Modality, SeriesInstanceUID, SeriesNumber, derived 00201209).
    common::assert_baseline(&common::json_key_set(&buf[..n]), &common::MANDATORY_SERIES);
}

/// the B-tree instance writer is byte-identical to the frozen golden.
#[test]
fn btree_instance_json_byte_exact() {
    use clarus_core::manifest::{AttrValue, InstanceRecord};
    use clarus_core::query::tag;

    let instance = InstanceRecord {
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
    };
    let mut buf = [0u8; 1024];
    let n = clarus_core::query::json::write_instance_json(
        &mut buf,
        &instance,
        "1.2.840.113619.2.55.3.1.1.77",
        "1.2.840.113619.2.55.3.1.1.77.1",
        None,
        None,
    )
    .expect("fits");
    let actual = std::str::from_utf8(&buf[..n]).expect("UTF-8");
    assert_eq!(actual, include_str!("fixtures/qido/instance.json"));

    // Key-set check: all five mandatory instance tags are present in the data
    // plus the injected Study/Series Instance UIDs.
    common::assert_baseline(
        &common::json_key_set(&buf[..n]),
        &common::MANDATORY_INSTANCE,
    );
}

/// The B-tree `SeriesRecord` mirror of `fixed_series()`, with three instances
/// so `00201209` (NumberOfSeriesRelatedInstances) is 3.
fn fixed_btree_series() -> clarus_core::manifest::SeriesRecord {
    use clarus_core::manifest::{AttrValue, InstanceRecord, SeriesRecord};
    use clarus_core::query::tag;

    let mk_inst = || InstanceRecord {
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
    };
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

/// The B-tree `InstanceRecord` mirror of `fixed_instance()`.
fn fixed_btree_instance() -> clarus_core::manifest::InstanceRecord {
    use clarus_core::manifest::{AttrValue, InstanceRecord};
    use clarus_core::query::tag;
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

/// freeze (2.7, variant A): the native B-tree series writer's
/// `includefield` filtering is pinned byte-exact against frozen goldens
/// (captured from the native renderer at freeze time; the former serde oracle
/// was removed). `None` stays the frozen `series.json` ("all tags"); each
/// filtered subset is pinned too.
#[test]
fn series_includefield_goldens() {
    let series = fixed_btree_series();

    let mut buf = [0u8; 1024];
    let n = clarus_core::query::json::write_series_json(
        &mut buf,
        &series,
        Some("00080005"),
        None,
        None,
    )
    .expect("fits");
    assert_eq!(
        std::str::from_utf8(&buf[..n]).unwrap(),
        include_str!("fixtures/qido/series_f080005.json"),
        "series includefield 00080005 drifted from frozen golden"
    );
    common::assert_baseline(&common::json_key_set(&buf[..n]), &common::MANDATORY_SERIES);

    let mut buf = [0u8; 1024];
    let n = clarus_core::query::json::write_series_json(
        &mut buf,
        &series,
        Some("00080005,0020000E,00201209"),
        None,
        None,
    )
    .expect("fits");
    assert_eq!(
        std::str::from_utf8(&buf[..n]).unwrap(),
        include_str!("fixtures/qido/series_f080005_0020000E_00201209.json"),
        "series includefield 00080005,0020000E,00201209 drifted from frozen golden"
    );
    common::assert_baseline(&common::json_key_set(&buf[..n]), &common::MANDATORY_SERIES);

    let mut buf = [0u8; 1024];
    let n = clarus_core::query::json::write_series_json(
        &mut buf,
        &series,
        Some("00200011"),
        None,
        None,
    )
    .expect("fits");
    assert_eq!(
        std::str::from_utf8(&buf[..n]).unwrap(),
        include_str!("fixtures/qido/series_f00200011.json"),
        "series includefield 00200011 drifted from frozen golden"
    );
    common::assert_baseline(&common::json_key_set(&buf[..n]), &common::MANDATORY_SERIES);

    // `None` must still equal the frozen golden (no filter drift).
    let mut buf = [0u8; 1024];
    let n = clarus_core::query::json::write_series_json(&mut buf, &series, None, None, None)
        .expect("fits");
    assert_eq!(
        std::str::from_utf8(&buf[..n]).unwrap(),
        include_str!("fixtures/qido/series.json")
    );
}

/// freeze: instance writer `includefield` pinned against frozen
/// goldens (native renderer captured at freeze time; serde oracle removed).
#[test]
fn instance_includefield_goldens() {
    let study_uid = "1.2.840.113619.2.55.3.1.1.77";
    let series_uid = "1.2.840.113619.2.55.3.1.1.77.1";
    let instance = fixed_btree_instance();

    let mut buf = [0u8; 1024];
    let n = clarus_core::query::json::write_instance_json(
        &mut buf,
        &instance,
        study_uid,
        series_uid,
        Some("00080018"),
        None,
    )
    .expect("fits");
    assert_eq!(
        std::str::from_utf8(&buf[..n]).unwrap(),
        include_str!("fixtures/qido/instance_f00080018.json"),
        "instance includefield 00080018 drifted from frozen golden"
    );
    common::assert_baseline(
        &common::json_key_set(&buf[..n]),
        &common::MANDATORY_INSTANCE,
    );

    let mut buf = [0u8; 1024];
    let n = clarus_core::query::json::write_instance_json(
        &mut buf,
        &instance,
        study_uid,
        series_uid,
        Some("00080016,0020000D,00200013"),
        None,
    )
    .expect("fits");
    assert_eq!(
        std::str::from_utf8(&buf[..n]).unwrap(),
        include_str!("fixtures/qido/instance_f00080016_0020000D_00200013.json"),
        "instance includefield 00080016,0020000D,00200013 drifted from frozen golden"
    );
    common::assert_baseline(
        &common::json_key_set(&buf[..n]),
        &common::MANDATORY_INSTANCE,
    );

    let mut buf = [0u8; 1024];
    let n = clarus_core::query::json::write_instance_json(
        &mut buf,
        &instance,
        study_uid,
        series_uid,
        Some("0020000E"),
        None,
    )
    .expect("fits");
    assert_eq!(
        std::str::from_utf8(&buf[..n]).unwrap(),
        include_str!("fixtures/qido/instance_f0020000E.json"),
        "instance includefield 0020000E drifted from frozen golden"
    );
    common::assert_baseline(
        &common::json_key_set(&buf[..n]),
        &common::MANDATORY_INSTANCE,
    );

    // `None` must still equal the frozen golden.
    let mut buf = [0u8; 1024];
    let n = clarus_core::query::json::write_instance_json(
        &mut buf, &instance, study_uid, series_uid, None, None,
    )
    .expect("fits");
    assert_eq!(
        std::str::from_utf8(&buf[..n]).unwrap(),
        include_str!("fixtures/qido/instance.json")
    );
}

/// Regression net: the exact OHIF query shape - a REQUESTED
/// NON-mandatory tag must be emitted alongside the mandatory baseline.
/// Pre-net, every filtered golden requested only mandatory tags, so a
/// regression that drops requested tags (while keeping the baseline) would
/// have passed green.
#[test]
fn filtered_study_adds_requested_non_mandatory() {
    let study = fixed_study_qido();
    let mut buf = [0u8; 4096];
    let n = clarus_core::query::json::write_study_json(
        &mut buf,
        &study,
        Some("00081030,00080060,00080090"),
        None,
    )
    .expect("fits");
    let keys = common::json_key_set(&buf[..n]);
    assert!(
        keys.contains(&0x0008_1030),
        "requested StudyDescription must be emitted"
    );
    assert!(
        keys.contains(&0x0020_000D),
        "mandatory StudyInstanceUID must survive the OHIF query"
    );
    assert!(
        keys.contains(&0x0008_0060) && keys.contains(&0x0008_0090),
        "requested Modality/ReferringPhysicianName must be emitted"
    );
}

// the `study.xml` fixture was re-baselined to the B-tree typed writer
// (see `xml_golden.rs`). The legacy Value-path converter no longer owns a
// standalone XML fixture; it is cross-checked tag-for-tag against the typed
// writer in `xml_golden.rs::study_xml_overlaps_legacy_value_path`.
