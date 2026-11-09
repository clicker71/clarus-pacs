// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! STOW response builder - pure DICOMweb PS3.18 STOW-RS response.
//!
//! Clarus extensions (X-Clarus-* headers, "clarus" JSON block) removed
//! Standard HTTP only.

#![forbid(unsafe_code)]

use clarus_ports::stow_response::InstanceOutcome;

use crate::handlers::DICOM_JSON_CONTENT_TYPE;
use crate::handlers::json_common::push_json_str;

// -- StowAccum ------------------------------------------------------------

/// Accumulator for per-instance outcomes during multipart STOW processing.
///
/// Built incrementally as each DICOM part is processed. No post-processing
/// pass needed - the final response is built directly from this accumulator.
#[derive(Debug, Clone, Default)]
pub struct StowAccum {
    /// Total number of application/dicom parts received in the multipart.
    pub received: u32,
    /// Number of instances accepted (stored + idempotent).
    pub accepted: u32,
    /// Number of instances that were exact duplicates (idempotent re-STOW).
    /// Both fresh stores and exact-duplicate re-STOWs
    /// are full success -> response 200 (no 201/204 distinction anymore).
    pub idempotent: u32,
    /// Number of instances replaced in place (2808 update-in-place).
    pub updated: u32,
    /// Number of ACCEPTED instances whose stored content differs from what the
    /// client sent: a coerced empty Type 2 field (Warn Reason B000) or an
    /// update that rewrote an existing SOP.
    ///
    /// PS3.18 10.5.2 maps "stored with warnings" to `202 Accepted`, and the
    /// reason is carried per instance by (0008,1196). `accepted` counts these
    /// the same as a clean store, so a client reading only the status code had
    /// no way to learn that its PatientID was generated for it or that its
    /// instance replaced an existing one - the two facts it most needs to act
    /// on. Kept separate from `updated` because a coerced fresh store is also
    /// a warning.
    pub warned: u32,
    /// Number of SOPInstanceUID conflicts detected.
    pub conflicts: u32,
    /// SOP Instance UIDs that conflicted.
    pub conflict_sops: Vec<String>,
    /// Number of instances that failed processing.
    pub failed: u32,
    /// ReferencedSOPSequence JSON entries (native bytes) for accepted
    /// instances.
    pub referenced_sops: Vec<String>,
    /// FailedSOPSequence JSON entries (native bytes) for failed/conflict
    /// instances.
    pub failed_sops: Vec<String>,
    /// StudyInstanceUID of the first accepted SOP (for logging).
    /// One patient = one StudyUID, so this identifies the study being uploaded.
    pub study_uid: Option<String>,
    /// PatientID of the first accepted SOP (for logging).
    /// Identifies the patient being uploaded.
    pub patient_id: Option<String>,
}

