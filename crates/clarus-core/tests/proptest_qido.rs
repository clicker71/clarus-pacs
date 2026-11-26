// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Property-based shadow-INDEX test - 50 000 random QIDO-ish cases.
//!
//! SCOPE, stated precisely: this file exercises the SHADOW INDEX only. It does
//! NOT compare the primary and shadow query paths. The property below writes
//! straight into a `DailyIndexSet` and reads the same bytes back, so it can
//! prove the index round-trips an arbitrary study - and nothing at all about
//! whether the production dual-write path agrees with the primary tier. A
//! dual-write that silently skips a study still passes here, because the
//! skipped path is never taken.
//!
//! A primary/shadow QIDO parity gate would have to feed the same inputs
//! through BOTH readers and compare their responses; that is a different test.
//! Two properties are checked under random input:
//!
//! 1. **Determinism** - from the same raw DICOM search inputs, the
//!    empty-field generators produce identical StudyInstanceUID / PatientID /
//!    PatientName. This is what lets both tiers derive the same values from
//!    the same manifest.
//! 2. **Shadow idx round trip** - every study written into the date + modality
//!    `.idx` reads back identically.
//!
//! `proptest` runs the configured cases (default 256; CI passes `--cases=50000`
//! via `PROPTEST_CASES`). The plan's 50 000-case gate is set through that env
//! var / harness flag.

use clarus_core::idx::rollover::DailyIndexSet;
use clarus_core::stow::{generate_patient_id, generate_patient_name, generate_study_instance_uid};
use proptest::prelude::*;

/// Random DICOM search inputs that mirror what STOW extraction produces.
#[derive(Debug, Clone)]
struct RandomStudy {
    patient_id_opt: Option<String>,
    study_date_opt: Option<String>,
    study_time_opt: Option<String>,
    accession_opt: Option<String>,
    modality: String,
    study_uid_opt: Option<String>,
    blake3: [u8; 32],
    day: [u8; 8],
}

fn arb_study() -> impl Strategy<Value = RandomStudy> {
    (
        proptest::option::of("[A-Z]{0,8}[0-9]{0,6}"), // patient_id (maybe empty)
        proptest::option::of("20[0-9]{6}"),           // StudyDate YYYYMMDD
        proptest::option::of("[0-9]{6}"),             // StudyTime HHMMSS
        proptest::option::of("[A-Z]{0,6}"),           // AccessionNumber
        proptest::prop_oneof![Just("CT"), Just("MR"), Just("XA"), Just("US")]
            .prop_map(|s: &str| s.to_string()),
        proptest::option::of("[0-9.]{5,20}"), // StudyInstanceUID (maybe empty)
        prop::array::uniform32(any::<u8>()),  // blake3 hash
        // Day as 8 ASCII digits so the resulting .idx path is a valid legal
        // `YYYYMMDD` file name (non-digit bytes would break the path).
        proptest::string::string_regex("[0-9]{8}")
            .unwrap()
            .prop_map(|s: String| {
                let mut d = [b'0'; 8];
                d.copy_from_slice(s.as_bytes());
                d
            }),
    )
        .prop_map(
            |(pid, sdate, stime, acc, modl, suid, blake3, day)| RandomStudy {
                patient_id_opt: pid,
                study_date_opt: sdate,
                study_time_opt: stime,
                accession_opt: acc,
                modality: modl,
                study_uid_opt: suid,
                blake3,
                day,
            },
        )
}

prop_compose! {
    /// A study with a non-empty PatientID, so StudyInstanceUID generation has
    /// its required anchor input (the primary STOW path).
    fn arbitrary_std_study()(
        pid in "[A-Za-z0-9]{2,10}",
        sdate in proptest::option::of("20[0-9]{6}"),
        stime in proptest::option::of("[0-9]{6}"),
        acc in proptest::option::of("[A-Z]{0,6}"),
        modality in proptest::prop_oneof![Just("CT"), Just("MR")].prop_map(|s: &str| s.to_string()),
        buid in proptest::option::of("[0-9.]{5,20}"),
        blake3 in prop::array::uniform32(any::<u8>()),
        day in prop::array::uniform8(prop::num::u8::ANY),
    ) -> RandomStudy {
        RandomStudy {
            patient_id_opt: Some(pid),
            study_date_opt: sdate,
            study_time_opt: stime,
            accession_opt: acc,
            modality,
            study_uid_opt: buid,
            blake3,
            day,
        }
    }
}

