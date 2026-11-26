// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Zero-heap guarantee for the top-level (global) QIDO scan.
//!
//! `search_all_series` / `search_all_instances` scan **every** study manifest
//! in the archive (no study scope) and collect **borrowed references** into a
//! caller-owned `out`, sorted by UID. They are the in-core mirrors of the
//! per-study `search_series` / `search_instances` (both ap101-guarded in
//! `ap101_series_json.rs`) and therefore share the same borrow contract: at
//! steady state (a warm, already-capacity `out`) they allocate nothing.
//!
//! The owned `Manifest::decode_to_series` allocates by construction and is
//! outside every window; only the borrow-form scan is guarded here.

use clarus_core::manifest::{
    AttrValue, InstanceRecord, Manifest, SeriesRecord, StudyQido, SysHeader, TIER_HOT,
    read_manifest_at,
};
use clarus_core::query::{InstanceFilter, SeriesFilter, search_all_instances, search_all_series};
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

/// Build a decoded manifest with one series `uid` (modality `modality`,
/// description `desc`) holding one instance `sop`. Study UID is the fixed
/// `STUDY` (0020,000D) so all instances share it.
fn manifest(series_uid: &str, modality: &str, desc: &str, sop: &str) -> Manifest {
    const STUDY: &str = "1.2.840.1.7";
    let m = Manifest {
        sys: SysHeader {
            version: 1,
            storage_tier: TIER_HOT,
            total_pixel_bytes: 1_000,
            arrived_at: 0,
        },
        study: StudyQido {
            attrs: vec![(0x0020_000D, AttrValue::Text(STUDY.into()))],
            ..Default::default()
        },
        series: vec![SeriesRecord {
            attrs: vec![
                (0x0008_0060, AttrValue::Text(modality.into())),
                (0x0008_103E, AttrValue::Text(desc.into())),
                (0x0020_000E, AttrValue::Text(series_uid.into())),
            ],
            instances: vec![InstanceRecord {
                attrs: vec![(0x0008_0018, AttrValue::Text(sop.into()))],
                transfer_syntax: "1.2.840.10008.1.2.1".into(),
                storage_tier: TIER_HOT,
                rejection_reason: 0,
                blake3: [0u8; 32],
                file_size: 1_000,
                frames: Vec::new(),
            }],
        }],
    };
    let bytes = m.encode().expect("encode");
    read_manifest_at(&bytes).expect("decode")
}

