// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Zero-heap verification for the manifest cache HIT path.
//!
//! `clarus_core::manifest_cache::get_or_load` runs on every WADO read, and
//! the widening of the slot bound (C8) added work to the INSERT path only.
//! The hit path must stay exactly as it was: a `HashMap::lookup`, a path
//! comparison against the entry's stored `PathBuf`, a `Relaxed` ticket store
//! and an `Arc::clone`. None of those touch the heap.
//!
//! The gate matters because the obvious way to bound the map - collecting
//! candidates, sorting by ticket, rebuilding the map - would put an
//! allocation on a path that runs per read. The test pins the hit path only:
//! the miss path decodes a manifest and allocates by design.
//!
//! Note on the allocator: `TestAllocator` counts ALL threads, so the window
//! is only meaningful on a quiet process. This binary runs a single test and
//! creates no threads, so the count is the current thread's.

use clarus_core::manifest::{Manifest, write_manifest};
use clarus_core::manifest_cache::get_or_load;
use clarus_core::stow::StudyManifestAttrs;
use ferrite_testkit::heap::TestAllocator;
use std::sync::Arc;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

fn sample_manifest(study: &str) -> Manifest {
    let attrs = StudyManifestAttrs {
        study_instance_uid: study.to_string(),
        ..StudyManifestAttrs::default()
    };
    clarus_core::stow::build_manifest_full(&attrs, &[])
}

#[test]
fn a_cache_hit_never_allocates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let study = "1.2.826.0.1.3680043.10.1234.9001";
    let path = dir.path().join(format!("{study}.manifest"));
    write_manifest(&path, &sample_manifest(study)).expect("write manifest");

    // Warm the slot: the FIRST call is a miss and decodes from disk.
    let warm = get_or_load(dir.path(), study).expect("miss decode");

    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let hit = get_or_load(dir.path(), study).expect("hit");
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);

    assert!(
        Arc::ptr_eq(&warm, &hit),
        "the hit path must return the cached Arc"
    );
    assert_eq!(
        before, after,
        "the hit path must not allocate (before: {before:?}, after: {after:?})"
    );
}
