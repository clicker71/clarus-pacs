// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! zero-heap guarantee for the WADO-facing `Manifest::find_instance`.
//!
//! QIDO's streaming path was already verified 0-alloc (ap101_qido_stream).
//! WADO's hot path resolves an instance by SOP Instance UID from a *decoded*
//! (owned, not mmap) manifest via `Manifest::find_instance`. That method must
//! allocate nothing: it is a short linear scan over the already-decoded
//! series/instance attribute vectors, returning a borrowed `&InstanceRecord`.
//!
//! # Why the allocator lives in this binary
//!
//! `TestAllocator` is installed as this test binary's `#[global_allocator]`.
//! The library crate declares none, so no conflict arises (the ap101 reference
//! examples do the same in their own test binaries).
//!
//! # Why a single test function
//!
//! `TestAllocator` counts process-wide, so a concurrent sibling test thread
//! could attribute its own allocation to a measured window. Running the check
//! within one test confines the measurement to a single live libtest thread.

use clarus_core::manifest::{
    AttrValue, FrameRecord, InstanceRecord, Manifest, SeriesRecord, StudyQido, SysHeader, TIER_HOT,
    read_manifest_at,
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

/// A fully populated, decoded manifest that mirrors a STOW-written study.
///
/// Each `InstanceRecord` carries the WADO-facing fields plus a SOP Instance
/// UID attribute (0008,0018) - exactly what a live accept writes.
fn decoded_manifest() -> Manifest {
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
                (0x0008_0020, AttrValue::UInt(20_260_802)),
                (0x0008_0060, AttrValue::Text("CT".into())),
                (0x0010_0010, AttrValue::Text("ivanov".into())),
                (0x0020_000d, AttrValue::Text("1.2.840.1.1".into())),
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
                    (0x0028_0010, AttrValue::UInt(512)),
                    (0x0028_0011, AttrValue::UInt(512)),
                ],
                transfer_syntax: "1.2.840.10008.1.2.1".into(),
                storage_tier: TIER_HOT,
                rejection_reason: 0,
                blake3,
                file_size: 2_000_000,
                frames: vec![FrameRecord {
                    number: 1,
                    hash: blake3,
                    offset: 0,
                    compressed_len: 1_059_968,
                }],
            }],
        }],
    };

    // Encode then decode: `find_instance` must be verified against a *decoded*
    // (owned, fully materialised) manifest, not the in-memory struct. Decode
    // allocates - it runs outside the guarded window, so it never pollutes the
    // 0-alloc measurement below.
    let bytes = manifest.encode().expect("encode");
    read_manifest_at(&bytes).expect("decode")
}

#[test]
fn find_instance_never_allocates() {
    let manifest = decoded_manifest();

    // Locate the known SOP present in the manifest.
    let (zero_alloc, found) = allocate_env(|| manifest.find_instance("1.2.840.1.3"));
    assert!(zero_alloc, "Manifest::find_instance must not allocate");
    let found = found.expect("instance must be present");
    assert_eq!(found.transfer_syntax, "1.2.840.10008.1.2.1");
    assert_eq!(found.storage_tier, TIER_HOT);
    assert_eq!(found.file_size, 2_000_000);

    // A miss is equally allocation-free.
    let (zero_alloc, _) = allocate_env(|| manifest.find_instance("9.9.9.9"));
    assert!(zero_alloc, "Manifest::find_instance miss must not allocate");
}