/// Global series + instance scan stays allocation-free and UID-sorted.
#[test]
fn global_qido_scan_is_allocation_free_and_sorted() {
    // Fixtures while counting is disabled.
    ALLOC.set_enabled(false);
    let m_b = manifest("1.2.840.1.2", "CT", "Chest", "1.2.840.1.30");
    let m_a = manifest("1.2.840.1.1", "MR", "Head", "1.2.840.1.20");
    // REVERSE UID order. Handing the scans an already-ascending array made
    // every "sorted" assertion below pass for a scan that merely preserved
    // input order; with B first, only a real sort can produce A's UIDs at the
    // head of the results.
    let visible_sop = sop_uid_of(&m_a.series[0].instances[0]);
    let manifests = [m_b, m_a];

    // -- Global series scan: warm once, then measure steady state --
    let mut sout: Vec<clarus_core::query::GlobalSeries<'_>> = Vec::new();
    ALLOC.set_enabled(false);
    search_all_series(&manifests, &SeriesFilter::default(), &mut sout);
    ALLOC.set_enabled(true);
    let (zero_alloc, count) = allocate_env(|| {
        search_all_series(&manifests, &SeriesFilter::default(), &mut sout);
        sout.len()
    });
    let first = series_uid_of(sout[0].series); // outside the window (clones in fixture)
    assert!(
        zero_alloc,
        "search_all_series (steady state) must not allocate"
    );
    assert_eq!(count, 2, "all series across all manifests");
    assert_eq!(
        first.as_deref(),
        Some("1.2.840.1.1"),
        "SeriesInstanceUID ASC"
    );

    // -- Global series scan with a filter: only matching series survive --
    let mut fout: Vec<clarus_core::query::GlobalSeries<'_>> = Vec::new();
    let f = SeriesFilter {
        modality: Some("MR".into()),
        ..Default::default()
    };
    ALLOC.set_enabled(false);
    search_all_series(&manifests, &f, &mut fout);
    ALLOC.set_enabled(true);
    let (zero_alloc, count) = allocate_env(|| {
        search_all_series(&manifests, &f, &mut fout);
        fout.len()
    });
    assert!(
        zero_alloc,
        "filtered search_all_series (steady state) must not allocate"
    );
    assert_eq!(count, 1, "only the MR series");
    // WHICH series survived, not merely how many. A scan that returned only
    // the CT series still gives `count == 1`; the filter's job is to keep MR.
    let kept = series_uid_of(fout[0].series); // after the allocation window
    assert_eq!(
        kept.as_deref(),
        Some("1.2.840.1.1"),
        "the modality filter must retain the MR series"
    );
    let kept_modality = attr_text(&fout[0].series.attrs, 0x0008_0060);
    assert_eq!(
        kept_modality.as_deref(),
        Some("MR"),
        "the retained series must actually be the MR one"
    );

    // -- Global instance scan: warm once, then measure --
    let mut iout: Vec<clarus_core::query::GlobalInstance<'_>> = Vec::new();
    ALLOC.set_enabled(false);
    search_all_instances(
        &manifests,
        &InstanceFilter::default(),
        &SeriesFilter::default(),
        None,
        &mut iout,
        false,
    );
    ALLOC.set_enabled(true);
    let (zero_alloc, count) = allocate_env(|| {
        search_all_instances(
            &manifests,
            &InstanceFilter::default(),
            &SeriesFilter::default(),
            None,
            &mut iout,
            false,
        );
        iout.len()
    });
    let first_sop = sop_uid_of(iout[0].instance); // outside the window
    assert!(
        zero_alloc,
        "search_all_instances (steady state) must not allocate"
    );
    assert_eq!(count, 2, "all instances across all manifests");
    assert_eq!(
        first_sop.as_deref(),
        Some("1.2.840.1.20"),
        "SOPInstanceUID ASC"
    );

    // -- The hide_rejected=true branch must also be allocation-free --------
    // Measured on a fixture that ACTUALLY holds a rejected instance and a
    // visible one. The original block reused `manifests` (every fixture has
    // `rejection_reason: 0`), so it re-ran the plain scan and could never show
    // the filter doing anything: a scan that ignored the flag entirely passed.
    let mut seq = manifest("1.2.840.1.3", "CT", "Sequestrated", "1.2.840.1.40");
    seq.series[0].instances[0].rejection_reason = 0x0000_0001;
    // A rejected fixture ALONGSIDE the clean ones: two visible instances
    // (m_b, m_a) plus this one.
    let mixed = [manifests[0].clone(), manifests[1].clone(), seq];

    let mut rout: Vec<clarus_core::query::GlobalInstance<'_>> = Vec::new();
    ALLOC.set_enabled(false);
    search_all_instances(
        &mixed,
        &InstanceFilter::default(),
        &SeriesFilter::default(),
        None,
        &mut rout,
        false,
    );
    ALLOC.set_enabled(true);
    let (zero_alloc_hide, hidden_count) = allocate_env(|| {
        search_all_instances(
            &mixed,
            &InstanceFilter::default(),
            &SeriesFilter::default(),
            None,
            &mut rout,
            false,
        );
        rout.len()
    });
    assert!(
        zero_alloc_hide,
        "search_all_instances with hide_rejected=true must not allocate"
    );
    assert_eq!(
        hidden_count, 2,
        "the sequestered instance must be hidden from the scan"
    );
    let shown: Vec<Option<String>> = rout.iter().map(|r| sop_uid_of(r.instance)).collect();
    assert!(
        !shown.contains(&Some("1.2.840.1.40".to_string())),
        "the rejected SOPInstanceUID must not be returned: {shown:?}"
    );
    assert!(
        shown.contains(&visible_sop),
        "the visible instance must still be returned: {shown:?}"
    );

    // The borrowed Study/Series UIDs are injected for the renderer.
    assert_eq!(iout[0].study_uid, "1.2.840.1.7");
    assert_eq!(iout[0].series_uid, "1.2.840.1.1", "A's series UID");
    assert_eq!(iout[1].series_uid, "1.2.840.1.2", "B's series UID");
    // The series scan threads the same borrowed study UID per row.
    assert_eq!(sout[0].study_uid, "1.2.840.1.7");
    assert_eq!(sout[1].study_uid, "1.2.840.1.7");
}

/// Extract the SeriesInstanceUID text of a series record (test helper).
fn series_uid_of(s: &clarus_core::manifest::SeriesRecord) -> Option<String> {
    attr_text(&s.attrs, 0x0020_000E)
}

/// Extract the SOPInstanceUID text of an instance record (test helper).
fn sop_uid_of(i: &clarus_core::manifest::InstanceRecord) -> Option<String> {
    attr_text(&i.attrs, 0x0008_0018)
}

/// Text value of `tag` in an attrs slice (test helper).
fn attr_text(attrs: &[(u32, AttrValue)], tag: u32) -> Option<String> {
    for (t, v) in attrs {
        if *t == tag {
            if let AttrValue::Text(s) = v {
                return Some(s.clone());
            }
            return None;
        }
    }
    None
}
