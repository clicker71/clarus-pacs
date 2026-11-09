// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Zero-heap verification for the pre-built query filter hotpath.
//!
//! `QueryFilter::build_expected()` builds a `HashMap<u32, Predicate>` **once**
//! per search; `QueryFilter::matches_prebuilt()` runs that map against each
//! candidate manifest's study attributes. The win is that the map is
//! reused across all candidates - but the per-candidate match must still be
//! allocation-free to pay for the save. This test enforces exactly that.
//!
//! # Which predicates are (and are not) zero-alloc
//!
//! The truly allocation-free predicate forms are those a hot `modality + date`
//! search exercises:
//! - `Predicate::CommaList` (Modality) - `pat.split(',').any(...)`, no alloc.
//! - `Predicate::DateRange` (StudyDate) - numeric range compare, no alloc.
//!
//! Two predicate forms allocate **by construction**, so they are deliberately
//! *not* asserted as zero-alloc here and are documented instead:
//! - `Predicate::Glob` runs `glob_match`, which collects `Vec<char>` for the
//!   pattern and the text before backtracking.
//! - `Predicate::Fuzzy` lowercases both sides (`to_lowercase()`) before Bitap.
//!
//! The ModalitiesInStudy fix (23.08) moved 0008,0061 out of the expected map
//! into a per-candidate field check (`QueryFilter::modalities_in_study_match`,
//! split/trim only, zero-alloc) applied by `match_study_attrs[_prebuilt]` -
//! asserted here in its own window.
//!
//! `build_expected()` itself allocates a `HashMap` once per search (the cost
//! the hotpath amortises), so it is never wrapped in a measured window.

use clarus_core::manifest::{AttrValue, Manifest, StudyQido, SysHeader};
use clarus_core::query::QueryFilter;
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// Run `compute` inside an inline allocation window, asserting zero allocs.
///
/// The window is inline (not a closure-capture wrapper) so the guarded call is
/// the only thing measured - the deterministic configuration `TestAllocator`
/// supports on a single live libtest thread.
fn assert_zero_alloc(compute: impl FnOnce() -> usize) -> usize {
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let n = compute();
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    assert_eq!(
        before, after,
        "guarded window must not allocate (before: {before:?}, after: {after:?})"
    );
    n
}

/// A study carrying the two attributes a `modality + date` hot query matches.
fn hot_search_manifest() -> Manifest {
    Manifest {
        sys: SysHeader {
            version: 1,
            storage_tier: 1,
            total_pixel_bytes: 0,
            arrived_at: 0,
        },
        study: StudyQido {
            attrs: vec![
                (0x0008_0020, AttrValue::UInt(20_260_802)),  // StudyDate
                (0x0008_0060, AttrValue::Text("CT".into())), // Modality
                (0x0010_0010, AttrValue::Text("Иванов".into())), // PatientName
                (0x0020_000D, AttrValue::Text("1.2.840.1.1".into())), // StudyUID
            ],
            ..Default::default()
        },
        series: vec![],
    }
}

#[test]
fn matches_prebuilt_is_allocation_free_on_multiple_candidates() {
    ALLOC.set_enabled(false);

    let m = hot_search_manifest();
    let filter = QueryFilter {
        modality: Some("CT".into()),
        study_date_from: Some("20260101".into()),
        study_date_to: Some("20261231".into()),
        ..Default::default()
    };

    // Build the expected map ONCE (allocates a HashMap - outside any window).
    let expected = filter.build_expected();
    // `Predicate` is `Copy` but not `PartialEq`, so assert on key presence and
    // the match outcome rather than struct equality.
    assert!(expected.contains_key(&0x0008_0060), "modality key present");
    assert!(
        expected.contains_key(&0x0008_0020),
        "study-date key present"
    );
    assert_eq!(expected.len(), 2, "only two search keys expected");

    // The per-candidate match runs against the same `&expected` map across a
    // batch of candidates - the hotpath. It must not allocate on any
    // candidate, no matter how many are matched in a loop.
    let matches = assert_zero_alloc(|| {
        let mut n = 0usize;
        for _ in 0..128 {
            if filter.matches_prebuilt(&m.study.attrs, &expected) {
                n += 1;
            }
        }
        n
    });
    assert_eq!(matches, 128, "every candidate must match the hot query");

    // A batch of *negative* candidates (wrong modality / wrong date) exercises
    // the predicate short-circuit path, which must also be allocation-free.
    let miss = Manifest {
        study: StudyQido {
            attrs: vec![
                (0x0008_0020, AttrValue::UInt(19_990_101)), // StudyDate < from
                (0x0008_0060, AttrValue::Text("MR".into())), // Modality != CT
            ],
            ..Default::default()
        },
        ..m
    };
    let matched = assert_zero_alloc(|| {
        let mut n = 0usize;
        for _ in 0..128 {
            if filter.matches_prebuilt(&miss.study.attrs, &expected) {
                n += 1;
            }
        }
        n
    });
    assert_eq!(
        matched, 0,
        "a mismatch must not allocate and must not match"
    );

    // Sanity: the ordinary exact-match path agrees with the budgeted forms.
    assert!(filter.matches_prebuilt(&m.study.attrs, &expected));
}

