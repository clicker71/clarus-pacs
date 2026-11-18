// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! zero-heap guarantee for the STOW-update hide-only refusal scan.
//!
//! `write_study_manifest_update` refuses a REPLACE when an incoming
//! SOPInstanceUID matches an existing instance whose rejection is hide-only
//! (113037 patient safety / 113038 incorrect MWL). The scan
//! (`refuse_hidden_reacceptance`) runs inside the coordinator lease on the
//! ingest hot path, so its accept side must allocate nothing:
//! `Manifest::find_instance` is a borrow-only linear scan and `is_hide_only` is
//! a u32 compare. The `RejectedForStore` String is cloned only on the refusal
//! path (rare, cold), outside the gated window.
//!
//! # Why the allocator lives in this binary
//!
//! `TestAllocator` is installed as this test binary's `#[global_allocator]`.
//! The library crate declares none, so no conflict arises.

use clarus_core::manifest::QUALITY_REASON_CODE;
use clarus_core::stow::{
    InstanceModel, SeriesModel, StudyManifestAttrs, build_manifest_full, refuse_hidden_reacceptance,
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

/// A one-instance manifest plus a matching incoming series, with the stored
/// instance's rejection reason set to `reason`.
fn accept_manifest(reason: u32) -> (clarus_core::manifest::Manifest, Vec<SeriesModel>) {
    let a = StudyManifestAttrs {
        study_instance_uid: "1.2.840.1.1".into(),
        ..Default::default()
    };
    let incoming = vec![SeriesModel {
        series_instance_uid: "1.2.840.1.2".into(),
        modality: "CT".into(),
        instances: vec![InstanceModel {
            sop_instance_uid: "1.2.840.1.3".into(),
            ..Default::default()
        }],
        ..Default::default()
    }];
    let mut manifest = build_manifest_full(&a, &incoming);
    manifest.series[0].instances[0].rejection_reason = reason;
    (manifest, incoming)
}

#[test]
fn hide_only_scan_accept_path_never_allocates() {
    // A visible instance and a Quality (113001) rejection are both
    // re-storable (non-hide-only): the scan must pass them without allocating.
    for reason in [0, QUALITY_REASON_CODE] {
        let (manifest, incoming) = accept_manifest(reason);
        let (zero_alloc, result) =
            allocate_env(|| refuse_hidden_reacceptance(&manifest, &incoming));
        assert!(
            zero_alloc,
            "accept-path scan must not allocate (reason {reason})"
        );
        assert!(
            result.is_ok(),
            "non-hide-only reasons must be accepted (reason {reason})"
        );
    }
}
