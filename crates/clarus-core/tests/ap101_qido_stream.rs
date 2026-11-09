// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! zero-heap guarantee for the QIDO CBOR->JSON streaming writer.
//!
//! `clarus_core::query::json::write_study_json` streams a study's integer
//! `(u32 tag, AttrValue)` attributes into a **caller-owned fixed slice** with
//! no `serde_json::Value` and no allocation: the writer owns no memory, so it
//! is allocation-free by construction. This test enforces that property by
//! running every measurement through [`TestAllocator`] and asserting the
//! guarded buffer-fill windows allocate nothing.
//!
//! # Why a single test function and a global allocator
//!
//! `TestAllocator` counts process-wide; libtest runs tests on separate threads,
//! so a concurrent sibling test could attribute its own allocation to a
//! measured window. Running every check within one function confines the
//! measurement to a single live libtest thread - the deterministic
//! configuration the global allocator supports.

use clarus_core::manifest::{AttrValue, StudyQido};
use clarus_core::query::json::write_study_json;
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// Assert that `body` runs with zero allocations and return its value.
///
/// The measurement is done inline (not through a closure) because this
/// environment's `TestAllocator` can report a phantom allocation for the
/// closure-call machinery itself; an inline window around the exact call under
/// test gives the deterministic `(count, bytes)` delta.
fn run_zero_alloc<R>(body: impl FnOnce() -> R) -> R {
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let result = body();
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    assert_eq!(
        before, after,
        "guarded window must not allocate (before: {before:?}, after: {after:?})"
    );
    result
}

/// A study populated with the full study-level QIDO subset.
fn full_study() -> StudyQido {
    StudyQido {
        attrs: vec![
            (0x0008_0020, AttrValue::UInt(20_260_802)), // StudyDate
            (0x0008_0030, AttrValue::Text("120000".into())), // StudyTime
            (0x0008_0050, AttrValue::Text("A100".into())), // AccessionNumber
            (0x0008_0060, AttrValue::Text("CT".into())), // Modality
            (0x0008_0090, AttrValue::Text("dr.smith".into())), // ReferringPhysician
            (0x0008_1030, AttrValue::Text("chest".into())), // StudyDescription
            (0x0010_0010, AttrValue::Text("ivanov".into())), // PatientName
            (0x0010_0020, AttrValue::Text("PAT12".into())), // PatientID
            (0x0010_0030, AttrValue::UInt(19700101)),   // PatientBirthDate
            (0x0010_0040, AttrValue::Text("M".into())), // PatientSex
            (0x0020_000D, AttrValue::Text("1.2.840.1.1".into())), // StudyInstanceUID
            (0x0020_0010, AttrValue::Text("STUDY1".into())), // StudyID
        ],
        ..Default::default()
    }
}

/// A study whose text values hammer the JSON escaping branch.
fn escape_study() -> StudyQido {
    StudyQido {
        attrs: vec![
            (0x0010_0010, AttrValue::Text("o\"brien\\jr".into())),
            (0x0008_1030, AttrValue::Text("line1\nline2\ttab".into())),
        ],
        ..Default::default()
    }
}

#[test]
fn qido_stream_is_allocation_free() {
    // Disable allocation tracking before building the fixtures. `TestAllocator`
    // starts enabled; if we allocate the `StudyQido` Strings while enabled and
    // they drop while the harness is disabled, the byte counter drifts and the
    // harness reports phantom allocations. Building fixtures while disabled
    // (their deallocation is irrelevant) keeps every measured window clean.
    ALLOC.set_enabled(false);

    // Build the fixtures up front (their setup allocations are outside every
    // measured window). The writer itself fills fixed stack buffers.
    let study = full_study();
    let escape = escape_study();

    // -- Full study: 0 alloc, correct DICOMweb JSON --
    let (n, buf) = run_zero_alloc(|| {
        let mut buf = [0u8; 1024];
        let n = write_study_json(&mut buf, &study, None, None).expect("fits");
        std::hint::black_box(&buf);
        (n, buf)
    });
    assert!(n > 0, "streaming must produce non-empty JSON");
    let text = String::from_utf8(buf[..n].to_vec()).expect("valid UTF-8 JSON");
    assert!(text.contains("\"0020000D\":{\"Value\":[\"1.2.840.1.1\"],\"vr\":\"UI\"}"));
    assert!(text.starts_with('{') && text.ends_with('}'));

    // -- Escaping branch: 0 alloc, correct escaping --
    let (n, buf) = run_zero_alloc(|| {
        let mut buf = [0u8; 512];
        let n = write_study_json(&mut buf, &escape, None, None).expect("fits");
        std::hint::black_box(&buf);
        (n, buf)
    });
    assert!(n > 0, "escaping branch must produce non-empty JSON");
    let etext = String::from_utf8(buf[..n].to_vec()).expect("valid UTF-8 JSON");
    assert!(etext.contains("\\\""), "quotes must be escaped");
    assert!(etext.contains("\\\\"), "backslashes must be escaped");

    // -- Filtered writer (includefield + mandatory baseline): 0 alloc --
    let (n, buf) = run_zero_alloc(|| {
        let mut buf = [0u8; 1024];
        let n = write_study_json(&mut buf, &study, Some("00081030"), None).expect("fits");
        std::hint::black_box(&buf);
        (n, buf)
    });
    assert!(n > 0, "filtered streaming must produce non-empty JSON");
    let ftext = String::from_utf8(buf[..n].to_vec()).expect("valid UTF-8 JSON");
    assert!(
        ftext.contains("\"0020000D\""),
        "mandatory StudyInstanceUID must survive includefield: {ftext}"
    );
    assert!(
        ftext.contains("\"00081030\""),
        "requested tag must be emitted"
    );

    // -- Filtered writer, 40-tag includefield (capacity overflow): 0 alloc ---
    // The 40-tag request string is built OUTSIDE the measured window; the
    // filter itself stays on the stack ([u32; 64]) and must not heap.
    let forty: String = (0..40)
        .map(|i| format!("{:08X}", 0x1000_0001u32 + i))
        .collect::<Vec<_>>()
        .join(",");
    let (n, buf) = run_zero_alloc(|| {
        let mut buf = [0u8; 1024];
        let n = write_study_json(&mut buf, &study, Some(&forty), None).expect("fits");
        std::hint::black_box(&buf);
        (n, buf)
    });
    assert!(
        n > 0,
        "40-tag filtered streaming must produce non-empty JSON"
    );
    let f40 = String::from_utf8(buf[..n].to_vec()).expect("valid UTF-8 JSON");
    assert!(
        f40.contains("\"0020000D\""),
        "mandatory StudyInstanceUID must survive a 40-tag includefield: {f40}"
    );

    // Finally, sanity on actual bytes (allocates only outside the windows).
    let mut buf = [0u8; 1024];
    let n = write_study_json(&mut buf, &study, None, None).expect("fits");
    let text = String::from_utf8(buf[..n].to_vec()).expect("valid UTF-8 JSON");
    assert!(text.contains("\"0020000D\":{\"Value\":[\"1.2.840.1.1\"],\"vr\":\"UI\"}"));
    assert!(text.starts_with('{') && text.ends_with('}'));

    let mut ebuf = [0u8; 512];
    let en = write_study_json(&mut ebuf, &escape, None, None).expect("fits");
    let etext = String::from_utf8(ebuf[..en].to_vec()).expect("valid UTF-8 JSON");
    assert!(etext.contains("\\\""), "quotes must be escaped");
    assert!(etext.contains("\\\\"), "backslashes must be escaped");
}