impl StowAccum {
    /// Record an instance outcome along with its DICOM JSON entries
    /// (pre-rendered native bytes).
    pub fn record(
        &mut self,
        outcome: &InstanceOutcome,
        referenced_sop: Option<String>,
        failed_sop: Option<String>,
    ) {
        self.received = self.received.saturating_add(1);
        match outcome {
            InstanceOutcome::Stored {
                study_instance_uid,
                patient_id,
                ..
            } => {
                self.accepted = self.accepted.saturating_add(1);
                // Capture StudyUID / PatientID from the first accepted SOP.
                if self.study_uid.is_none() {
                    self.study_uid = Some(study_instance_uid.clone());
                }
                if self.patient_id.is_none() {
                    self.patient_id = Some(patient_id.clone());
                }
                if let Some(entry) = referenced_sop {
                    self.referenced_sops.push(entry);
                }
            }
            InstanceOutcome::Idempotent {
                study_instance_uid,
                patient_id,
                ..
            } => {
                self.accepted = self.accepted.saturating_add(1);
                self.idempotent = self.idempotent.saturating_add(1);
                // Observability: count exact duplicates (STOW-statuses 16.08).
                crate::metrics::record_stow_duplicate();
                // Capture StudyUID / PatientID from the first accepted SOP.
                if self.study_uid.is_none() {
                    self.study_uid = Some(study_instance_uid.clone());
                }
                if self.patient_id.is_none() {
                    self.patient_id = Some(patient_id.clone());
                }
                if let Some(entry) = referenced_sop {
                    self.referenced_sops.push(entry);
                }
            }
            InstanceOutcome::Updated {
                study_instance_uid,
                patient_id,
                ..
            } => {
                self.accepted = self.accepted.saturating_add(1);
                self.updated = self.updated.saturating_add(1);
                // An in-place replacement is accepted with a warning: the
                // client's instance is not what the archive now holds.
                self.warned = self.warned.saturating_add(1);
                // Capture StudyUID / PatientID from the first accepted SOP.
                if self.study_uid.is_none() {
                    self.study_uid = Some(study_instance_uid.clone());
                }
                if self.patient_id.is_none() {
                    self.patient_id = Some(patient_id.clone());
                }
                if let Some(entry) = referenced_sop {
                    self.referenced_sops.push(entry);
                }
            }
            InstanceOutcome::Conflict {
                sop_instance_uid, ..
            } => {
                self.conflicts = self.conflicts.saturating_add(1);
                self.conflict_sops.push(sop_instance_uid.clone());
                if let Some(entry) = failed_sop {
                    self.failed_sops.push(entry);
                }
            }
            InstanceOutcome::Failed { .. } => {
                self.failed = self.failed.saturating_add(1);
                if let Some(entry) = failed_sop {
                    self.failed_sops.push(entry);
                }
            }
        }
    }

    /// Returns true if any instance was accepted.
    pub fn has_any_accepted(&self) -> bool {
        self.accepted > 0
    }

    /// Returns true if any conflict or failure occurred.
    pub fn has_any_rejected(&self) -> bool {
        self.conflicts > 0 || self.failed > 0
    }
}

// -- Pure response builder -----------------------------------------------

/// Build the pure parts of a STOW-RS response from accumulated outcomes.
///
/// Returns `(status, content_type, body_bytes, warning)` where `warning` is
/// `Some((header_name, header_value))` - the RFC 7234 `Warning` header - when
/// the response status is 202 and there is something to warn about.
pub fn build_stow_response(
    accum: &StowAccum,
) -> (u16, &'static str, Vec<u8>, Option<(String, String)>) {
    let status = determine_status(accum);
    let body_bytes = build_body(accum);

    let warning = if status == 202 {
        let mut parts: Vec<String> = Vec::new();
        if accum.conflicts > 0 {
            parts.push(format!("{} conflict(s)", accum.conflicts));
        }
        if accum.failed > 0 {
            parts.push(format!("{} failure(s)", accum.failed));
        }
        // A warning-only 202 (every instance accepted) still says WHICH
        // warning, or the status code alone would be unexplained.
        if parts.is_empty() && accum.warned > 0 {
            parts.push(format!("{} instance(s) stored with warnings", accum.warned));
        }
        if !parts.is_empty() {
            // RFC 7234: warn-text MUST be a quoted-string.
            let warning_msg = format!("299 localhost \"{}\"", parts.join(", "));
            Some(("Warning".to_string(), warning_msg))
        } else {
            None
        }
    } else {
        None
    };

    (status, DICOM_JSON_CONTENT_TYPE, body_bytes, warning)
}

