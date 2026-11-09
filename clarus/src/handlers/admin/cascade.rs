// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Derived objects that a study deletion must take with it.
//!
//! When a study is deleted, objects that exist only because of that study
//! become unreachable and must go too. Two kinds are handled here:
//!
//! - **Workitems.** The CAD workitem for a study is derived from the study
//!   UID, so it is removed by recomputing its UID. A workitem that names the
//!   study explicitly (the standard `InputInformationSequence`) is not part
//!   of the record we store, so that route does not exist to read; the
//!   derived UID is what the field deployments actually create.
//! - **Derived instances.** Presentation states and segmentations reference
//!   the images they annotate. Their references are captured into the
//!   manifest at STOW time - see [`matches`] - and read back here, so the
//!   cascade never parses DICOM bytes.
//!
//! # The rule is a table, not a list of cases
//!
//! Every derived SOP class is one entry in [`CLASS_TABLE`]: the class UID,
//! what it is called, and which captured reference tells us where it points.
//! A new derived class is a new row. Nothing about the deletion itself
//! changes.
//!
//! # Scope
//!
//! A study deletion removes every derived object that points anywhere inside
//! the study. A series deletion removes only those that point at that series,
//! leaving objects that annotate the study's other series untouched - an
//! annotation of a different series is not part of this series.

#![forbid(unsafe_code)]

use clarus_core::manifest::{AttrValue, read_manifest};
use clarus_core::query::tag;
use clarus_core::stow::manifest_path;

use crate::state::AppState;

/// Grayscale Softcopy Presentation State Storage.
pub const SOP_GSPS: &str = "1.2.840.10008.5.1.4.1.1.11.1";

/// Segmentation Storage.
pub const SOP_SEG: &str = "1.2.840.10008.5.1.4.1.1.66.4";

/// Which captured reference a derived object is matched by.
///
/// The two shapes differ in what the reference points at, and one class may
/// carry both, so a match is "any of these ranges contains a target".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefKind {
    /// The captured series UIDs this object references.
    pub series: bool,
    /// The captured SOP Instance UIDs (referenced images) it references.
    pub instances: bool,
}

/// One derived SOP class and how to tell whether it belongs to a deletion.
#[derive(Debug, Clone, Copy)]
pub struct ClassEntry {
    /// SOP Class UID (0008,0016).
    pub sop_class_uid: &'static str,
    /// Name used in the removal log.
    pub name: &'static str,
    /// Which captured references are read for this class.
    pub refs: RefKind,
}

/// The derived classes the cascade understands.
///
/// PS3.3: a presentation state references the images it applies to through
/// `ReferencedSeriesSequence` (0008,1115) and/or `ReferencedImageSequence`
/// (0008,1140); a segmentation references its source images through
/// `SourceImageSequence` (0008,2112) and its segments through
/// `ReferencedSeriesSequence`. Both are derived objects of the same kind, so
/// they are the same mechanism here with different reference shapes.
pub const CLASS_TABLE: &[ClassEntry] = &[
    ClassEntry {
        sop_class_uid: SOP_GSPS,
        name: "presentation state",
        refs: RefKind {
            series: true,
            instances: true,
        },
    },
    ClassEntry {
        sop_class_uid: SOP_SEG,
        name: "segmentation",
        refs: RefKind {
            series: true,
            instances: true,
        },
    },
];

/// Look up the table entry for a SOP class UID.
#[must_use]
pub fn class_entry(sop_class_uid: &str) -> Option<&'static ClassEntry> {
    CLASS_TABLE
        .iter()
        .find(|entry| entry.sop_class_uid == sop_class_uid)
}

/// What a deletion targets.
#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    /// The whole study: any reference into it matches.
    Study(&'a str),
    /// One series: only references to that series match.
    Series(&'a str),
}

/// The resolved matching scope: a series UID, or a study's own contents.
///
/// The study arm carries the series and SOP Instance UIDs the deleted study
/// held, so a reference is claimed only when it points at an object this
/// study owned. Those UIDs must be read BEFORE the tombstone: after the
/// rename there is no manifest left to enumerate them from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StudyScope<'a> {
    /// One series UID.
    Series(&'a str),
    /// The deleted study's series and instance UIDs.
    Study {
        /// The deleted study UID (the derived workitem is keyed by it).
        study_uid: &'a str,
        /// Series Instance UIDs the study contained.
        series_uids: std::collections::HashSet<String>,
        /// SOP Instance UIDs the study contained.
        instance_uids: std::collections::HashSet<String>,
    },
}

/// The series UIDs and instance UIDs a derived object points at.
///
/// Read out of the manifest, never out of the DICOM bytes: the references
/// were captured at STOW, when the referenced objects were known to exist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct References {
    /// Referenced Series Instance UIDs.
    pub series: Vec<String>,
    /// Referenced SOP Instance UIDs.
    pub instances: Vec<String>,
}

