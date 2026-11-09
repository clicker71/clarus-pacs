// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Shared QIDO key-set regression helpers.
//!
//! The golden fixtures are byte-exact, but neither field incident (OHIF lost
//! 0020000D; MicroDicom lost keys) would have failed them: no golden asserts
//! the mandatory key SET independently of the stored data. These scanners
//! extract the emitted tag keys from JSON / XML bodies and assert that the
//! mandatory baseline is present.
//!
//! INVARIANT: R attributes absent from the stored data are OMITTED by
//! the renderers, so `assert_baseline` MUST be applied to
//! the intersection of the baseline with the tags the fixture guarantees to
//! emit - otherwise a future corpus without, say, PatientBirthDate would
//! false-fail. Build the expectation from the fixture data; the derived study
//! tags (00201206 / 00201208 / 00080061) are always emitted by the writers
//! and belong in the expectation whenever the data guarantees them.

#![forbid(unsafe_code)]
// Each integration-test crate (`qido_golden`, `xml_golden`) compiles this
// shared module on its own and uses only a subset of its items (the JSON
// scanners in qido_golden, the XML scanners in xml_golden, one baseline
// subset per level). The union across the two crates is fully used, so the
// per-crate dead_code lint is a false positive of the split - silence it
// here, at the source, instead of sprinkling per-item allows.
#![allow(dead_code)]

/// Mirror of `clarus_core::query::json::MANDATORY_STUDY` (json.rs:104).
/// Those constants are `pub(crate)` and not visible from integration tests,
/// so this file pins copies - keep them in sync when json.rs changes.
pub const MANDATORY_STUDY: [u32; 13] = [
    0x0008_0020, // StudyDate
    0x0008_0030, // StudyTime
    0x0008_0050, // AccessionNumber
    0x0008_0061, // ModalitiesInStudy
    0x0008_0090, // ReferringPhysicianName
    0x0010_0010, // PatientName
    0x0010_0020, // PatientID
    0x0010_0030, // PatientBirthDate
    0x0010_0040, // PatientSex
    0x0020_000D, // StudyInstanceUID
    0x0020_0010, // StudyID
    0x0020_1206, // NumberOfStudyRelatedSeries
    0x0020_1208, // NumberOfStudyRelatedInstances
];

/// Mirror of `clarus_core::query::json::MANDATORY_SERIES` (json.rs:119).
pub const MANDATORY_SERIES: [u32; 4] = [
    0x0008_0060, // Modality
    0x0020_000E, // SeriesInstanceUID
    0x0020_0011, // SeriesNumber
    0x0020_1209, // NumberOfSeriesRelatedInstances
];

/// Mirror of `clarus_core::query::json::MANDATORY_INSTANCE` (json.rs:125).
pub const MANDATORY_INSTANCE: [u32; 5] = [
    0x0008_0016, // SOPClassUID
    0x0008_0018, // SOPInstanceUID
    0x0020_000D, // StudyInstanceUID
    0x0020_000E, // SeriesInstanceUID
    0x0020_0013, // InstanceNumber
];

/// Whether `b` is a hex digit (either case).
fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

/// The numeric value of a hex digit byte.
fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        _ => b - b'A' + 10,
    }
}

/// Scan a DICOMweb JSON body for object-level tag keys: tokens of the form
/// `"` + exactly 8 hex digits + `":`. Nested value objects (`{"vr":..}`,
/// `{"Alphabetic":..}`) carry no tag keys, so the scanner only sees the
/// level's attribute keys.
pub fn json_key_set(body: &[u8]) -> Vec<u32> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i + 11 <= body.len() {
        if body[i] == b'"'
            && body[i + 1..i + 9].iter().all(|&b| is_hex(b))
            && body[i + 9] == b'"'
            && body[i + 10] == b':'
        {
            let mut v: u32 = 0;
            for &b in &body[i + 1..i + 9] {
                v = (v << 4) | hex_val(b) as u32;
            }
            keys.push(v);
            i += 11;
        } else {
            i += 1;
        }
    }
    keys
}

/// Scan a DICOM XML body for `tag="` + 8 hex digits + `"` attributes (the
/// `<DicomAttribute>` keys of every level). Escaped text cannot fake the
/// token (quotes in text are `&quot;`), so the scan is exact.
pub fn xml_tag_set(body: &[u8]) -> Vec<u32> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i + 14 <= body.len() {
        if &body[i..i + 5] == b"tag=\""
            && body[i + 5..i + 13].iter().all(|&b| is_hex(b))
            && body[i + 13] == b'"'
        {
            let mut v: u32 = 0;
            for &b in &body[i + 5..i + 13] {
                v = (v << 4) | hex_val(b) as u32;
            }
            keys.push(v);
            i += 14;
        } else {
            i += 1;
        }
    }
    keys
}

/// Assert every tag of `baseline` is present in `keys`; the failure message
/// lists the missing tags in hex. Apply to the baseline-intersect-data
/// expectation (see the module doc) to avoid false failures on corpora whose
/// R attributes are legitimately omitted.
pub fn assert_baseline(keys: &[u32], baseline: &[u32]) {
    let missing: Vec<String> = baseline
        .iter()
        .filter(|t| !keys.contains(t))
        .map(|t| format!("{t:08X}"))
        .collect();
    assert!(
        missing.is_empty(),
        "mandatory baseline tags missing from response: {}",
        missing.join(", ")
    );
}