/// Determine the appropriate HTTP status code (200/202/409/400).
///
/// Full STOW success is ALWAYS `200 OK` - both fresh stores and
/// exact-duplicate (idempotent) re-STOWs. `201 Created` is no longer used
/// (PS3.18 STOW-RS has no per-request resource to "create", and the body
/// carries the per-instance ReferencedSOPSequence). `202` is the partial-
/// success path (some accepted + some rejected) AND the "stored with warnings"
/// answer (PS3.18 10.5.2): every instance needs no retry, but at least one was
/// coerced or replaced, so the client is told the archive does not hold exactly
/// what it sent. `409` a pure SOP conflict, `400` all-failed / empty.
fn determine_status(accum: &StowAccum) -> u16 {
    let has_accepted = accum.accepted > 0;
    let has_rejected = accum.conflicts > 0 || accum.failed > 0;

    match (has_accepted, has_rejected) {
        (true, true) => 202,
        // Warn Reason B000 / update-in-place: warn even when nothing failed.
        (true, false) if accum.warned > 0 => 202,
        (true, false) => 200,
        (false, true) => {
            if accum.conflicts > 0 && accum.failed == 0 {
                409
            } else {
                400
            }
        }
        (false, false) => 400,
    }
}

/// Build the JSON response body (DICOM `ReferencedSOPSequence`/`FailedSOPSequence`).
/// Assemble the STOW-RS body natively (byte-exact with the legacy serde:
/// failed/conflict block 00081198 before referenced 00081199, each a SQ with
/// the pre-rendered entry bytes).
fn build_body(accum: &StowAccum) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(b'{');
    let mut wrote = false;
    if !accum.failed_sops.is_empty() {
        // { "00081198": { "vr": "SQ", "Value": [ entries ] } }
        body.extend_from_slice(b"\"00081198\":{\"Value\":[");
        for (i, e) in accum.failed_sops.iter().enumerate() {
            if i > 0 {
                body.push(b',');
            }
            body.extend_from_slice(e.as_bytes());
        }
        body.extend_from_slice(b"],\"vr\":\"SQ\"}");
        wrote = true;
    }
    if !accum.referenced_sops.is_empty() {
        if wrote {
            body.push(b',');
        }
        body.extend_from_slice(b"\"00081199\":{\"Value\":[");
        for (i, e) in accum.referenced_sops.iter().enumerate() {
            if i > 0 {
                body.push(b',');
            }
            body.extend_from_slice(e.as_bytes());
        }
        body.extend_from_slice(b"],\"vr\":\"SQ\"}");
    }
    body.push(b'}');
    body
}

// -- Helpers --------------------------------------------------------------

/// Build a ReferencedSOPSequence JSON entry for an accepted instance.
///
/// When `coerced` is true (FR-001 generated an empty Type 2 field, e.g. a
/// `AUTOPAT_` / `Anonymous^` PatientID/PatientName), the entry carries the
/// DICOM Warn Reason `(0008,1196)` = `45056` (B000, "Coercion of Data
/// Elements"), PS3.18 Table I.2-1 / Table 10.5.3-1. `(0400,0561)` is
/// deliberately NOT emitted (KISS; the standard only says it "may be
/// included").
pub fn build_referenced_sop_entry(
    sop_class_uid: &str,
    sop_instance_uid: &str,
    study_instance_uid: &str,
    series_instance_uid: &str,
    coerced: bool,
) -> String {
    // Byte-exact with the legacy serde render: BTreeMap tag sort, tag object
    // Value-before-vr; 00081196 (coerce warn reason) last when present.
    let mut out = Vec::new();
    out.extend_from_slice(b"{\"00081150\":{\"Value\":[");
    push_json_str(&mut out, sop_class_uid);
    out.extend_from_slice(b"],\"vr\":\"UI\"},\"00081155\":{\"Value\":[");
    push_json_str(&mut out, sop_instance_uid);
    out.extend_from_slice(b"],\"vr\":\"UI\"},\"00081190\":{\"Value\":[\"/dicomweb/studies/");
    // The UIDs are embedded raw inside the single URI string (ASCII, no
    // escaping needed - serde_json emits them verbatim).
    out.extend_from_slice(study_instance_uid.as_bytes());
    out.extend_from_slice(b"/series/");
    out.extend_from_slice(series_instance_uid.as_bytes());
    out.extend_from_slice(b"/instances/");
    out.extend_from_slice(sop_instance_uid.as_bytes());
    out.extend_from_slice(b"\"],\"vr\":\"UR\"}");
    if coerced {
        out.extend_from_slice(b",\"00081196\":{\"Value\":[45056],\"vr\":\"US\"}");
    }
    out.push(b'}');
    // UIDs/URLs here are ASCII; from_utf8 is safe.
    String::from_utf8(out).expect("referenced_sop_entry is valid UTF-8")
}

