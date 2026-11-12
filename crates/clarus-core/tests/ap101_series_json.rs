// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! zero-heap guarantee for series/instance QIDO emission and search.
//!
//! `write_series_json` / `write_instance_json` stream a series' or instance's
//! `(tag, AttrValue)` attributes into a **caller-owned fixed slice** with no
//! allocation (the study-level pattern, ap101_qido_stream).
//!
//! `search_series` / `search_instances` collect **borrowed references** into a
//! caller-owned `out` and sort in place via `sort_unstable_by`. At steady
//! state (a warm `out`, already-capacity) they allocate nothing - the same
//! borrow contract as `intersect_study_uids`. The first call may
//! grow the caller's `out`; the window below warms it first so the measured
//! call is steady-state.
//!
//! The owned `Manifest::decode_to_series`
//! allocates by construction and is outside this window; only the writers and
//! the borrow-form search are guarded here.
//!
//! # Why a single test function
//!
//! `TestAllocator` counts process-wide; libtest runs tests on separate threads.
//! Running every check within one function confines the measurement to a
//! single live libtest thread.

use clarus_core::manifest::{
    AttrValue, FrameRecord, InstanceRecord, Manifest, SeriesRecord, StudyQido, SysHeader, TIER_HOT,
    read_manifest_at,
};
use clarus_core::query::json::{write_instance_json, write_series_json};
use clarus_core::query::{InstanceFilter, SeriesFilter, search_instances, search_series};
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

/// Build a materialised (owned, fully decoded) manifest with two series and
/// multiple instances, exercising the captured attrs. Decode runs
/// outside every measured window.
fn decoded_manifest() -> Manifest {
    let mut blake3 = [0u8; 32];
    blake3[0] = 0xAB;
    blake3[31] = 0xEF;

    let m = Manifest {
        sys: SysHeader {
            version: 1,
            storage_tier: TIER_HOT,
            total_pixel_bytes: 2_000_000,
            arrived_at: 0,
        },
        study: StudyQido {
            attrs: vec![(0x0020_000D, AttrValue::Text("1.2.840.1.1".into()))],
            ..Default::default()
        },
        series: vec![
            SeriesRecord {
                attrs: vec![
                    (0x0008_0021, AttrValue::UInt(20260802)),    // SeriesDate
                    (0x0008_0060, AttrValue::Text("CT".into())), // Modality
                    (0x0008_103E, AttrValue::Text("Chest CT".into())),
                    (0x0020_000E, AttrValue::Text("1.2.840.1.2".into())),
                    (0x0020_0011, AttrValue::Text("1".into())), // SeriesNumber
                ],
                instances: vec![
                    InstanceRecord {
                        attrs: vec![
                            (
                                0x0008_0016,
                                AttrValue::Text("1.2.840.10008.5.1.4.1.1.2".into()),
                            ),
                            (0x0008_0018, AttrValue::Text("1.2.840.1.3".into())),
                            (0x0020_0013, AttrValue::Text("2".into())),
                        ],
                        transfer_syntax: "1.2.840.10008.1.2.1".into(),
                        storage_tier: TIER_HOT,
                        rejection_reason: 0,
                        blake3,
                        file_size: 1_000_000,
                        frames: vec![FrameRecord {
                            number: 1,
                            hash: blake3,
                            offset: 0,
                            compressed_len: 1_000_000,
                        }],
                    },
                    InstanceRecord {
                        attrs: vec![
                            (0x0008_0018, AttrValue::Text("1.2.840.1.4".into())),
                            (0x0020_0013, AttrValue::Text("1".into())),
                        ],
                        transfer_syntax: "1.2.840.10008.1.2.1".into(),
                        storage_tier: TIER_HOT,
                        rejection_reason: 0,
                        blake3,
                        file_size: 1_000_000,
                        frames: Vec::new(),
                    },
                ],
            },
            SeriesRecord {
                attrs: vec![
                    (0x0008_0060, AttrValue::Text("MR".into())),
                    (0x0008_103E, AttrValue::Text("Head".into())),
                    (0x0020_000E, AttrValue::Text("1.2.840.1.5".into())),
                ],
                instances: Vec::new(),
            },
        ],
    };

    let bytes = m.encode().expect("encode");
    read_manifest_at(&bytes).expect("decode")
}

