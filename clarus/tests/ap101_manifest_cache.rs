// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! B3 ap101 gate: the decoded-manifest cache HIT path is zero-allocation
//! Decoded-manifest cache integration.
//!
//! The hit path is `RwLock` read acquire + `HashMap<String, _>.get(&str)`
//! (String: Borrow<str> - no alloc) + `Arc::clone` (atomic increment) +
//! `Manifest::find_instance` (already gated 0-alloc by ap101_wado_find).
//! The gate proves it, rather than asserting it in prose.
//!
//! The MISS path (decode from disk, owned strings) allocates by design and
//! is explicitly NOT gated.
//!
//! # Why a single test function
//!
//! `TestAllocator` counts process-wide; a concurrent sibling test thread
//! could attribute its own allocation to a measured window. One test
//! confines the measurement to a single live libtest thread. The cache
//! static is process-global, so the fixture is seeded inside the window's
//! test itself (miss outside the window, hit inside).

use clarus_core::manifest::{
    AttrValue, InstanceRecord, Manifest, SeriesRecord, StudyQido, SysHeader, TIER_HOT,
    write_manifest,
};
use clarus_core::manifest_cache::get_or_load;
use ferrite_testkit::heap::TestAllocator;
use std::path::Path;
use std::sync::Arc;

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

/// Write a minimal one-instance study manifest (fixture OUTSIDE the window).
fn write_fixture(dir: &Path, study: &str) {
    let mut blake3 = [0u8; 32];
    blake3[0] = 0xAB;
    blake3[31] = 0xEF;
    let manifest = Manifest {
        sys: SysHeader {
            version: 1,
            storage_tier: TIER_HOT,
            total_pixel_bytes: 2_000_000,
            arrived_at: 0,
        },
        study: StudyQido {
            attrs: vec![
                (0x0008_0060, AttrValue::Text("CT".into())),
                (0x0020_000d, AttrValue::Text(study.to_string())),
            ],
            ..Default::default()
        },
        series: vec![SeriesRecord {
            attrs: vec![(0x0020_000e, AttrValue::Text("1.2.840.1.2".into()))],
            instances: vec![InstanceRecord {
                attrs: vec![
                    (0x0008_0018, AttrValue::Text("1.2.840.1.3".into())),
                    (
                        0x0008_0016,
                        AttrValue::Text("1.2.840.10008.5.1.4.1.1.2".into()),
                    ),
                ],
                transfer_syntax: "1.2.840.10008.1.2.1".into(),
                storage_tier: TIER_HOT,
                rejection_reason: 0,
                blake3,
                file_size: 2_000_000,
                frames: Vec::new(),
            }],
        }],
    };
    let path = dir.join(format!("{study}.manifest"));
    write_manifest(&path, &manifest).expect("write fixture manifest");
}

#[test]
fn manifest_cache_hit_path_alloc_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_fixture(dir.path(), "gate.1");

    // Warm outside the window: the miss decodes + inserts (allocates by
    // design - explicitly not gated).
    let first = get_or_load(dir.path(), "gate.1").expect("miss decode");

    // The gated window: a warm hit = read lock + Borrow get + Arc::clone +
    // find_instance. Any allocation fails the gate.
    let (zero, (hit, found)) = allocate_env(|| {
        let m = get_or_load(dir.path(), "gate.1").expect("hit");
        let found = m.find_instance("1.2.840.1.3").is_some();
        (m, found)
    });
    assert!(
        zero,
        "manifest cache hit path must be zero-allocation (B3 ap101)"
    );
    assert!(found, "the cached instance must resolve");
    assert!(
        Arc::ptr_eq(&first, &hit),
        "the hit must clone the cached Arc (no re-decode)"
    );
}
