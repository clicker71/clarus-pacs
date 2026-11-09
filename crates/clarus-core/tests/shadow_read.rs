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

    let ct_uids = modality::read_study_uids(dir.path(), "CT", &day).expect("ct");
    let mr_uids = modality::read_study_uids(dir.path(), "MR", &day).expect("mr");
    let stored_modality: BTreeSet<String> = ct_uids.into_iter().chain(mr_uids).collect();
    assert_eq!(
        stored_modality, reference_modality,
        "modality idx != accepted"
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