#[test]
fn series_instance_qido_is_allocation_free() {
    // Build fixtures while counting is disabled.
    ALLOC.set_enabled(false);
    let manifest = decoded_manifest();

    // -- Series JSON into a fixed buffer: 0 alloc, valid DICOMweb JSON --
    let (zero_alloc, n) = allocate_env(|| {
        let mut buf = [0u8; 2048];
        let n = write_series_json(&mut buf, &manifest.series[0], None, None, None).expect("fits");
        std::hint::black_box(&buf);
        n
    });
    assert!(zero_alloc, "write_series_json must not allocate");
    assert!(n > 0);

    // -- Instance JSON into a fixed buffer: 0 alloc --
    let (zero_alloc, n) = allocate_env(|| {
        let mut buf = [0u8; 2048];
        let n = write_instance_json(
            &mut buf,
            &manifest.series[0].instances[0],
            "1.2.840.1.1",
            "1.2.840.1.2",
            None,
            None,
        )
        .expect("fits");
        std::hint::black_box(&buf);
        n
    });
    assert!(zero_alloc, "write_instance_json must not allocate");
    assert!(n > 0);

    // -- Filtered writers (includefield + mandatory baseline): 0 alloc --
    let (zero_alloc, n) = allocate_env(|| {
        let mut buf = [0u8; 2048];
        let n = write_series_json(&mut buf, &manifest.series[0], Some("0008103E"), None, None)
            .expect("fits");
        std::hint::black_box(&buf);
        n
    });
    assert!(zero_alloc, "filtered write_series_json must not allocate");
    assert!(n > 0);

    let (zero_alloc, n) = allocate_env(|| {
        let mut buf = [0u8; 2048];
        let n = write_instance_json(
            &mut buf,
            &manifest.series[0].instances[0],
            "1.2.840.1.1",
            "1.2.840.1.2",
            Some("00080018"),
            None,
        )
        .expect("fits");
        std::hint::black_box(&buf);
        n
    });
    assert!(zero_alloc, "filtered write_instance_json must not allocate");
    assert!(n > 0);

    // -- Borrow-form series search: warm `out` once, then measure steady state --
    let mut out: Vec<&SeriesRecord> = Vec::new();
    // Warm-up outside the window (may grow `out`).
    ALLOC.set_enabled(false);
    search_series(&manifest, &SeriesFilter::default(), &mut out);
    ALLOC.set_enabled(true);
    let (zero_alloc, count) = allocate_env(|| {
        search_series(&manifest, &SeriesFilter::default(), &mut out);
        out.len()
    });
    assert!(zero_alloc, "search_series (steady state) must not allocate");
    assert_eq!(count, 2);

    // -- Borrow-form instance search: warm once, then measure --
    let mut iout: Vec<&InstanceRecord> = Vec::new();
    ALLOC.set_enabled(false);
    search_instances(
        &manifest,
        "1.2.840.1.2",
        &InstanceFilter::default(),
        &mut iout,
        false,
    );
    ALLOC.set_enabled(true);
    let (zero_alloc, count) = allocate_env(|| {
        search_instances(
            &manifest,
            "1.2.840.1.2",
            &InstanceFilter::default(),
            &mut iout,
            false,
        );
        iout.len()
    });
    assert!(
        zero_alloc,
        "search_instances (steady state) must not allocate"
    );
    assert_eq!(count, 2);

    // -- Borrow-form series search with a filter: warm once, then measure --
    let mut fout: Vec<&SeriesRecord> = Vec::new();
    let f = SeriesFilter {
        series_description: Some("Chest*".into()),
        ..Default::default()
    };
    ALLOC.set_enabled(false);
    search_series(&manifest, &f, &mut fout);
    ALLOC.set_enabled(true);
    let (zero_alloc, count) = allocate_env(|| {
        search_series(&manifest, &f, &mut fout);
        fout.len()
    });
    assert!(
        zero_alloc,
        "filtered search_series (steady state) must not allocate"
    );
    assert_eq!(count, 1);

    // Sanity on the emitted bytes (allocation outside the windows).
    let mut buf = [0u8; 2048];
    let n = write_series_json(&mut buf, &manifest.series[0], None, None, None).expect("fits");
    let text = String::from_utf8(buf[..n].to_vec()).unwrap();
    assert!(text.contains("\"0020000E\":{\"Value\":[\"1.2.840.1.2\"],\"vr\":\"UI\"}"));
    assert!(text.contains("\"00200011\":{\"Value\":[1],\"vr\":\"IS\"}"));

    // Filtered output keeps the mandatory baseline.
    let mut fbuf = [0u8; 2048];
    let fn_ = write_series_json(&mut fbuf, &manifest.series[0], Some("0008103E"), None, None)
        .expect("fits");
    let ftext = String::from_utf8(fbuf[..fn_].to_vec()).unwrap();
    assert!(
        ftext.contains("\"0020000E\""),
        "mandatory SeriesInstanceUID must survive includefield: {ftext}"
    );
}