/// Build a FailedSOPSequence JSON entry for a SOP conflict.
/// Per DICOM PS3.18 Section 10.5.2.2.2 - includes Failure Reason (0008,1197) = 272.
///
/// The comment names WHAT conflicted, never WHOSE it was. A SOP Instance UID
/// reuse means the stored object belongs to a different patient, so quoting
/// the existing Patient ID in the response hands the sender another
/// patient's identifier - PHI disclosed to whoever can post a colliding UID.
/// The value stays in the server log for the operator, where it is already
/// masked; the wire carries only the fact of the conflict and the UIDs the
/// sender itself supplied.
pub fn build_conflict_failed_sop_entry(sop_class_uid: &str, sop_instance_uid: &str) -> String {
    // Byte-exact with the legacy serde render: BTreeMap tag sort puts 00000902
    // first, then 00081150/00081155/00081197; tag object Value-before-vr.
    let mut out = Vec::new();
    out.extend_from_slice(b"{\"00000902\":{\"Value\":[");
    push_json_str(
        &mut out,
        "SOP Instance already exists with different Patient/Study metadata",
    );
    out.extend_from_slice(b"],\"vr\":\"LO\"},\"00081150\":{\"Value\":[");
    push_json_str(&mut out, sop_class_uid);
    out.extend_from_slice(b"],\"vr\":\"UI\"},\"00081155\":{\"Value\":[");
    push_json_str(&mut out, sop_instance_uid);
    out.extend_from_slice(b"],\"vr\":\"UI\"},\"00081197\":{\"Value\":[272],\"vr\":\"US\"}}");
    String::from_utf8(out).expect("conflict_failed_sop_entry is valid UTF-8")
}

