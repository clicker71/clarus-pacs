// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: the study-manifest restamp (STOW commit path) is zero-alloc on the
//! steady-state path.
//!
//! `restamp_from_manifest` feeds the RAM study index the MERGED disk truth
//! after every manifest commit. Another instance of the SAME study with
//! identical identity must not allocate (text compares borrowed, dates render
//! into a stack buffer); only a real divergence (a derived object whose
//! coerced placeholders differ from the archived truth) is allowed to clone.

use clarus_core::stow::manifest::{StudyManifestAttrs, build_manifest, restamp_from_manifest};
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

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

fn attrs() -> StudyManifestAttrs {
    StudyManifestAttrs {
        patient_name: Some("Clinfer^Transport^Demo".to_string()),
        patient_id: Some("CLIN-DEMO-001".to_string()),
        patient_id_issuer: Some("CLIN".to_string()),
        patient_birth_date: Some("19800101".to_string()),
        patient_sex: Some("O".to_string()),
        study_instance_uid: "1.2.840.113857.10.2026.9.17.1".to_string(),
        study_date: Some("20260917".to_string()),
        study_time: Some("201136".to_string()),
        study_description: Some("ChestX-Det10 demo (clinfer transport)".to_string()),
        accession_number: Some("ACC-1".to_string()),
        referring_physician: Some("REF^DOC".to_string()),
        performing_physician: Some("PERF^DOC".to_string()),
        name_of_reading_physicians: Some("READ^DOC".to_string()),
        institution_name: Some("CLIN-DEMO".to_string()),
        requesting_physician: Some("REQ^DOC".to_string()),
        requested_procedure_description: Some("CHEST".to_string()),
        responsible_person: Some("RESP^DOC".to_string()),
        modality: "DX".to_string(),
        study_id: Some("S1".to_string()),
        num_series: 2,
        num_instances: 8,
        modalities_in_study: Some("DX".to_string()),
        other_patient_ids: None,
    }
}

#[test]
fn steady_state_restamp_allocates_nothing() {
    let m = build_manifest(&attrs());
    let mut a = attrs();
    let (zero_alloc, ()) = allocate_env(|| {
        restamp_from_manifest(&mut a, &m);
    });
    assert!(zero_alloc, "identical identity must not allocate");
    assert_eq!(a.patient_id.as_deref(), Some("CLIN-DEMO-001"));
    assert_eq!(a.modality, "DX");
}

#[test]
fn divergent_restamp_learns_the_merged_truth() {
    let m = build_manifest(&attrs());
    let mut a = attrs();
    a.patient_id = Some("AUTOPAT_3820bdba2d61".to_string());
    a.patient_name = Some("Anonymous^3820bd".to_string());
    a.modality = "PR".to_string();
    restamp_from_manifest(&mut a, &m);
    assert_eq!(a.patient_id.as_deref(), Some("CLIN-DEMO-001"));
    assert_eq!(a.patient_name.as_deref(), Some("Clinfer^Transport^Demo"));
    assert_eq!(a.modality, "DX");
}
