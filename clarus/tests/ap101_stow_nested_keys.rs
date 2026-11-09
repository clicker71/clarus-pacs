// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: the nested-sequence capture added to the DICOM parse hot path.
//!
//! PS3.18 Table 10.6.1-5 types `00400275.00400009`, `00400275.00401001` and
//! `00101002.00100020` as matching keys, so `extract_metadata` reads three
//! more scalars - two from the first item of RequestAttributesSequence
//! (0040,0275) and one from every item of OtherPatientIDsSequence (0010,1002).
//! The parse runs once per STOWed instance, which is the hottest path the
//! server has, so the added walk must not allocate.
//!
//! The gate measures the DIFFERENCE the capture adds. The two nested scalars
//! of RequestAttributesSequence are `Option<String>` values the parser has to
//! own, and each item of OtherPatientIDsSequence contributes one more owned
//! value - so the honest statement is "the walk adds one allocation per value
//! collected and no per-element scratch", not "the parse became free". The
//! second test pins that rate exactly; a helper that started building a
//! temporary `Vec` or formatting a tag per element would raise it.

use clarus_adapters::dicom_rs::DicomRsParser;
use clarus_ports::dicom::DicomParser;
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

/// Append a defined-length explicit-VR element.
fn element(buf: &mut Vec<u8>, group: u16, elem: u16, vr: &[u8; 2], value: &[u8]) {
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&elem.to_le_bytes());
    buf.extend_from_slice(vr);
    let is_long = matches!(
        std::str::from_utf8(vr).unwrap_or(""),
        "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
    );
    if is_long {
        buf.extend_from_slice(&[0u8, 0u8]);
        buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
    } else {
        buf.extend_from_slice(&(value.len() as u16).to_le_bytes());
    }
    buf.extend_from_slice(value);
    if !value.len().is_multiple_of(2) {
        buf.push(0x00);
    }
}

/// Append a defined-length item header carrying `body`.
fn item(buf: &mut Vec<u8>, body: &[u8]) {
    buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
    buf.extend_from_slice(&0xE000u16.to_le_bytes());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(body);
}

/// A minimal instance. `ras` adds RequestAttributesSequence (0040,0275) with
/// its (0040,0009)/(0040,1001) scalars; `items` adds that many
/// OtherPatientIDsSequence (0010,1002) items.
fn instance(ras: bool, items: usize, _unused: bool) -> Vec<u8> {
    let mut d = Vec::new();
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    element(&mut d, 0x0008, 0x0018, b"UI", b"1.2.3.4.5");
    element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    element(&mut d, 0x0010, 0x0010, b"PN", b"DOE^JOHN");
    element(&mut d, 0x0010, 0x0020, b"LO", b"PAT-1");
    element(&mut d, 0x0020, 0x000D, b"UI", b"1.2.3");
    element(&mut d, 0x0020, 0x000E, b"UI", b"1.2.3.4");

    if ras {
        let mut inner = Vec::new();
        element(&mut inner, 0x0040, 0x0009, b"SH", b"SPS-7");
        element(&mut inner, 0x0040, 0x1001, b"SH", b"RP-7");
        let mut ras_item = Vec::new();
        item(&mut ras_item, &inner);
        element(&mut d, 0x0040, 0x0275, b"SQ", &ras_item);
    }

    if items > 0 {
        let mut ids = Vec::new();
        for i in 0..items {
            let mut one = Vec::new();
            let value = format!("OTHER-{i}");
            element(&mut one, 0x0010, 0x0020, b"LO", value.as_bytes());
            item(&mut ids, &one);
        }
        element(&mut d, 0x0010, 0x1002, b"SQ", &ids);
    }
    d
}

/// The RequestAttributesSequence scalars cost exactly two allocations - the two
/// owned values - and nothing else.
#[test]
fn the_request_attributes_scalars_cost_one_allocation_each() {
    let plain = instance(false, 0, false);
    let ras_only = instance(true, 0, true);

    let _ = DicomRsParser.extract_metadata(&plain).expect("warm plain");
    let _ = DicomRsParser.extract_metadata(&ras_only).expect("warm ras");

    let (plain_allocs, _) = count_env(|| DicomRsParser.extract_metadata(&plain));
    let (ras_allocs, meta) = count_env(|| DicomRsParser.extract_metadata(&ras_only));
    let meta = meta.expect("ras parse");
    assert_eq!(meta.scheduled_procedure_step_id.as_deref(), Some("SPS-7"));
    assert_eq!(meta.requested_procedure_id.as_deref(), Some("RP-7"));
    assert!(meta.other_patient_ids.is_empty());

    assert_eq!(
        ras_allocs - plain_allocs,
        2,
        "two owned scalars, no scratch (plain={plain_allocs}, ras={ras_allocs})"
    );

    // The window is live: an allocating body inside it must be observed.
    let (over, _) = count_env(|| {
        let v: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(&v);
    });
    assert!(over > 0, "the window must observe allocations");

    println!("AP101 RAS scalars: {ras_allocs} allocs vs plain {plain_allocs} (+2)");
}

/// One instance, one file: the multi-valued walk costs one owned value per item
/// plus the `Vec`'s own doubling growth. At 16 items the capacity has already
/// doubled to 16, so the count is the plain parse plus the two RAS scalars,
/// sixteen values and the four capacity steps - a fixed, predicted number.
#[test]
fn the_item_walk_costs_one_value_per_item_plus_vector_growth() {
    let plain = instance(false, 0, false);
    let ras_only = instance(true, 0, true);
    let small = instance(true, 2, true);
    let large = instance(true, 16, true);

    for d in [&plain, &ras_only, &small, &large] {
        let _ = DicomRsParser.extract_metadata(d).expect("warm");
    }

    let (plain_allocs, _) = count_env(|| DicomRsParser.extract_metadata(&plain));
    let (ras_allocs, _) = count_env(|| DicomRsParser.extract_metadata(&ras_only));
    let (small_allocs, m_small) = count_env(|| DicomRsParser.extract_metadata(&small));
    let (large_allocs, m_large) = count_env(|| DicomRsParser.extract_metadata(&large));
    assert_eq!(m_small.expect("small").other_patient_ids.len(), 2);
    assert_eq!(m_large.expect("large").other_patient_ids.len(), 16);

    // Two items: the two RAS scalars, two owned values, and the vector's
    // first growth (an empty `Vec` reaching capacity 4 in one step).
    assert_eq!(
        small_allocs - plain_allocs,
        2 + 2 + 1,
        "small file budget (plain={plain_allocs}, small={small_allocs})"
    );
    // Sixteen items: the two RAS scalars, sixteen values, and three further
    // growth steps (4 -> 8 -> 16) after the first.
    assert_eq!(
        large_allocs - plain_allocs,
        2 + 16 + 1 + 2,
        "large file budget (plain={plain_allocs}, large={large_allocs})"
    );
    // The RAS-only delta is the two owned scalars, independent of the vector.
    assert_eq!(ras_allocs - plain_allocs, 2, "RAS scalars only");

    println!(
        "AP101 item walk: plain={plain_allocs} ras={ras_allocs} \
         2items={small_allocs} 16items={large_allocs}"
    );
}
