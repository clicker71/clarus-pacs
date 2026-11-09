// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Rejection-note reason table and per-reason sequestering scope.
//!
//! A rejection note (IOCM / RAD-66) is an ordinary Key Object Selection
//! document (SOP Class `1.2.840.10008.5.1.4.1.1.88.59`) whose Document Title
//! names the reason the referenced instances are being rejected. Clarus
//! INTERPRETS such a note at STOW time and hides the instances it names.
//!
//! The mechanism is generic IOCM, not one profile: [`REASONS`] is a table and
//! the detector matches against it. Adding a reason means adding a row plus
//! its [`Scope`] - never touching detection or the marking loop.
//!
//! Facts: `standard/ps3.3/key-object-selection-a35.4.json` (documentTitle,
//! airaRejectionNoteCodes, scopeGuard) and
//! `standard/ps3.6/sop-class-uids.json`. All codes and UIDs below are quoted
//! from those files, not from memory.

/// Whether a reason's note is recognised, and how far its authority reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Hide only the referenced instances whose SOP Class is an AI Result
    /// class ([`AI_RESULT_SOP_CLASSES`]).
    ///
    /// A key-object note is stored through the ordinary STOW path, which is
    /// not admin-gated. Without this bound, a crafted or malformed note could
    /// hide diagnostic CT/MR images from the archive - a data-hiding denial of
    /// service against patient care. The bound is the reason a Quality note
    /// is safe to honour automatically.
    AiResultClassesOnly,
    /// Hide every referenced instance regardless of class.
    ///
    /// Correct for reasons that legitimately name diagnostic images (for
    /// example a wrong-patient rejection naming the original CT). No row uses
    /// it yet; it exists so adding such a reason is a table entry, not a code
    /// change.
    AnyClass,
}

/// One coded Document Title that turns a KOS into a rejection note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reason {
    /// Short name used in logs and tests.
    pub name: &'static str,
    /// Code Value (0008,0100) of the Document Title.
    pub code_value: &'static str,
    /// Coding Scheme Designator (0008,0102) of the Document Title.
    pub scheme: &'static str,
    /// How far this reason's note may hide instances.
    pub scope: Scope,
}

/// The reasons a rejection note may state.
///
/// Four standard rows, all quoted from
/// `standard/ps3.3/rejection-note-reasons.json` (verified against PS3.16
/// CID 7010 and IHE RAD TF-2 4.66.4). Quality is the AIRA reason and stays
/// class-bounded; the other three are IOCM reasons whose whole point is to
/// hide diagnostic images, so they are [`Scope::AnyClass`] and carry a much
/// heavier authority - see [`NEVER_EXPOSE_REASONS`].
///
/// Revoke is deliberately ABSENT: dcm4chee's restore reason is a PRIVATE
/// `99DCM4CHEE` code, and there is no standard DICOM un-reject. Adopting a
/// private code to undo a standard one would make the archive's visible state
/// depend on which vendor's tooling was last pointed at it.
pub const REASONS: &[Reason] = &[
    Reason {
        name: "quality",
        // PS3.3 C.17.2 Document Title; AIRA 57.1.1.1 uses "Rejected for
        // Quality Reasons" for AI result assessment.
        code_value: "113001",
        scheme: "DCM",
        scope: Scope::AiResultClassesOnly,
    },
    // IOCM 4.66.4.2: wrong patient / patient-safety rejections name the
    // original diagnostic images, so the class bound cannot apply.
    Reason {
        name: "patient-safety",
        code_value: "113037",
        scheme: "DCM",
        scope: Scope::AnyClass,
    },
    // IOCM 4.66.4.3: acquired against the wrong Modality Worklist entry.
    Reason {
        name: "incorrect-mwl",
        code_value: "113038",
        scheme: "DCM",
        scope: Scope::AnyClass,
    },
    // IOCM 4.66.4.4: retention period has lapsed. A lifecycle marker: the
    // instances are past their policy window, which is orthogonal to whether
    // the pixels were acquired correctly.
    Reason {
        name: "retention-expired",
        code_value: "113039",
        scheme: "DCM",
        scope: Scope::AnyClass,
    },
];