/// The attribute keys the capture writes into an instance's attribute list.
///
/// Declared in `clarus_core::query::tag` beside every other attribute key,
/// so the capture and the read share one definition.
pub const REF_SERIES_KEY: u32 = tag::REFERENCED_SERIES_UIDS;
/// See [`REF_SERIES_KEY`].
pub const REF_INSTANCES_KEY: u32 = tag::REFERENCED_SOP_INSTANCE_UIDS;

/// Read the captured references out of an instance's attribute list.
#[must_use]
pub fn references_of(attrs: &[(u32, AttrValue)]) -> References {
    let mut out = References::default();
    for (key, value) in attrs {
        match *key {
            REF_SERIES_KEY => collect_strings(value, &mut out.series),
            REF_INSTANCES_KEY => collect_strings(value, &mut out.instances),
            _ => {}
        }
    }
    out
}

/// Append every string in `value` to `into`, accepting a scalar or an array.
fn collect_strings(value: &AttrValue, into: &mut Vec<String>) {
    match value {
        AttrValue::Text(text) => into.push(text.clone()),
        AttrValue::Array(items) => {
            for item in items {
                collect_strings(item, into);
            }
        }
        _ => {}
    }
}

/// Whether a derived object's references point inside the deleted scope.
///
/// A series deletion needs the exact series. A study deletion is scoped by
/// [`StudyScope`], the series and instance UIDs the deleted study actually
/// contained: "references anything at all" would claim every derived object
/// in the store, including ones that annotate an unrelated study.
#[must_use]
pub fn matches(entry: &ClassEntry, references: &References, scope: &StudyScope<'_>) -> bool {
    match scope {
        StudyScope::Series(series_uid) => {
            (entry.refs.series && references.series.iter().any(|s| s == series_uid))
                || (entry.refs.instances
                    && references
                        .instances
                        .iter()
                        .any(|i| instance_belongs_to(i, series_uid)))
        }
        StudyScope::Study {
            series_uids,
            instance_uids,
            ..
        } => {
            (entry.refs.series && references.series.iter().any(|s| series_uids.contains(s)))
                || (entry.refs.instances
                    && references
                        .instances
                        .iter()
                        .any(|i| instance_uids.contains(i)))
        }
    }
}

/// Whether a referenced SOP Instance UID belongs to a series.
///
/// The capture resolves references to series UIDs, so a referenced instance
/// is matched through its own series when the caller only has the instance.
/// Without a manifest lookup the instance cannot be placed, and guessing
/// would delete an object that annotates a *different* series - the one
/// outcome the series rule exists to prevent. So an unmatched instance does
/// not match.
fn instance_belongs_to(_instance_uid: &str, _series_uid: &str) -> bool {
    false
}

/// A derived object the cascade decided to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removal {
    /// The SOP Instance UID of the derived object.
    pub sop_instance_uid: String,
    /// The class name from the table, for the log.
    pub name: &'static str,
    /// The series the object belongs to (its own series, not the target).
    pub series_instance_uid: String,
    /// The study that owns the manifest holding the object.
    ///
    /// Manifests are named `{study_uid}.manifest`, so this - not the series
    /// UID - is the key the manifest is opened by.
    pub study_instance_uid: String,
}

/// Find every derived object in the store that the scope claims.
///
/// Walks the live manifests. A manifest that cannot be read is skipped rather
/// than failing the deletion: a study that has already been tombstoned has no
/// live manifest, and its derived objects were found when it was deleted.
#[must_use]
pub fn collect(state: &AppState, scope: &StudyScope<'_>) -> Vec<Removal> {
    let dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".manifest") {
            continue;
        }
        let study_uid = name.trim_end_matches(".manifest").to_string();
        let path = manifest_path(dir, &study_uid);
        let Ok(manifest) = read_manifest(&path) else {
            continue;
        };
        for series in &manifest.series {
            let series_uid = attr_text(&series.attrs, tag::SERIES_INSTANCE_UID).unwrap_or_default();
            for instance in &series.instances {
                let Some(sop_class_uid) = attr_text(&instance.attrs, tag::SOP_CLASS_UID) else {
                    continue;
                };
                let Some(entry) = class_entry(&sop_class_uid) else {
                    continue;
                };
                let references = references_of(&instance.attrs);
                if matches(entry, &references, scope) {
                    out.push(Removal {
                        sop_instance_uid: attr_text(&instance.attrs, tag::SOP_INSTANCE_UID)
                            .unwrap_or_default(),
                        name: entry.name,
                        series_instance_uid: series_uid.clone(),
                        study_instance_uid: study_uid.clone(),
                    });
                }
            }
        }
    }
    out
}