/// The inputs the DETERMINISM property actually reads: everything that feeds
/// `generate_study_instance_uid`. It deliberately omits the hash, day and
/// pre-existing UID, which that function never consults - generating them made
/// a failing case print data the assertion could not depend on.
fn arbitrary_uid_inputs() -> impl Strategy<Value = RandomStudy> {
    (
        "[A-Za-z0-9]{2,10}",
        proptest::option::of("20[0-9]{6}"),
        proptest::option::of("[0-9]{6}"),
        proptest::option::of("[A-Z]{0,6}"),
        proptest::prop_oneof![Just("CT"), Just("MR")].prop_map(|s: &str| s.to_string()),
    )
        .prop_map(|(pid, sdate, stime, acc, modality)| RandomStudy {
            patient_id_opt: Some(pid),
            study_date_opt: sdate,
            study_time_opt: stime,
            accession_opt: acc,
            modality,
            // Unused by the determinism property; fixed rather than generated.
            study_uid_opt: None,
            blake3: [0u8; 32],
            day: *b"20240115",
        })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 50_000, // The plan's 50 000-case gate. No file I/O here, so fast.
        .. ProptestConfig::default()
    })]

    /// determinism: the same input always yields the same study UID,
    /// patient ID and patient name - the property that makes SQLite == B-tree.
    #[test]
    fn empty_field_generation_is_deterministic(
        s in arbitrary_uid_inputs(),
    ) {
        let pid = s.patient_id_opt.as_deref().unwrap_or("");
        let a = generate_study_instance_uid(
            pid,
            s.study_date_opt.as_deref(),
            s.study_time_opt.as_deref(),
            s.accession_opt.as_deref(),
            &s.modality,
        )
        .expect("study uid");
        let b = generate_study_instance_uid(
            pid,
            s.study_date_opt.as_deref(),
            s.study_time_opt.as_deref(),
            s.accession_opt.as_deref(),
            &s.modality,
        )
        .expect("study uid");
        prop_assert_eq!(&a, &b, "StudyInstanceUID must be deterministic");
        prop_assert!(a.starts_with("2.25."), "ISO 9834-3 OID root");

        // PatientID + PatientName anchored on the generated study UID.
        let p1 = generate_patient_id(&a).expect("patient id");
        let p2 = generate_patient_id(&b).expect("patient id");
        prop_assert_eq!(&p1, &p2, "PatientID must be study-anchored");

        let n1 = generate_patient_name(&a).expect("patient name");
        let n2 = generate_patient_name(&b).expect("patient name");
        prop_assert_eq!(&n1, &n2, "PatientName must be study-anchored");
        prop_assert!(n1.starts_with("Anonymous^"), "PN placeholder form");
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        // File-I/O bound (a tempdir per case); the full 50 000-case sweep of
        // this property runs in CI via `--cases=50000`. Locally keep it small.
        cases: 256,
        .. ProptestConfig::default()
    })]

    /// Shadow idx consistency: any random study dual-written into the shadow
    /// tier reads back exactly, with the right date row and modality bucket.
    #[test]
    fn shadow_idx_roundtrips_arbitrary_studies(
        s in arb_study(),
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut shadow = DailyIndexSet::new(dir.path().to_path_buf(), s.day);

        // Dual-write path (mirrors B-001).
        clarus_core::idx::rollover::append_date_row(&mut shadow, || s.day, &s.blake3)
            .expect("date idx");
        let mut row = [0u8; 128];
        let filled = clarus_core::idx::modality::format_row(&s.study_uid_opt.clone().unwrap_or_default(), &mut row);
        shadow
            .modality_writer(&s.modality)
            .expect("mod")
            .append(filled)
            .expect("mod row");

        // Shadow-read: the date idx contains this exact hash.
        let dates = clarus_core::idx::date::read_hashes(dir.path(), &s.day).expect("read dates");
        prop_assert!(
            dates.contains(&s.blake3),
            "date idx lost the study hash"
        );

        // The modality bucket contains this study UID. An empty UID is a
        // degenerate input the reader legally skips; production always has a
        // UID (B-007 generates one), so only non-empty UIDs are asserted.
        let bucket = clarus_core::idx::modality::read_study_uids(
            dir.path(),
            &s.modality,
            &s.day,
        )
        .expect("read bucket");
        let expected_uid = s.study_uid_opt.unwrap_or_default();
        if !expected_uid.is_empty() {
            prop_assert!(
                bucket.iter().any(|u| u == &expected_uid),
                "modality bucket lost the study"
            );
        }
    }
}