/// Reasons whose rejection can NOT be lifted by any retrieval surface.
///
/// IOCM 4.66.4.2.3 / 4.66.4.3.3 define no expose path for patient-safety and
/// incorrect-MWL rejections, and the receiver "shall not accept subsequent
/// occurrence of instances that have been hidden" under them. Only Quality
/// (113001) has a review surface (AIRA 57.2.1: a webservice endpoint
/// specifically configured to provide access to hidden instances).
///
/// Keeping this as a predicate over the reason CODE - rather than a flag on
/// [`Reason`] - is deliberate: the code is what gets persisted in the
/// manifest, so the expose decision is made from durable data and cannot
/// drift from what was actually recorded at STOW time.
pub fn never_expose(reason_code: u32) -> bool {
    reason_code != QUALITY_REASON_CODE
}

/// The Quality reason code as an integer, for the persisted reason field.
///
/// A rejection note states its reason as a DICOM code string; the manifest
/// stores it as a number so the expose decision is one comparison on a
/// decoded record, not a string parse on every retrieve.
pub const QUALITY_REASON_CODE: u32 = 113_001;

/// Reason code stored for an instance that is not rejected.
pub const NOT_REJECTED: u32 = 0;

/// Parse a Document Title code value into the persisted reason code.
///
/// Returns `None` for a code that is not four ASCII digits: the persisted
/// field is a number, and a malformed code must not silently become 0 (which
/// would read as "not rejected" and quietly expose a hidden instance).
pub fn reason_code_to_u32(code_value: &str) -> Option<u32> {
    if code_value.len() != 6 || !code_value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    code_value.parse().ok()
}

/// Whether a note whose reason resolved to `reason_code` may hide `sop_class`.
///
/// The single entry point the STOW marking loop uses: it resolves the code to
/// a row, then applies that row's [`Scope`]. Both halves come from the table,
/// so adding a reason never touches the loop.
pub fn may_hide_code(reason_code: u32, sop_class_uid: &str) -> bool {
    REASONS
        .iter()
        .find(|r| reason_code_to_u32(r.code_value) == Some(reason_code))
        .is_some_and(|r| may_hide(r, sop_class_uid))
}

/// SOP Class UIDs a Quality rejection may hide (AIRA Tables 6.8.2-1/6.8.2-2).
///
/// UIDs are quoted from `standard/ps3.6/sop-class-uids.json`. A referenced
/// instance of any OTHER class (a CT or MR diagnostic image, an ultrasound
/// series) is never hidden, whatever the note claims.
pub const AI_RESULT_SOP_CLASSES: &[&str] = &[
    // Enhanced SR Storage
    "1.2.840.10008.5.1.4.1.1.88.22",
    // Comprehensive SR Storage
    "1.2.840.10008.5.1.4.1.1.88.33",
    // Comprehensive 3D SR Storage
    "1.2.840.10008.5.1.4.1.1.88.34",
    // Secondary Capture Image Storage
    "1.2.840.10008.5.1.4.1.1.7",
    // RT Structure Set Storage
    "1.2.840.10008.5.1.4.1.1.481.3",
    // Grayscale Softcopy Presentation State Storage
    "1.2.840.10008.5.1.4.1.1.11.1",
    // Encapsulated PDF Storage
    "1.2.840.10008.5.1.4.1.1.104.1",
    // Segmentation Storage
    "1.2.840.10008.5.1.4.1.1.66.4",
    // Parametric Map Storage
    "1.2.840.10008.5.1.4.1.1.30",
];

/// Key Object Selection Document Storage - the only SOP class that can be a
/// rejection note.
pub const KEY_OBJECT_SELECTION_SOP_CLASS: &str = "1.2.840.10008.5.1.4.1.1.88.59";

/// Look a reason up by its Document Title code.
///
/// Matching is on the code VALUE and its scheme together: the same number
/// under a different scheme is a different concept, and treating it as the
/// same would let a private code hide images.
pub fn reason_for(code_value: &str, scheme: &str) -> Option<&'static Reason> {
    REASONS
        .iter()
        .find(|r| r.code_value == code_value && r.scheme == scheme)
}