/// The ModalitiesInStudy fix (23.08): `match_study_attrs_prebuilt` now also
/// checks the derived `StudyQido.modalities_in_study` field next to the
/// pre-built map. That check (split/trim/any over borrowed strings) must be
/// as allocation-free as the predicate match, across a batch of candidates -
/// both for a combined modality+field query and for a field-only query whose
/// expected map is empty (the field check is then the sole decider).
#[test]
fn match_study_attrs_prebuilt_zero_alloc_with_modalities_in_study() {
    ALLOC.set_enabled(false);

    let m = Manifest {
        study: StudyQido {
            attrs: vec![(0x0008_0060, AttrValue::Text("CT".into()))],
            modalities_in_study: Some("CT,MR".into()),
            ..Default::default()
        },
        ..hot_search_manifest()
    };
    let filter = QueryFilter {
        modalities_in_study: Some("CT".into()),
        modality: Some("CT".into()),
        ..Default::default()
    };
    let expected = filter.build_expected();

    let matches = assert_zero_alloc(|| {
        let mut n = 0usize;
        for _ in 0..128 {
            if clarus_core::query::match_study_attrs_prebuilt(&filter, &m, &expected) {
                n += 1;
            }
        }
        n
    });
    assert_eq!(matches, 128, "every candidate must match");

    // Field-only query: the expected map is empty, the field check decides.
    let field_only = QueryFilter {
        modalities_in_study: Some("CT".into()),
        ..Default::default()
    };
    let expected2 = field_only.build_expected();
    assert!(expected2.is_empty());
    let matches2 = assert_zero_alloc(|| {
        let mut n = 0usize;
        for _ in 0..128 {
            if clarus_core::query::match_study_attrs_prebuilt(&field_only, &m, &expected2) {
                n += 1;
            }
        }
        n
    });
    assert_eq!(matches2, 128);

    // A stored aggregate without the requested modality must not match and
    // must not allocate either.
    let miss = QueryFilter {
        modalities_in_study: Some("DX".into()),
        ..Default::default()
    };
    let expected3 = miss.build_expected();
    let matched3 = assert_zero_alloc(|| {
        let mut n = 0usize;
        for _ in 0..128 {
            if clarus_core::query::match_study_attrs_prebuilt(&miss, &m, &expected3) {
                n += 1;
            }
        }
        n
    });
    assert_eq!(matched3, 0, "DX is not in the CT,MR aggregate");
}

