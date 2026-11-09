// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! STOW-side helpers shared by the server's store pipeline.
//!
//! Currently holds the deterministic generation of empty DICOM Type 2 fields
//! (B-007). The algorithms are StudyInstanceUID-anchored so every instance of
//! the same study derives identical synthetic values - required for QIDO
//! round-trips to agree between the SQLite and B-tree tiers.

pub mod coordinator;
pub mod manifest;
pub mod validation;

pub use coordinator::{StudyLease, acquire};
pub use manifest::{
    InstanceModel, SeriesModel, StudyManifestAttrs, build_manifest_full, hidden_reason_of,
    index_study_names, manifest_path, mark_rejected, mark_restored, orphan_sidecar_path,
    rebuild_series_attrs, tombstone_path, write_study_manifest, write_study_manifest_full,
    write_study_manifest_rebuild, write_study_manifest_update,
};
pub use validation::{
    GenerationError, fallback_study_uid_key, generate_patient_id, generate_patient_name,
    generate_study_instance_uid,
};