/// Whether `sop_class_uid` may be hidden under `reason`'s scope.
pub fn may_hide(reason: &Reason, sop_class_uid: &str) -> bool {
    match reason.scope {
        Scope::AnyClass => true,
        Scope::AiResultClassesOnly => AI_RESULT_SOP_CLASSES.contains(&sop_class_uid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_reason_is_recognised_by_code_and_scheme() {
        let r = reason_for("113001", "DCM").expect("quality must be in the table");
        assert_eq!(r.name, "quality");
        assert_eq!(r.scope, Scope::AiResultClassesOnly);
    }

    #[test]
    fn same_number_under_other_scheme_is_not_quality() {
        assert!(reason_for("113001", "99IHE").is_none());
        assert!(reason_for("113001", "SCT").is_none());
    }

    #[test]
    fn the_three_any_class_reasons_are_recognised() {
        for (code, name) in [
            ("113037", "patient-safety"),
            ("113038", "incorrect-mwl"),
            ("113039", "retention-expired"),
        ] {
            let r = reason_for(code, "DCM").unwrap_or_else(|| panic!("{code} must be known"));
            assert_eq!(r.name, name);
            assert_eq!(r.scope, Scope::AnyClass, "{code} names diagnostic images");
        }
    }

    #[test]
    fn the_incorrect_mwl_reason_is_113038_not_110514() {
        // 110514 is a PROCEDURE DISCONTINUATION reason (PS3.16 family
        // 110500-110514), a different context group. Accepting it as a
        // rejection-note title would honour a note that means something else.
        assert!(reason_for("110514", "DCM").is_none());
        assert!(reason_for("113038", "DCM").is_some());
    }

    #[test]
    fn revoke_is_not_a_recognised_reason() {
        // Private 99DCM4CHEE code: there is no standard DICOM un-reject.
        assert!(reason_for("REVOKE_REJECTION", "99DCM4CHEE").is_none());
    }

    #[test]
    fn only_quality_may_ever_be_exposed() {
        assert!(!never_expose(QUALITY_REASON_CODE));
        for code in [113_037u32, 113_038, 113_039] {
            assert!(never_expose(code), "{code} has no expose path in IOCM");
        }
    }

    #[test]
    fn a_malformed_code_does_not_persist_as_not_rejected() {
        // Silently collapsing to 0 would read back as "visible".
        assert_eq!(reason_code_to_u32("1130O1"), None);
        assert_eq!(reason_code_to_u32("11300"), None);
        assert_eq!(reason_code_to_u32(""), None);
        assert_eq!(reason_code_to_u32("113001"), Some(113_001));
    }

    #[test]
    fn the_class_bound_follows_the_resolved_row() {
        // AnyClass reasons hide a CT; the Quality reason does not.
        assert!(may_hide_code(113_037, "1.2.840.10008.5.1.4.1.1.2"));
        assert!(may_hide_code(113_038, "1.2.840.10008.5.1.4.1.1.2"));
        assert!(!may_hide_code(
            QUALITY_REASON_CODE,
            "1.2.840.10008.5.1.4.1.1.2"
        ));
        // An unknown code hides nothing.
        assert!(!may_hide_code(123_456, "1.2.840.10008.5.1.4.1.1.2"));
    }

    #[test]
    fn quality_scope_hides_ai_results_only() {
        let q = reason_for("113001", "DCM").expect("quality");
        // An AI result the note names: hidden.
        assert!(may_hide(q, "1.2.840.10008.5.1.4.1.1.88.33"));
        assert!(may_hide(q, "1.2.840.10008.5.1.4.1.1.66.4"));
        // A diagnostic image the note names: NOT hidden.
        assert!(!may_hide(q, "1.2.840.10008.5.1.4.1.1.2")); // CT
        assert!(!may_hide(q, "1.2.840.10008.5.1.4.1.1.4")); // MR
        // A private/unknown class: NOT hidden.
        assert!(!may_hide(q, "1.2.3.4.5"));
    }

    #[test]
    fn any_class_scope_hides_diagnostic_images() {
        // No shipped row uses AnyClass, but the policy must be exercisable so
        // adding such a reason cannot silently inherit the Quality bound.
        let wide = Reason {
            name: "test",
            code_value: "0",
            scheme: "X",
            scope: Scope::AnyClass,
        };
        assert!(may_hide(&wide, "1.2.840.10008.5.1.4.1.1.2"));
    }
}
