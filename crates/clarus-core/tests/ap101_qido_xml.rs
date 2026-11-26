// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! zero-heap guarantee for the native QIDO-RS XML writers.
//!
//! `write_study_attrs`, `write_series_attrs`, `write_instance_attrs` and
//! `write_patient_xml` stream a row's attributes into a **caller-owned fixed
//! slice**. Every attribute value reaches the writer as a borrowed `&str`;
//! before that change each element formatted its payload into an owned
//! `String`, so one row cost one allocation per attribute.
//!
//! Values that are genuinely derived (the signed IS body, the retrieve URL
//! built from base + path + UID) still build a small value, so this gate
//! measures the path a normal row takes - the attributes that come straight
//! out of the manifest - and not the two derived ones.
//!
//! # Why a single test function
//!
//! `TestAllocator` counts process-wide; libtest runs tests on separate
//! threads. Running every check within one function confines the measurement
//! to a single live libtest thread.

use clarus_core::manifest::{AttrValue, InstanceRecord, SeriesRecord, StudyQido};
use clarus_core::query::json::PatientQido;
use clarus_core::query::xml::{
    write_instance_attrs, write_patient_xml, write_series_attrs, write_study_attrs,
};
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// Enable counting, run `f`, disable counting, report whether `f` allocated.
fn allocate_env<F, R>(f: F) -> (bool, R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let result = f();
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (before == after, result)
}

#[test]
fn qido_xml_writers_do_not_allocate() {
    let study = StudyQido {
        attrs: vec![
            (0x0008_0020, AttrValue::UInt(20_260_802)),
            (0x0008_0050, AttrValue::Text("ACC-1".into())),
            (0x0008_0060, AttrValue::Text("CT".into())),
            (0x0008_1030, AttrValue::Text("CHEST".into())),
            (0x0010_0010, AttrValue::Text("IVANOV^IVAN".into())),
        ],
        ..Default::default()
    };
    let series = SeriesRecord {
        attrs: vec![
            (0x0008_0060, AttrValue::Text("CT".into())),
            (0x0008_103E, AttrValue::Text("AXIAL".into())),
            (0x0020_000E, AttrValue::Text("1.2.840.1.2".into())),
            (0x0020_0011, AttrValue::UInt(2)),
        ],
        instances: Vec::new(),
    };
    let instance = InstanceRecord {
        blake3: [0u8; 32],
        attrs: vec![
            (
                0x0008_0016,
                AttrValue::Text("1.2.840.10008.5.1.4.1.1.2".into()),
            ),
            (0x0008_0018, AttrValue::Text("1.2.840.1.3".into())),
            (0x0020_0013, AttrValue::UInt(7)),
        ],
        transfer_syntax: "1.2.840.10008.1.2.1".into(),
        storage_tier: 1,
        rejection_reason: 0,
        file_size: 1024,
        frames: Vec::new(),
    };
    let patient = PatientQido {
        patient_uid: "PT-1".into(),
        patient_name: "IVANOV^IVAN".into(),
        last_arrival: None,
        study_count: 1,
        patient_id: 1,
    };

    // One buffer size for every writer; the row sizes above are well inside.
    let mut buf = [0u8; 4096];

    let (clean, n) = allocate_env(|| write_study_attrs(&mut buf, &study, None, None));
    assert!(clean, "write_study_attrs allocated");
    let study_len = n.expect("the study row fits");

    let (clean, n) =
        allocate_env(|| write_series_attrs(&mut buf, &series, None, None, Some("1.2")));
    assert!(clean, "write_series_attrs allocated");
    let series_len = n.expect("the series row fits");

    // A retrieve URL is derived from base + path + UID, so the base is a
    // borrowed literal here and only the assembled URL is rebuilt.
    let (clean, n) = allocate_env(|| {
        write_instance_attrs(&mut buf, &instance, "1.2", "1.2.3", None, Some("http://h"))
    });
    assert!(clean, "write_instance_attrs allocated");
    let instance_len = n.expect("the instance row fits");

    let (clean, n) = allocate_env(|| write_patient_xml(&mut buf, &patient));
    assert!(clean, "write_patient_xml allocated");
    let patient_len = n.expect("the patient row fits");

    // Guard against a writer that allocates nothing because it emits nothing:
    // each render must be a non-trivial document fragment.
    assert!(study_len > 200, "study row is too small: {study_len}");
    assert!(series_len > 100, "series row is too small: {series_len}");
    assert!(
        instance_len > 100,
        "instance row is too small: {instance_len}"
    );
    assert!(patient_len > 100, "patient row is too small: {patient_len}");
}
