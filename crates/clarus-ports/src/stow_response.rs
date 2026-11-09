// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! STOW response data types for the three-level Bridge-Server Protocol.
//!
//! These types are shared between the server (response builder) and the
//! DbActor (InsertResult). They live in clarus-ports so the adapters crate
//! can reference InsertResult without a circular dependency.

// -- Instance Outcome ----------------------------------------------------

/// Outcome of a single DICOM instance processed in a STOW request.
#[derive(Debug, Clone)]
pub enum InstanceOutcome {
    /// Instance stored successfully (newly inserted into DB).
    Stored {
        sop_class_uid: String,
        sop_instance_uid: String,
        study_instance_uid: String,
        series_instance_uid: String,
        /// PatientID of the patient this instance belongs to.
        patient_id: String,
    },
    /// Idempotent retry - same SOP, same Patient, same Study.
    /// No data was written; the existing instance is referenced.
    Idempotent {
        sop_class_uid: String,
        sop_instance_uid: String,
        study_instance_uid: String,
        series_instance_uid: String,
        /// PatientID of the existing patient.
        patient_id: String,
    },
    /// Update-in-place (2808): same SOP, same Study, same PatientID but the
    /// incoming bytes differ from the stored blob. The manifest re-points to
    /// the new canonical blob; `changed_attrs` counts the (0400,0561)
    /// OriginalAttributesSequence items embedded (0 = pixel-only
    /// replacement), `bytes_replaced` is always true today (kept separate
    /// from `changed_attrs` for the U-event schema).
    Updated {
        sop_class_uid: String,
        sop_instance_uid: String,
        study_instance_uid: String,
        series_instance_uid: String,
        /// PatientID of the patient this instance belongs to.
        patient_id: String,
        /// Count of (0400,0561) items embedded in the newly stored blob.
        changed_attrs: usize,
        /// True when the stored bytes were replaced.
        bytes_replaced: bool,
    },
    /// SOPInstanceUID conflict - SOP exists under a DIFFERENT Patient/Study.
    /// Instance was NOT stored. Per DICOM PS3.18 Section 10.5.2.2.2.
    Conflict {
        sop_class_uid: String,
        sop_instance_uid: String,
        /// PatientID of the existing instance.
        existing_patient_id: String,
        /// PatientID that the new instance claims.
        conflicting_patient_id: String,
    },
    /// Processing failure - parse error, I/O error, DB error, etc.
    Failed {
        sop_class_uid: Option<String>,
        sop_instance_uid: Option<String>,
        reason: String,
    },
}

impl InstanceOutcome {
    /// Returns the SOP Class UID if known.
    pub fn sop_class_uid(&self) -> Option<&str> {
        match self {
            InstanceOutcome::Stored { sop_class_uid, .. }
            | InstanceOutcome::Idempotent { sop_class_uid, .. }
            | InstanceOutcome::Updated { sop_class_uid, .. }
            | InstanceOutcome::Conflict { sop_class_uid, .. } => Some(sop_class_uid),
            InstanceOutcome::Failed { sop_class_uid, .. } => sop_class_uid.as_deref(),
        }
    }

    /// Returns the SOP Instance UID if known.
    pub fn sop_instance_uid(&self) -> Option<&str> {
        match self {
            InstanceOutcome::Stored {
                sop_instance_uid, ..
            }
            | InstanceOutcome::Idempotent {
                sop_instance_uid, ..
            }
            | InstanceOutcome::Updated {
                sop_instance_uid, ..
            }
            | InstanceOutcome::Conflict {
                sop_instance_uid, ..
            } => Some(sop_instance_uid),
            InstanceOutcome::Failed {
                sop_instance_uid, ..
            } => sop_instance_uid.as_deref(),
        }
    }

    /// Returns `true` if the instance was accepted (stored, updated, or
    /// idempotent).
    pub fn is_accepted(&self) -> bool {
        matches!(
            self,
            InstanceOutcome::Stored { .. }
                | InstanceOutcome::Updated { .. }
                | InstanceOutcome::Idempotent { .. }
        )
    }

    /// Returns `true` if this is a SOP conflict.
    pub fn is_conflict(&self) -> bool {
        matches!(self, InstanceOutcome::Conflict { .. })
    }
}

// -- DbActor Insert Result -----------------------------------------------