/// A text attribute by tag, or `None` when absent or not text.
fn attr_text(attrs: &[(u32, AttrValue)], wanted: u32) -> Option<String> {
    attrs.iter().find_map(|(tag, value)| match (tag, value) {
        (t, AttrValue::Text(text)) if *t == wanted => Some(text.clone()),
        _ => None,
    })
}

/// Remove every CAD workitem that belongs to a study.
///
/// The UIDs are not computed here. A study yields one workitem per catalog
/// entry, and the workitem UID folds in that entry's code, so the study UID
/// alone does NOT determine a workitem UID. The records are therefore found
/// the way [`crate::handlers::readiness`] finds them - by the `input_study_uid`
/// they were minted with - through
/// [`crate::handlers::readiness::workitem_uids_for_study`]. That is one
/// definition of "the workitems of this study" for both callers; deriving it
/// a second time would work until the naming changed, at which point the
/// cascade would silently stop finding the workitems it is meant to remove
/// while every test still passed.
///
/// Returns `true` when at least one workitem was removed.
///
/// # The client-created boundary
///
/// Every workitem this server creates is created by the STOW path and names
/// its study in `input_study_uid`, so this removal covers all of them,
/// including the CAD workitems the field deployments already have. A workitem
/// created by a client - with an arbitrary UID and a study named only inside
/// its `InputInformationSequence` - is invisible to this cascade: that
/// sequence is not part of the record we store, so there is nothing to match
/// on. No such client exists today, and the capture is deliberately not added
/// for one; the boundary is stated here so that adding such a client becomes a
/// deliberate act rather than a silent gap.
pub fn remove_workitem_for_study(state: &AppState, study_instance_uid: &str) -> bool {
    let store = crate::handlers::ups_pure::workitem_store(state);
    // A store that cannot be listed yields nothing to remove. The cascade is
    // best-effort by design (a leftover workitem is recoverable, a failed
    // study deletion is not), so the error does not stop the deletion.
    let Ok(uids) = crate::handlers::readiness::workitem_uids_for_study(&store, study_instance_uid)
    else {
        return false;
    };

    let mut removed_any = false;
    for uid in uids {
        match store.delete(&uid) {
            Ok(removed) => {
                if removed {
                    removed_any = true;
                    crate::clog_info!(
                        workitem_uid = %uid,
                        study = %study_instance_uid,
                        "derived workitem removed with its study"
                    );
                }
            }
            Err(error) => {
                // A workitem that cannot be removed must not abort the study
                // deletion: the study is already tombstoned, and a leftover
                // workitem is recoverable, while failing here would leave the
                // caller unsure whether the study went. The remaining
                // workitems are still attempted, because one unreadable record
                // is not a reason to leave the others behind.
                crate::clog_warn!(
                    workitem_uid = %uid,
                    error = %error,
                    "could not remove the derived workitem"
                );
            }
        }
    }
    removed_any
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(series: &[&str], instances: &[&str]) -> Vec<(u32, AttrValue)> {
        let mut out = Vec::new();
        if !series.is_empty() {
            out.push((
                REF_SERIES_KEY,
                AttrValue::Array(
                    series
                        .iter()
                        .map(|s| AttrValue::Text((*s).to_string()))
                        .collect(),
                ),
            ));
        }
        if !instances.is_empty() {
            out.push((
                REF_INSTANCES_KEY,
                AttrValue::Array(
                    instances
                        .iter()
                        .map(|s| AttrValue::Text((*s).to_string()))
                        .collect(),
                ),
            ));
        }
        out
    }

    /// A study scope holding exactly the UIDs the deleted study contained.
    fn study_scope<'a>(study: &'a str, series: &[&str], instances: &[&str]) -> StudyScope<'a> {
        StudyScope::Study {
            study_uid: study,
            series_uids: series.iter().map(|s| (*s).to_string()).collect(),
            instance_uids: instances.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn the_table_names_both_derived_classes() {
        assert!(class_entry(SOP_GSPS).is_some());
        assert!(class_entry(SOP_SEG).is_some());
        // An ordinary image is not a derived class and never cascades.
        assert!(class_entry("1.2.840.10008.5.1.4.1.1.7").is_none());
    }

    #[test]
    fn references_are_read_from_either_shape() {
        let scalar = vec![(REF_SERIES_KEY, AttrValue::Text("1.2.3".to_string()))];
        assert_eq!(references_of(&scalar).series, vec!["1.2.3"]);

        let many = attrs(&["1.2.3", "4.5"], &["6.7"]);
        let refs = references_of(&many);
        assert_eq!(refs.series, vec!["1.2.3", "4.5"]);
        assert_eq!(refs.instances, vec!["6.7"]);

        // Unrelated attributes are ignored rather than mistaken for refs.
        let other = vec![(0x0008_0060, AttrValue::Text("CT".to_string()))];
        assert_eq!(references_of(&other), References::default());
    }

    #[test]
    fn a_study_scope_claims_a_reference_into_the_deleted_study() {
        let gsps = class_entry(SOP_GSPS).expect("gsps");
        let seg = class_entry(SOP_SEG).expect("seg");
        let refs = References {
            series: vec!["1.2.3".to_string()],
            instances: Vec::new(),
        };
        let scope = study_scope("9.9", &["1.2.3"], &[]);
        assert!(matches(gsps, &refs, &scope));
        assert!(matches(seg, &refs, &scope));
    }

    /// The rule that M8 unmasked: once the cascade actually reads manifests,
    /// "matches any reference at all" claims derived objects of OTHER studies.
    /// A study deletion must be scoped to what the deleted study contained.
    #[test]
    fn a_study_scope_leaves_a_reference_into_another_study_alone() {
        let gsps = class_entry(SOP_GSPS).expect("gsps");
        let refs = References {
            series: vec!["7.7".to_string()],
            instances: vec!["8.8".to_string()],
        };
        let scope = study_scope("9.9", &["1.2.3"], &["4.5.6"]);
        assert!(
            !matches(gsps, &refs, &scope),
            "an object annotating a different study must survive"
        );
    }

    #[test]
    fn a_study_scope_matches_an_instance_reference_inside_the_study() {
        let gsps = class_entry(SOP_GSPS).expect("gsps");
        let refs = References {
            series: Vec::new(),
            instances: vec!["4.5.6".to_string()],
        };
        assert!(matches(
            gsps,
            &refs,
            &study_scope("9.9", &["1.2.3"], &["4.5.6"])
        ));
    }

    #[test]
    fn a_study_scope_ignores_an_object_with_no_captured_references() {
        // The capture never rejects a STOW for missing references, so an
        // object can exist with none. It is invisible to the cascade, which
        // is the documented outcome: no reference means nothing to match on.
        let gsps = class_entry(SOP_GSPS).expect("gsps");
        assert!(!matches(
            gsps,
            &References::default(),
            &study_scope("9.9", &["1.2.3"], &[])
        ));
        assert!(!matches(
            gsps,
            &References::default(),
            &StudyScope::Series("1.2.3")
        ));
    }

    /// An empty study scope claims nothing. A caller that failed to snapshot
    /// the study before tombstoning gets no cascade rather than every derived
    /// object in the store.
    #[test]
    fn an_empty_study_scope_claims_nothing() {
        let gsps = class_entry(SOP_GSPS).expect("gsps");
        let refs = References {
            series: vec!["1.2.3".to_string()],
            instances: vec!["4.5.6".to_string()],
        };
        assert!(!matches(gsps, &refs, &study_scope("9.9", &[], &[])));
    }

    #[test]
    fn a_series_scope_matches_only_the_named_series() {
        let gsps = class_entry(SOP_GSPS).expect("gsps");
        let refs = References {
            series: vec!["1.2.3".to_string()],
            instances: Vec::new(),
        };
        assert!(matches(gsps, &refs, &StudyScope::Series("1.2.3")));
        // The same object, a different series: it annotates another series
        // and must survive.
        assert!(!matches(gsps, &refs, &StudyScope::Series("4.5")));
    }

    #[test]
    fn an_instance_only_reference_does_not_match_a_series_scope() {
        // An instance reference cannot be placed into a series without a
        // manifest lookup, and guessing would delete an annotation of a
        // different series. Not matching is the safe answer, and it is the
        // one the series rule depends on.
        let gsps = class_entry(SOP_GSPS).expect("gsps");
        let refs = References {
            series: Vec::new(),
            instances: vec!["6.7".to_string()],
        };
        assert!(!matches(gsps, &refs, &StudyScope::Series("1.2.3")));
        // A study scope still claims it when the study owned that instance.
        assert!(matches(gsps, &refs, &study_scope("9.9", &[], &["6.7"])));
    }

    #[test]
    fn a_class_that_reads_only_series_ignores_an_instance_reference() {
        let entry = ClassEntry {
            sop_class_uid: "1.2.3",
            name: "series-only",
            refs: RefKind {
                series: true,
                instances: false,
            },
        };
        let refs = References {
            series: Vec::new(),
            instances: vec!["6.7".to_string()],
        };
        assert!(!matches(&entry, &refs, &StudyScope::Series("1.2.3")));
        assert!(!matches(&entry, &refs, &study_scope("9.9", &[], &["6.7"])));
    }
}