/// Build a FailedSOPSequence JSON entry for a processing failure.
pub fn build_failed_sop_entry(
    sop_class_uid: Option<&str>,
    sop_instance_uid: Option<&str>,
    reason: &str,
) -> String {
    // Byte-exact with the legacy serde render: BTreeMap tag sort puts 00000902
    // first, then 00081150/00081155 (when present) and 00081197; Value first.
    let mut out = Vec::new();
    out.extend_from_slice(b"{\"00000902\":{\"Value\":[");
    push_json_str(&mut out, reason);
    out.extend_from_slice(b"],\"vr\":\"LO\"}");
    if let (Some(sc), Some(si)) = (sop_class_uid, sop_instance_uid) {
        out.extend_from_slice(b",\"00081150\":{\"Value\":[");
        push_json_str(&mut out, sc);
        out.extend_from_slice(b"],\"vr\":\"UI\"},\"00081155\":{\"Value\":[");
        push_json_str(&mut out, si);
        out.extend_from_slice(b"],\"vr\":\"UI\"}");
    }
    out.extend_from_slice(b",\"00081197\":{\"Value\":[272],\"vr\":\"US\"}}");
    String::from_utf8(out).expect("failed_sop_entry is valid UTF-8")
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::json_common::{JsonVal, parse_json};

    const SC: &str = "1.2.840.10008.5.1.4.1.1.2";
    const SI: &str = "1.2.3.4.5.6.7.1";
    const STU: &str = "1.2.3.4.study";
    const SER: &str = "1.2.3.4.series";

    /// The native STOW entry/body writers are byte-exact with the legacy
    /// serde render (captured 19.08) for all entry forms.
    #[test]
    fn stow_entries_json_matches_legacy_render() {
        assert_eq!(
            build_referenced_sop_entry(SC, SI, STU, SER, false),
            "{\"00081150\":{\"Value\":[\"1.2.840.10008.5.1.4.1.1.2\"],\"vr\":\"UI\"},\"00081155\":{\"Value\":[\"1.2.3.4.5.6.7.1\"],\"vr\":\"UI\"},\"00081190\":{\"Value\":[\"/dicomweb/studies/1.2.3.4.study/series/1.2.3.4.series/instances/1.2.3.4.5.6.7.1\"],\"vr\":\"UR\"}}"
        );
        assert_eq!(
            build_referenced_sop_entry(SC, SI, STU, SER, true),
            "{\"00081150\":{\"Value\":[\"1.2.840.10008.5.1.4.1.1.2\"],\"vr\":\"UI\"},\"00081155\":{\"Value\":[\"1.2.3.4.5.6.7.1\"],\"vr\":\"UI\"},\"00081190\":{\"Value\":[\"/dicomweb/studies/1.2.3.4.study/series/1.2.3.4.series/instances/1.2.3.4.5.6.7.1\"],\"vr\":\"UR\"},\"00081196\":{\"Value\":[45056],\"vr\":\"US\"}}"
        );
        assert_eq!(
            build_conflict_failed_sop_entry(SC, SI),
            "{\"00000902\":{\"Value\":[\"SOP Instance already exists with different Patient/Study metadata\"],\"vr\":\"LO\"},\"00081150\":{\"Value\":[\"1.2.840.10008.5.1.4.1.1.2\"],\"vr\":\"UI\"},\"00081155\":{\"Value\":[\"1.2.3.4.5.6.7.1\"],\"vr\":\"UI\"},\"00081197\":{\"Value\":[272],\"vr\":\"US\"}}"
        );
        assert_eq!(
            build_failed_sop_entry(Some(SC), Some(SI), "boom"),
            "{\"00000902\":{\"Value\":[\"boom\"],\"vr\":\"LO\"},\"00081150\":{\"Value\":[\"1.2.840.10008.5.1.4.1.1.2\"],\"vr\":\"UI\"},\"00081155\":{\"Value\":[\"1.2.3.4.5.6.7.1\"],\"vr\":\"UI\"},\"00081197\":{\"Value\":[272],\"vr\":\"US\"}}"
        );
        assert_eq!(
            build_failed_sop_entry(None, None, "boom"),
            "{\"00000902\":{\"Value\":[\"boom\"],\"vr\":\"LO\"},\"00081197\":{\"Value\":[272],\"vr\":\"US\"}}"
        );

        // Assembled body: failed (00081198) before referenced (00081199).
        let mut acc = StowAccum::default();
        acc.referenced_sops
            .push(build_referenced_sop_entry(SC, SI, STU, SER, false));
        acc.failed_sops
            .push(build_failed_sop_entry(None, None, "boom"));
        assert_eq!(
            String::from_utf8(build_body(&acc)).unwrap(),
            "{\"00081198\":{\"Value\":[{\"00000902\":{\"Value\":[\"boom\"],\"vr\":\"LO\"},\"00081197\":{\"Value\":[272],\"vr\":\"US\"}}],\"vr\":\"SQ\"},\"00081199\":{\"Value\":[{\"00081150\":{\"Value\":[\"1.2.840.10008.5.1.4.1.1.2\"],\"vr\":\"UI\"},\"00081155\":{\"Value\":[\"1.2.3.4.5.6.7.1\"],\"vr\":\"UI\"},\"00081190\":{\"Value\":[\"/dicomweb/studies/1.2.3.4.study/series/1.2.3.4.series/instances/1.2.3.4.5.6.7.1\"],\"vr\":\"UR\"}}],\"vr\":\"SQ\"}}"
        );
    }

    /// A coerced instance (FR-001 generated an empty Type 2 field) must carry
    /// Warn Reason (0008,1196) = 45056 (B000, Coercion of Data Elements).
    #[test]
    fn referenced_sop_entry_coerced_has_b000() {
        let bytes = build_referenced_sop_entry(SC, SI, STU, SER, true);
        let entry = parse_json(bytes.as_bytes()).expect("json");
        let v = entry.obj_get("00081196").unwrap();
        assert_eq!(v.obj_get("vr").and_then(|x| x.as_str()), Some("US"));
        assert_eq!(
            v.obj_get("Value"),
            Some(&JsonVal::Arr(vec![JsonVal::U64(45056)]))
        );
        // Core fields are still present.
        assert_eq!(
            entry.obj_get("00081150").and_then(|x| x.obj_get("Value")),
            Some(&JsonVal::Arr(vec![JsonVal::Str(SC.to_string())]))
        );
        assert_eq!(
            entry.obj_get("00081155").and_then(|x| x.obj_get("Value")),
            Some(&JsonVal::Arr(vec![JsonVal::Str(SI.to_string())]))
        );
    }

    /// A non-coerced instance must NOT carry (0008,1196).
    #[test]
    fn referenced_sop_entry_not_coerced_no_b000() {
        let bytes = build_referenced_sop_entry(SC, SI, STU, SER, false);
        let entry = parse_json(bytes.as_bytes()).expect("json");
        assert!(
            entry.obj_get("00081196").is_none(),
            "no B000 for clean instance"
        );
        assert!(entry.obj_get("00081150").is_some());
        assert!(entry.obj_get("00081155").is_some());
        assert!(entry.obj_get("00081190").is_some());
    }

    /// Full success (fresh AND exact-duplicate) is unified to 200; only the
    /// partial/mixed and rejection paths keep their distinct codes.
    #[test]
    fn determine_status_unifies_full_success_to_200() {
        // Fresh store: accepted == received, no idempotent, no rejections -> 200.
        let fresh = StowAccum {
            received: 2,
            accepted: 2,
            idempotent: 0,
            ..StowAccum::default()
        };
        assert_eq!(determine_status(&fresh), 200);

        // Exact duplicate of every sent instance -> 200.
        let dup = StowAccum {
            received: 2,
            accepted: 2,
            idempotent: 2,
            ..StowAccum::default()
        };
        assert_eq!(determine_status(&dup), 200);

        // Partial success (some accepted, some failed) -> 202.
        let partial = StowAccum {
            received: 2,
            accepted: 1,
            idempotent: 0,
            failed: 1,
            ..StowAccum::default()
        };
        assert_eq!(determine_status(&partial), 202);

        // Stored with WARNINGS: every instance accepted, but one was coerced
        // (Warn Reason B000) or replaced in place. PS3.18 10.5.2 answers 202,
        // and a client that reads only the status code must see it.
        let warned = StowAccum {
            received: 2,
            accepted: 2,
            idempotent: 0,
            warned: 2,
            ..StowAccum::default()
        };
        assert_eq!(
            determine_status(&warned),
            202,
            "a coerced/updated instance is stored WITH WARNINGS"
        );
        // ... and the Warning header explains an otherwise bare 202.
        let (status, _, _, warning) = build_stow_response(&warned);
        assert_eq!(status, 202);
        let (name, value) = warning.expect("a warning-only 202 still explains itself");
        assert_eq!(name, "Warning");
        assert!(
            value.contains("stored with warnings") && value.ends_with('"'),
            "RFC 7234 requires a quoted-string: {value}"
        );

        // Pure conflict (rejected, no failures) -> 409.
        let conflict = StowAccum {
            received: 1,
            accepted: 0,
            conflicts: 1,
            conflict_sops: vec!["1.2.3".into()],
            ..StowAccum::default()
        };
        assert_eq!(determine_status(&conflict), 409);

        // All failed -> 400.
        let all_failed = StowAccum {
            received: 1,
            accepted: 0,
            failed: 1,
            ..StowAccum::default()
        };
        assert_eq!(determine_status(&all_failed), 400);

        // Empty -> 400.
        assert_eq!(determine_status(&StowAccum::default()), 400);
    }
}