/// FR-vet stage 1 (ResponsiblePerson 0010,2297): a manifest that CARRIES the
/// veterinary PN tag must not introduce any per-candidate allocation on the
/// hot many-manifest match path, and a `modality+date` query must not grow
/// its expected map with the new field when it is absent (the search key is
/// not set). A ResponsiblePerson *query* itself goes through
/// `name_predicate` -> `Glob`/`Fuzzy`, which allocate by construction exactly
/// like the other PN search keys (PatientName / ReferringPhysicianName); those
/// PN matches are deliberately not asserted zero-alloc here (see the module
/// doc). This window pins the two guarantees that DO hold: the storage side
/// (a stored vet tag costs nothing to scan) and the no-regression side (an
/// unset ResponsiblePerson adds no key to the hot expected map).
#[test]
fn manifest_carrying_responsible_person_zero_alloc_on_hot_match() {
    ALLOC.set_enabled(false);

    let m = Manifest {
        study: StudyQido {
            attrs: vec![
                (0x0008_0020, AttrValue::UInt(20_260_802)),  // StudyDate
                (0x0008_0060, AttrValue::Text("CT".into())), // Modality
                (0x0010_0010, AttrValue::Text("Иванов^Иван".into())),
                (0x0010_2297, AttrValue::Text("Петров^Пётр".into())), // vet PN tag
                (0x0020_000D, AttrValue::Text("1.2.840.1.1".into())), // StudyUID
            ],
            ..Default::default()
        },
        ..hot_search_manifest()
    };

    // A plain modality+date hot query (ResponsiblePerson unset).
    let filter = QueryFilter {
        modality: Some("CT".into()),
        study_date_from: Some("20260101".into()),
        study_date_to: Some("20261231".into()),
        ..Default::default()
    };
    let expected = filter.build_expected();
    // An unset ResponsiblePerson must NOT add its tag to the expected map.
    assert_eq!(
        expected.len(),
        2,
        "only modality + study-date keys expected"
    );
    assert!(
        !expected.contains_key(&0x0010_2297),
        "unset ResponsiblePerson must not enter the expected map"
    );

    // The many-manifest scan over a candidate carrying the vet tag stays 0-alloc.
    let matches = assert_zero_alloc(|| {
        let mut n = 0usize;
        for _ in 0..128 {
            if clarus_core::query::match_study_attrs_prebuilt(&filter, &m, &expected) {
                n += 1;
            }
        }
        n
    });
    assert_eq!(matches, 128, "every candidate must match the hot query");
}

/// 08.09 (Stage A ap101 gate): the PatientBirthDate range key rides the
/// prebuilt path exactly like StudyDate - `Predicate::DateRange` over the
/// stored UInt. `build_expected` maps the key once (outside any window);
/// the per-candidate `matches_prebuilt` is a pure numeric compare and must
/// not allocate for ANY candidate, matching or not.
#[test]
fn birth_date_range_prebuilt_is_allocation_free() {
    ALLOC.set_enabled(false);

    let m = Manifest {
        study: StudyQido {
            attrs: vec![
                (0x0008_0020, AttrValue::UInt(20_260_802)),  // StudyDate
                (0x0008_0060, AttrValue::Text("CT".into())), // Modality
                (0x0010_0030, AttrValue::UInt(19_800_101)),  // PatientBirthDate
                (0x0020_000D, AttrValue::Text("1.2.840.1.1".into())),
            ],
            ..Default::default()
        },
        ..hot_search_manifest()
    };

    let filter = QueryFilter {
        birth_date_from: Some("19700101".into()),
        birth_date_to: Some("19991231".into()),
        ..Default::default()
    };
    let expected = filter.build_expected();
    assert!(
        expected.contains_key(&0x0010_0030),
        "birth-date key present"
    );
    assert_eq!(expected.len(), 1, "only the birth-date key expected");

    let matches = assert_zero_alloc(|| {
        let mut n = 0usize;
        for _ in 0..128 {
            if filter.matches_prebuilt(&m.study.attrs, &expected) {
                n += 1;
            }
        }
        n
    });
    assert_eq!(matches, 128, "every candidate must match the hot query");

    // Negative candidate (birth date outside the range) - short-circuit path.
    let miss = Manifest {
        study: StudyQido {
            attrs: vec![(0x0010_0030, AttrValue::UInt(20_050_101))],
            ..Default::default()
        },
        ..m
    };
    let matched = assert_zero_alloc(|| {
        let mut n = 0usize;
        for _ in 0..128 {
            if filter.matches_prebuilt(&miss.study.attrs, &expected) {
                n += 1;
            }
        }
        n
    });
    assert_eq!(
        matched, 0,
        "a mismatch must not allocate and must not match"
    );
}
