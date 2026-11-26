// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! shadow-read - the B-tree tier must reflect every study SQLite accepted.
//!
//! The B-tree tier is the index of record and the CBOR manifest is the
//! idx/ is a shadow mirror. The invariant that makes dual-write safe is:
//! **the set of studies accepted by SQLite == the set recorded in the B-tree
//! idx**, so a byte-for-byte QIDO comparison can never
//! diverge. This test models that contract without SQLite (clarus-core has no
//! DB dependency): we build an in-memory reference of accepted studies, feed
//! them through the identical dual-write path, then read the idx back and
//! assert the two sets agree.

use clarus_core::idx::date;
use clarus_core::idx::modality;
use clarus_core::idx::rollover::DailyIndexSet;
use std::collections::BTreeSet;

/// A study as it would be recorded by the STOW dual-write path.
struct AcceptedStudy {
    blake3: [u8; 32],
    modality: String,
    study_uid: String,
    day: [u8; 8],
}

/// Write `study` into the shadow idx via the production dual-write helper.
fn dual_write(shadow: &mut DailyIndexSet, study: &AcceptedStudy) {
    clarus_core::idx::rollover::append_date_row(shadow, || study.day, &study.blake3)
        .expect("date idx");
    let mut row = [0u8; 128];
    let filled = modality::format_row(&study.study_uid, &mut row);
    shadow
        .modality_writer(&study.modality)
        .expect("modality idx")
        .append(filled)
        .expect("modality row");
}

#[test]
fn shadow_tier_matches_sqlite_accepted_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let day = *b"20240115";
    let mut shadow = DailyIndexSet::new(dir.path().to_path_buf(), day);

    // The reference set SQLite accepted (in-memory model of the DB response).
    let mut reference_dates: BTreeSet<[u8; 32]> = BTreeSet::new();
    let mut reference_modality: BTreeSet<String> = BTreeSet::new();

    // 1) Smoke the dual-write path with a handful of representative studies.
    let mut studies = Vec::new();
    for i in 0..8u8 {
        let mut blob = [0u8; 32];
        blob[0] = i;
        blob[31] = i.wrapping_mul(7);
        let study = AcceptedStudy {
            blake3: blob,
            modality: if i % 2 == 0 { "CT".into() } else { "MR".into() },
            study_uid: format!("1.2.840.1.{}", i),
            day,
        };
        reference_dates.insert(blob);
        reference_modality.insert(study.study_uid.clone());
        dual_write(&mut shadow, &study);
        studies.push(study);
    }

    // 2) Shadow-read: pull the B-tree contents back and compare.
    let date_rows = date::read_hashes(dir.path(), &day).expect("date readback");
    let stored_dates: BTreeSet<[u8; 32]> = date_rows.into_iter().collect();
    assert_eq!(stored_dates, reference_dates, "dates idx != accepted set");

    // Compare EACH bucket with its own expected set. The previous union of
    // CT+MR hid cross-contamination entirely: a CT study also written into MR
    // still appeared once in the union, so the set comparison passed while the
    // MR accelerator answered lookups with a study that is not MR.
    let ct_uids = modality::read_study_uids(dir.path(), "CT", &day).expect("ct");
    let mr_uids = modality::read_study_uids(dir.path(), "MR", &day).expect("mr");
    let expected_ct: BTreeSet<String> = reference_modality
        .iter()
        .filter(|u| {
            studies
                .iter()
                .any(|s| s.modality == "CT" && &s.study_uid == *u)
        })
        .cloned()
        .collect();
    let expected_mr: BTreeSet<String> = reference_modality
        .iter()
        .filter(|u| {
            studies
                .iter()
                .any(|s| s.modality == "MR" && &s.study_uid == *u)
        })
        .cloned()
        .collect();
    // Sorted vectors, not sets: a DUPLICATE row inside one bucket must stay
    // visible instead of collapsing.
    let mut ct_sorted = ct_uids.clone();
    ct_sorted.sort();
    let mut mr_sorted = mr_uids.clone();
    mr_sorted.sort();
    assert_eq!(
        ct_sorted,
        expected_ct.iter().cloned().collect::<Vec<_>>(),
        "the CT bucket must hold exactly the CT studies, once each"
    );
    assert_eq!(
        mr_sorted,
        expected_mr.iter().cloned().collect::<Vec<_>>(),
        "the MR bucket must hold exactly the MR studies, once each"
    );
    assert!(
        expected_ct.is_disjoint(&expected_mr),
        "the fixture itself must not put a UID in both buckets"
    );

    // 3) Every study appears exactly in the right bucket (no cross-contamination).
    for s in &studies {
        let bucket = modality::read_study_uids(dir.path(), &s.modality, &day).expect("bucket read");
        assert!(
            bucket.iter().any(|u| u == &s.study_uid),
            "{} missing from {} bucket",
            s.study_uid,
            s.modality
        );
    }
    assert!(!studies.is_empty(), "dual-write must record studies");
}