/// Result of inserting a single DICOM instance into the database via DbActor.
///
/// This is the return type of `insert_instance_hierarchy()` in the adapters
/// layer, passed through `process_insert_hierarchy()` in DbActor.
#[derive(Debug, Clone)]
pub enum InsertResult {
    /// Instance was newly inserted into the database.
    Inserted {
        instance_id: i64,
        sop_class_uid: String,
        sop_instance_uid: String,
        study_instance_uid: String,
        series_instance_uid: String,
        /// PatientID of the patient this instance belongs to.
        patient_id: String,
    },
    /// Idempotent retry - same SOP, same Patient, same Study.
    /// No data was written; the existing instance is referenced.
    Idempotent {
        sop_class_uid: String,
        sop_instance_uid: String,
        study_instance_uid: String,
        series_instance_uid: String,
        /// PatientID of the existing patient.
        patient_id: String,
    },
    /// Update-in-place (2808): same SOP, same Study, same PatientID, but the
    /// incoming bytes differ from the stored blob (the stored blob was not
    /// lost to a read failure). `old_attrs` is the `(group, element,
    /// old_value)` set the (0400,0561) CORRECT items are built from - the
    /// four StudyManifestAttrs tags only; empty when the manifest was
    /// unreadable. `old_blake3`/`old_tier` identify the replaced blob so the
    /// erase worker can reclaim it once the study is erased (orphan
    /// sidecar).
    Updated {
        sop_class_uid: String,
        sop_instance_uid: String,
        study_instance_uid: String,
        series_instance_uid: String,
        /// PatientID of the existing patient.
        patient_id: String,
        /// (group, element, old_value) for (0400,0561) CORRECT items.
        old_attrs: Vec<(u16, u16, String)>,
        /// CAS address of the replaced (old) blob.
        old_blake3: [u8; 32],
        /// Storage tier of the replaced (old) blob.
        old_tier: u8,
    },
    /// SOPInstanceUID conflict - SOP exists under a DIFFERENT Patient/Study.
    /// Instance was NOT stored.
    Conflict {
        sop_class_uid: String,
        sop_instance_uid: String,
        /// PatientID of the existing instance in the database.
        existing_patient_id: String,
        /// PatientID that the new instance claimed.
        conflicting_patient_id: String,
    },
}

// -- Final Response Types ------------------------------------------------

// -- Final Response Types ------------------------------------------------

/// Server-side statistics included in the `clarus` JSON block.
#[derive(Debug, Clone)]
pub struct ClarusServerStats {
    pub queue_depth: u32,
    pub queue_capacity: u32,
}

/// STOW statistics included in the `clarus` JSON block.
#[derive(Debug, Clone)]
pub struct ClarusStowStats {
    pub received: u32,
    pub stored: u32,
    pub conflicts: u32,
    pub conflict_sops: Vec<String>,
}

/// The full `clarus` extension block in the STOW response JSON.
#[derive(Debug, Clone)]
pub struct ClarusBlock {
    pub stow: ClarusStowStats,
    pub server: ClarusServerStats,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instance_outcome_is_accepted() {
        let stored = InstanceOutcome::Stored {
            sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".into(),
            sop_instance_uid: "1.2.840.1".into(),
            study_instance_uid: "1.2.840.2".into(),
            series_instance_uid: "1.2.840.3".into(),
            patient_id: "PAT_001".into(),
        };
        assert!(stored.is_accepted());
        assert!(!stored.is_conflict());

        let idem = InstanceOutcome::Idempotent {
            sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".into(),
            sop_instance_uid: "1.2.840.1".into(),
            study_instance_uid: "1.2.840.2".into(),
            series_instance_uid: "1.2.840.3".into(),
            patient_id: "PAT_001".into(),
        };
        assert!(idem.is_accepted());
        assert!(!idem.is_conflict());

        let upd = InstanceOutcome::Updated {
            sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".into(),
            sop_instance_uid: "1.2.840.1".into(),
            study_instance_uid: "1.2.840.2".into(),
            series_instance_uid: "1.2.840.3".into(),
            patient_id: "PAT_001".into(),
            changed_attrs: 1,
            bytes_replaced: true,
        };
        assert!(upd.is_accepted());
        assert!(!upd.is_conflict());

        let conflict = InstanceOutcome::Conflict {
            sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".into(),
            sop_instance_uid: "1.2.840.1".into(),
            existing_patient_id: "PAT_X".into(),
            conflicting_patient_id: "PAT_Y".into(),
        };
        assert!(!conflict.is_accepted());
        assert!(conflict.is_conflict());
    }
}
