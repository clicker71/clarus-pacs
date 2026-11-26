// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! STOW response builder - pure DICOMweb PS3.18 STOW-RS response.
//!
//! Clarus extensions (X-Clarus-* headers, "clarus" JSON block) removed
//! Standard HTTP only.

#![forbid(unsafe_code)]

use clarus_ports::stow_response::{FailureClass, InstanceOutcome};

use crate::handlers::DICOM_JSON_CONTENT_TYPE;
use crate::handlers::DICOM_XML_CONTENT_TYPE;
use crate::handlers::json_common::push_json_str;

/// (0000,0902) Error Comment text for a study/patient conflict.
///
/// One constant for both serialisations: the JSON entry and the XML writer
/// each used to build their own string, and only one of them said anything.
const STUDY_PATIENT_CONFLICT_COMMENT: &str =
    "Study Instance UID already stored under a different patient";

// -- StowAccum ------------------------------------------------------------

/// One accepted instance, as the response must describe it.
///
/// Carries the fields, not the rendered text: the JSON and XML serialisers each
/// render their own format from this, so an Accept-driven format switch cannot
/// change what the answer says.
#[derive(Debug, Clone, Default)]
pub struct ReferencedItem {
    pub sop_class_uid: String,
    pub sop_instance_uid: String,
    pub study_instance_uid: String,
    pub series_instance_uid: String,
    /// (0008,1196) Warn Reason is present when the stored instance differs
    /// from what the client sent (a coerced Type 2 field, or an in-place
    /// update).
    pub coerced: bool,
    /// Which accepted outcome produced this item, so a demotion can decrement
    /// the matching counter instead of only `accepted`.
    pub kind: ReferencedKind,
    /// The pre-rendered DICOM JSON entry for this instance, when the caller
    /// supplied one.
    ///
    /// Held HERE rather than in a parallel `Vec` on the accumulator: the entry
    /// is optional per item, so a positional index into a separate list goes
    /// stale as soon as one item is recorded without an entry. Demoting that
    /// item used to remove the NEXT item's entry, leaving an XML/<JSON>
    /// disagreement (XML said stored, JSON had lost the entry).
    pub json_entry: Option<String>,
}

/// The accepted outcome kind behind a [`ReferencedItem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReferencedKind {
    /// A fresh store ([`InstanceOutcome::Stored`]).
    #[default]
    Stored,
    /// An exact duplicate ([`InstanceOutcome::Idempotent`]).
    Idempotent,
    /// An in-place replacement ([`InstanceOutcome::Updated`]).
    Updated,
}

/// One rejected instance, as the response must describe it.
#[derive(Debug, Clone, Default)]
pub struct FailedItem {
    /// (0000,0902) Error Comment. Absent for a pure SOP conflict, which
    /// carries only the failure reason.
    ///
    /// PS3.18 I.2.1: the per-instance response module carries Error Comment
    /// alongside Failure Reason. Both serialisers read this ONE field, so the
    /// two bodies cannot describe a different answer - emitting the local-use
    /// (0009,0902) from the XML writer meant a client looking the standard tag
    /// up found nothing.
    pub error_comment: Option<String>,
    pub sop_class_uid: Option<String>,
    pub sop_instance_uid: Option<String>,
    /// (0008,1197) Failure Reason. `272` = Processing Failure.
    pub failure_reason: u16,
}

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
    /// How many of `failed` were the server's own fault (an unreadable
    /// manifest, a failed persistence step) rather than the payload's.
    ///
    /// The distinction only matters when NOTHING was accepted: a request whose
    /// every refusal is a store failure is a 5xx, while one carrying a bad
    /// instance is a 400 (PS3.18 10.5.3.1). A mixed batch is 202 either way,
    /// which is why this is a counter and not a per-request flag.
    pub store_failures: u32,
    /// FailedSOPSequence JSON entries (native bytes) for failed/conflict
    /// instances.
    ///
    /// Still a parallel list because a [`FailedItem`] has no optional shape:
    /// every failure produces exactly one entry, appended in the same call
    /// that pushes its structured twin. A demotion appends to both, so the
    /// indices stay aligned for the failure sequences.
    pub failed_sops: Vec<String>,
    /// The SAME two sequences in their structured form.
    ///
    /// The JSON entries above are pre-rendered bytes, which cannot be
    /// re-serialised as XML. Rather than parse our own JSON back apart (and
    /// re-derive the URI, the warn reason and the failure reason from text),
    /// both serialisers read these fields: one source of truth, and the two
    /// bodies cannot describe different answers.
    pub referenced_items: Vec<ReferencedItem>,
    pub failed_items: Vec<FailedItem>,
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
        // The structured mirror is derived HERE from the outcome, not from the
        // caller's pre-rendered strings: the caller supplies JSON bytes only,
        // and a second argument would let the two representations disagree.
        // The entry lands ON the item it describes, so an item recorded
        // without one cannot shift the entry of the item after it.
        self.record_structured(outcome, referenced_sop);
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
            InstanceOutcome::StudyPatientConflict {
                sop_instance_uid, ..
            } => {
                // Counted as a conflict: the request was answered with an
                // existing resource (a study) in a state that prevents the
                // store. A batch whose only refusals are this one must answer
                // 409, not 400, and a mixed batch 202 - the same rule the
                // per-SOP conflict follows.
                self.conflicts = self.conflicts.saturating_add(1);
                self.conflict_sops.push(sop_instance_uid.clone());
                if let Some(entry) = failed_sop {
                    self.failed_sops.push(entry);
                }
            }
            InstanceOutcome::Failed { class, .. } => {
                self.failed = self.failed.saturating_add(1);
                if *class == FailureClass::StoreFailure {
                    self.store_failures = self.store_failures.saturating_add(1);
                }
                if let Some(entry) = failed_sop {
                    self.failed_sops.push(entry);
                }
            }
        }
    }

    /// Mirror one outcome into the structured sequences the XML writer reads.
    ///
    /// `coerced` is not on the outcome, so a coerced store is marked by the
    /// caller through [`Self::mark_last_referenced_coerced`]; everything else
    /// is read straight off the variant.
    fn record_structured(&mut self, outcome: &InstanceOutcome, json_entry: Option<String>) {
        match outcome {
            InstanceOutcome::Stored {
                sop_class_uid,
                sop_instance_uid,
                study_instance_uid,
                series_instance_uid,
                ..
            } => self.referenced_items.push(ReferencedItem {
                sop_class_uid: sop_class_uid.clone(),
                sop_instance_uid: sop_instance_uid.clone(),
                study_instance_uid: study_instance_uid.clone(),
                series_instance_uid: series_instance_uid.clone(),
                coerced: false,
                kind: ReferencedKind::Stored,
                json_entry,
            }),
            InstanceOutcome::Idempotent {
                sop_class_uid,
                sop_instance_uid,
                study_instance_uid,
                series_instance_uid,
                ..
            } => self.referenced_items.push(ReferencedItem {
                sop_class_uid: sop_class_uid.clone(),
                sop_instance_uid: sop_instance_uid.clone(),
                study_instance_uid: study_instance_uid.clone(),
                series_instance_uid: series_instance_uid.clone(),
                coerced: false,
                kind: ReferencedKind::Idempotent,
                json_entry,
            }),
            InstanceOutcome::Updated {
                sop_class_uid,
                sop_instance_uid,
                study_instance_uid,
                series_instance_uid,
                ..
            } => self.referenced_items.push(ReferencedItem {
                sop_class_uid: sop_class_uid.clone(),
                sop_instance_uid: sop_instance_uid.clone(),
                study_instance_uid: study_instance_uid.clone(),
                series_instance_uid: series_instance_uid.clone(),
                // An in-place update is always a warning: the archive no longer
                // holds exactly what the client sent.
                coerced: true,
                kind: ReferencedKind::Updated,
                json_entry,
            }),
            InstanceOutcome::Conflict {
                sop_class_uid,
                sop_instance_uid,
                ..
            } => self.failed_items.push(FailedItem {
                error_comment: None,
                sop_class_uid: Some(sop_class_uid.clone()),
                sop_instance_uid: Some(sop_instance_uid.clone()),
                failure_reason: 272,
            }),
            // A pure SOP conflict answers 409 and carries only the failure
            // reason; a study/patient conflict is the one conflict that has
            // something to say, so it carries the SAME comment in both
            // serialisations (the JSON entry below and the XML writer both
            // read this field).
            InstanceOutcome::StudyPatientConflict {
                sop_class_uid,
                sop_instance_uid,
                ..
            } => self.failed_items.push(FailedItem {
                error_comment: Some(STUDY_PATIENT_CONFLICT_COMMENT.to_string()),
                sop_class_uid: Some(sop_class_uid.clone()),
                sop_instance_uid: Some(sop_instance_uid.clone()),
                failure_reason: 272,
            }),
            InstanceOutcome::Failed {
                sop_class_uid,
                sop_instance_uid,
                reason,
                ..
            } => self.failed_items.push(FailedItem {
                error_comment: Some(reason.clone()),
                sop_class_uid: sop_class_uid.clone(),
                sop_instance_uid: sop_instance_uid.clone(),
                failure_reason: 272,
            }),
        }
    }

    /// Flag the most recent referenced item as carrying Warn Reason B000.
    ///
    /// A coerced store is `Stored` + a separate `coerced` flag on the ingest
    /// result, so the flag arrives after the outcome was recorded.
    pub fn mark_last_referenced_coerced(&mut self) {
        if let Some(last) = self.referenced_items.last_mut() {
            last.coerced = true;
        }
    }

    /// Move an accepted instance into FailedSOPSequence after its study
    /// manifest write failed (P1 group commit).
    ///
    /// Returns `true` when the instance was found among the accepted sequences
    /// and demoted (its ReferencedSOP entry removed, the accepted counter and
    /// the matching kind counter decremented). Returns `false` when it was
    /// already recorded as a failure. Either way a failed entry carrying BOTH
    /// UIDs is pushed, so the client can resend the specific instance
    /// (PS3.18 10.5.2) - the pre-fix shape carried neither UID and left the
    /// same SOP in both sequences.
    pub fn demote_to_failed(
        &mut self,
        sop_instance_uid: &str,
        sop_class_uid: &str,
        reason: &str,
    ) -> bool {
        let idx = self
            .referenced_items
            .iter()
            .position(|it| it.sop_instance_uid == sop_instance_uid);
        if let Some(idx) = idx {
            // Removing the ITEM removes its entry too, because the entry is a
            // field of that item. A positional index into a separate List would
            // remove the next item's entry as soon as any item lacked one.
            let item = self.referenced_items.remove(idx);
            self.accepted = self.accepted.saturating_sub(1);
            match item.kind {
                ReferencedKind::Stored => {
                    if item.coerced {
                        self.warned = self.warned.saturating_sub(1);
                    }
                }
                ReferencedKind::Idempotent => {
                    self.idempotent = self.idempotent.saturating_sub(1);
                }
                ReferencedKind::Updated => {
                    self.updated = self.updated.saturating_sub(1);
                    self.warned = self.warned.saturating_sub(1);
                }
            }
        }
        self.failed = self.failed.saturating_add(1);
        self.failed_items.push(FailedItem {
            error_comment: Some(reason.to_string()),
            sop_class_uid: Some(sop_class_uid.to_string()),
            sop_instance_uid: Some(sop_instance_uid.to_string()),
            failure_reason: 272,
        });
        self.failed_sops.push(build_failed_sop_entry(
            Some(sop_class_uid),
            Some(sop_instance_uid),
            reason,
        ));
        idx.is_some()
    }

    /// Returns true if any instance was accepted.
    pub fn has_any_accepted(&self) -> bool {
        self.accepted > 0
    }

    /// Whether the JSON ReferencedSOPSequence has anything to write.
    ///
    /// The entry is per item and optional, so "non-empty" is a question about
    /// the items, not about a counter: an accepted instance whose caller
    /// supplied no entry (XML-only bookkeeping) must not emit an empty `Value`.
    fn has_any_referenced_entry(&self) -> bool {
        self.referenced_items
            .iter()
            .any(|item| item.json_entry.is_some())
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
///
/// `wants_xml` selects the body SERIALISATION only; the status, the warning and
/// every field of the answer are identical either way.
///
/// The Accept header matters because a STOW client that gets a `202` or `409`
/// parses the body: dcm4che3-based senders (Weasis among them) call
/// `SAXReader.parse()` and declare `application/dicom+xml` in their Accept. A
/// JSON body answered to that request fails with `SAXParseException: Content is
/// not allowed in prolog` - the data is stored fine, but the client reports the
/// export as failed. Answering in the format the client asked for is what
/// PS3.18 8.4.1 requires of the origin server anyway.
pub fn build_stow_response(
    accum: &StowAccum,
    wants_xml: bool,
) -> (u16, &'static str, Vec<u8>, Option<(String, String)>) {
    let status = determine_status(accum);
    let (content_type, body_bytes) = if wants_xml {
        (DICOM_XML_CONTENT_TYPE, build_body_xml(accum))
    } else {
        (DICOM_JSON_CONTENT_TYPE, build_body(accum))
    };

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

    (status, content_type, body_bytes, warning)
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
            if accum.conflicts == 0 && accum.failed > 0 && accum.store_failures == accum.failed {
                // Every refusal was the server failing to persist a request it
                // understood. Retrying the SAME payload is the right client
                // action, so 400 (which says "your request is wrong") would
                // send it to fix bytes that are fine.
                500
            } else if accum.conflicts > 0 && accum.failed == 0 {
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
    if !accum.failed_items.is_empty() {
        // { "00081198": { "vr": "SQ", "Value": [ entries ] } }
        body.extend_from_slice(b"\"00081198\":{\"Value\":[");
        for (i, item) in accum.failed_items.iter().enumerate() {
            if i > 0 {
                body.push(b',');
            }
            body.extend_from_slice(&failed_item_json(item));
        }
        body.extend_from_slice(b"],\"vr\":\"SQ\"}");
        wrote = true;
    }
    if accum.has_any_referenced_entry() {
        if wrote {
            body.push(b',');
        }
        body.extend_from_slice(b"\"00081199\":{\"Value\":[");
        let mut first = true;
        for item in &accum.referenced_items {
            let Some(entry) = &item.json_entry else {
                continue;
            };
            if !first {
                body.push(b',');
            }
            first = false;
            body.extend_from_slice(entry.as_bytes());
        }
        body.extend_from_slice(b"],\"vr\":\"SQ\"}");
    }
    body.push(b'}');
    body
}

/// Build the DICOM XML response body - the same two sequences as
/// [`build_body`], in the NativeDicomModel envelope every other XML route uses.
///
/// The envelope and the `<?xml ...?>` prolog come from
/// `clarus_core::query::xml`, so a `SAXReader`-based client (dcm4che3, and so
/// Weasis) sees exactly the document shape it already parses from QIDO. The
/// keyword attribute is filled because PS3.19 A.1.1 makes it the human-readable
/// half of the pair; `tag` alone is enough for a parser but not for a human
/// reading a captured response.
///
/// Element order matches the JSON body: `FailedSOPSequence` (0008,1198) before
/// `ReferencedSOPSequence` (0008,1199), which is also ascending tag order.
fn build_body_xml(accum: &StowAccum) -> Vec<u8> {
    let mut body =
        Vec::with_capacity((accum.referenced_items.len() + accum.failed_items.len()) * 256 + 256);
    clarus_core::query::xml::write_xml_doc_start(&mut body);
    if !accum.failed_items.is_empty() {
        push_sq_start(&mut body, 0x0008_1198, "FailedSOPSequence");
        // PS3.19 A.1.1: `Item` is numbered one-based WITHIN each sequence, so
        // the counter restarts here and again for the referenced sequence.
        for (i, item) in accum.failed_items.iter().enumerate() {
            push_item_start(&mut body, i + 1);
            // (0000,0902) Error Comment, VR LO (PS3.18 I.2.1).
            if let Some(comment) = &item.error_comment {
                push_attr(&mut body, 0x0000_0902, "ErrorComment", "LO", comment);
            }
            if let (Some(sc), Some(si)) = (&item.sop_class_uid, &item.sop_instance_uid) {
                push_attr(&mut body, 0x0008_1150, "ReferencedSOPClassUID", "UI", sc);
                push_attr(&mut body, 0x0008_1155, "ReferencedSOPInstanceUID", "UI", si);
            }
            // (0008,1197) Failure Reason is US: a decimal value, not a code
            // string.
            push_attr_us(
                &mut body,
                0x0008_1197,
                "FailureReason",
                u64::from(item.failure_reason),
            );
            push_item_end(&mut body);
        }
        push_sq_end(&mut body);
    }
    if !accum.referenced_items.is_empty() {
        push_sq_start(&mut body, 0x0008_1199, "ReferencedSOPSequence");
        for (i, item) in accum.referenced_items.iter().enumerate() {
            push_item_start(&mut body, i + 1);
            push_attr(
                &mut body,
                0x0008_1150,
                "ReferencedSOPClassUID",
                "UI",
                &item.sop_class_uid,
            );
            push_attr(
                &mut body,
                0x0008_1155,
                "ReferencedSOPInstanceUID",
                "UI",
                &item.sop_instance_uid,
            );
            // (0008,1190) RetrieveURL - same path the JSON body builds, so a
            // client that follows it lands on the same instance either way.
            let url = format!(
                "/dicomweb/studies/{}/series/{}/instances/{}",
                item.study_instance_uid, item.series_instance_uid, item.sop_instance_uid
            );
            push_attr(&mut body, 0x0008_1190, "RetrieveURL", "UR", &url);
            if item.coerced {
                push_attr_us(&mut body, 0x0008_1196, "WarningReason", 45056);
            }
            push_item_end(&mut body);
        }
        push_sq_end(&mut body);
    }
    clarus_core::query::xml::write_xml_doc_end(&mut body);
    body
}

// -- Minimal NativeDicomModel writers -------------------------------------
//
// The QIDO writer builds these through a streaming `Sink` with a bounded
// scratch buffer because it walks whole manifests. A STOW answer carries one
// item per instance in the request, so the same shapes are rendered directly
// into the body instead - no scratch buffer, no overflow path to handle, and
// the output is byte-identical to what the streaming writer produces for the
// same element.

/// Open one `<Item number="N">`.
///
/// PS3.19 A.1.1: the numbering is one-based and restarts inside each
/// sequence, so the caller passes the position in ITS OWN loop. A fixed
/// `number="1"` made two stored instances collide on the same item number.
fn push_item_start(out: &mut Vec<u8>, number: usize) {
    out.extend_from_slice(b"<Item number=\"");
    out.extend_from_slice(number.to_string().as_bytes());
    out.extend_from_slice(b"\">\n");
}

fn push_item_end(out: &mut Vec<u8>) {
    out.extend_from_slice(b"</Item>\n");
}

fn push_sq_start(out: &mut Vec<u8>, tag: u32, keyword: &str) {
    out.extend_from_slice(b"<DicomAttribute tag=\"");
    push_tag_hex(out, tag);
    out.extend_from_slice(b"\" vr=\"SQ\" keyword=\"");
    out.extend_from_slice(keyword.as_bytes());
    out.extend_from_slice(b"\">\n");
}

fn push_sq_end(out: &mut Vec<u8>) {
    out.extend_from_slice(b"</DicomAttribute>\n");
}

/// Append one `<DicomAttribute>` with a single text `<Value number="1">`.
fn push_attr(out: &mut Vec<u8>, tag: u32, keyword: &str, vr: &str, value: &str) {
    out.extend_from_slice(b"<DicomAttribute tag=\"");
    push_tag_hex(out, tag);
    out.extend_from_slice(b"\" vr=\"");
    out.extend_from_slice(vr.as_bytes());
    out.extend_from_slice(b"\" keyword=\"");
    out.extend_from_slice(keyword.as_bytes());
    out.extend_from_slice(b"\"><Value number=\"1\">");
    push_xml_escaped(out, value);
    out.extend_from_slice(b"</Value></DicomAttribute>\n");
}

/// Append one `<DicomAttribute>` carrying a single `US` value.
///
/// A `US` value is a number in the XML encoding (PS3.19 A.1.1 / Table A.1-1:
/// numeric VRs write the decimal form, not a string), so it is written
/// unescaped - digits cannot contain markup.
fn push_attr_us(out: &mut Vec<u8>, tag: u32, keyword: &str, value: u64) {
    out.extend_from_slice(b"<DicomAttribute tag=\"");
    push_tag_hex(out, tag);
    out.extend_from_slice(b"\" vr=\"US\" keyword=\"");
    out.extend_from_slice(keyword.as_bytes());
    out.extend_from_slice(b"\"><Value number=\"1\">");
    out.extend_from_slice(value.to_string().as_bytes());
    out.extend_from_slice(b"</Value></DicomAttribute>\n");
}

/// 8 uppercase hex digits, the `tag` attribute spelling (PS3.19 A.1.1).
fn push_tag_hex(out: &mut Vec<u8>, tag: u32) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for shift in [28u32, 24, 20, 16, 12, 8, 4, 0] {
        out.push(HEX[((tag >> shift) & 0xF) as usize]);
    }
}

/// Escape the five XML metacharacters and drop what XML 1.0 cannot represent.
///
/// A failure reason carries the client's own text (a parse error message, an
/// SOP UID echoed back), so it is untrusted: emitting it raw lets a crafted
/// DICOM file close the element and inject markup into the response. `&` must
/// be replaced first or the other replacements would be re-escaped.
///
/// XML 1.0 section 2.2 defines Char as `#x9 | #xA | #xD | [#x20-#xD7FF] |
/// [#xE000-#xFFFD] | [#x10000-#x10FFFF]`. A NUL or a vertical tab is NOT a
/// Char, and a document holding one is not well-formed at all - a strict
/// parser rejects the WHOLE response, including the successful SOP references
/// it also carries. Such bytes are replaced with U+FFFD (the Unicode
/// replacement character) rather than deleted, so the text keeps its length
/// and a reader can see that something was dropped.
///
/// A `char` above U+FFFD is always legal, so only the low range needs testing;
/// the surrogate range cannot occur in a Rust `char`.
fn push_xml_escaped(out: &mut Vec<u8>, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.extend_from_slice(b"&amp;"),
            '<' => out.extend_from_slice(b"&lt;"),
            '>' => out.extend_from_slice(b"&gt;"),
            '"' => out.extend_from_slice(b"&quot;"),
            '\'' => out.extend_from_slice(b"&apos;"),
            '\t' | '\n' | '\u{20}'..='\u{D7FF}' | '\u{E000}'.. => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            // U+FFFD, encoded as EF BF BD: the replacement character says the
            // byte was dropped instead of silently shortening the text.
            _ => out.extend_from_slice(b"\xEF\xBF\xBD"),
        }
    }
}

/// Render one [`FailedItem`] as a DICOM JSON item.
///
/// Rendered from the SAME struct the XML writer reads, so the two bodies
/// cannot state different things. Tag order is ascending and `Value` precedes
/// `vr`, matching the byte shape this server has always emitted (and what
/// [`build_failed_sop_entry`] writes, whose output is pinned by a test).
fn failed_item_json(item: &FailedItem) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(b'{');
    let mut wrote = false;
    if let Some(comment) = &item.error_comment {
        out.extend_from_slice(b"\"00000902\":{\"Value\":[");
        push_json_str(&mut out, comment);
        out.extend_from_slice(b"],\"vr\":\"LO\"}");
        wrote = true;
    }
    if let (Some(sc), Some(si)) = (&item.sop_class_uid, &item.sop_instance_uid) {
        if wrote {
            out.push(b',');
        }
        out.extend_from_slice(b"\"00081150\":{\"Value\":[");
        push_json_str(&mut out, sc);
        out.extend_from_slice(b"],\"vr\":\"UI\"},\"00081155\":{\"Value\":[");
        push_json_str(&mut out, si);
        out.extend_from_slice(b"],\"vr\":\"UI\"}");
        wrote = true;
    }
    if wrote {
        out.push(b',');
    }
    out.extend_from_slice(b"\"00081197\":{\"Value\":[");
    out.extend_from_slice(item.failure_reason.to_string().as_bytes());
    out.extend_from_slice(b"],\"vr\":\"US\"}}");
    out
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

/// Build a FailedSOPSequence entry for a study-level patient conflict.
///
/// Same wire shape as [`build_conflict_failed_sop_entry`] - `(0008,1197)`
/// Failure Reason 0110H/272 (PS3.18 Table I.2-2 "Processing failure", the
/// general code, since Table I.2-2 defines no patient-mismatch code) plus an
/// Error Comment naming the reason - with a comment that describes the STUDY
/// collision rather than a SOP UID reuse. As with the SOP-level entry the
/// comment never names the stored patient.
pub fn build_study_conflict_failed_sop_entry(
    sop_class_uid: &str,
    sop_instance_uid: &str,
) -> String {
    let mut out = Vec::new();
    out.extend_from_slice(b"{\"00000902\":{\"Value\":[");
    push_json_str(&mut out, STUDY_PATIENT_CONFLICT_COMMENT);
    out.extend_from_slice(b"],\"vr\":\"LO\"},\"00081150\":{\"Value\":[");
    push_json_str(&mut out, sop_class_uid);
    out.extend_from_slice(b"],\"vr\":\"UI\"},\"00081155\":{\"Value\":[");
    push_json_str(&mut out, sop_instance_uid);
    out.extend_from_slice(b"],\"vr\":\"UI\"},\"00081197\":{\"Value\":[272],\"vr\":\"US\"}}");
    String::from_utf8(out).expect("study_conflict_failed_sop_entry is valid UTF-8")
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
        acc.referenced_items.push(ReferencedItem {
            sop_class_uid: SC.to_string(),
            sop_instance_uid: SI.to_string(),
            study_instance_uid: STU.to_string(),
            series_instance_uid: SER.to_string(),
            coerced: false,
            kind: ReferencedKind::Stored,
            json_entry: Some(build_referenced_sop_entry(SC, SI, STU, SER, false)),
        });
        acc.failed_items.push(FailedItem {
            error_comment: Some("boom".to_string()),
            sop_class_uid: None,
            sop_instance_uid: None,
            failure_reason: 272,
        });
        assert_eq!(
            String::from_utf8(build_body(&acc)).unwrap(),
            "{\"00081198\":{\"Value\":[{\"00000902\":{\"Value\":[\"boom\"],\"vr\":\"LO\"},\"00081197\":{\"Value\":[272],\"vr\":\"US\"}}],\"vr\":\"SQ\"},\"00081199\":{\"Value\":[{\"00081150\":{\"Value\":[\"1.2.840.10008.5.1.4.1.1.2\"],\"vr\":\"UI\"},\"00081155\":{\"Value\":[\"1.2.3.4.5.6.7.1\"],\"vr\":\"UI\"},\"00081190\":{\"Value\":[\"/dicomweb/studies/1.2.3.4.study/series/1.2.3.4.series/instances/1.2.3.4.5.6.7.1\"],\"vr\":\"UR\"}}],\"vr\":\"SQ\"}}"
        );
    }

    /// A study-level patient conflict is a Conflict for status purposes: a
    /// batch whose only refusals are these must answer 409 (PS3.18 Table
    /// 10.5.3-1 "unable to store ANY Instances due to a conflict"), and a
    /// mixed batch 202 - the same rule the per-SOP conflict follows.
    #[test]
    fn a_study_patient_conflict_is_a_conflict_for_status() {
        let mut accum = StowAccum::default();
        let outcome = InstanceOutcome::StudyPatientConflict {
            sop_class_uid: SC.to_string(),
            sop_instance_uid: SI.to_string(),
            study_instance_uid: STU.to_string(),
        };
        accum.record(
            &outcome,
            None,
            Some(build_study_conflict_failed_sop_entry(SC, SI)),
        );
        assert_eq!(accum.conflicts, 1, "counted as a conflict");
        assert_eq!(accum.failed, 0);
        assert_eq!(
            determine_status(&accum),
            409,
            "nothing stored and only study-patient conflicts -> 409"
        );

        // The structured mirror must carry the same refusal, or the XML body
        // would describe a different outcome than the JSON one.
        assert_eq!(accum.failed_items.len(), 1);
        assert_eq!(accum.failed_items[0].failure_reason, 272);
        assert_eq!(
            accum.failed_items[0].error_comment.as_deref(),
            Some("Study Instance UID already stored under a different patient")
        );
        assert!(
            !accum.failed_sops[0].contains("PAT-"),
            "the entry must not leak a stored patient identifier"
        );

        // One stored + one study-patient conflict -> 202.
        let mut mixed = StowAccum::default();
        mixed.record(
            &InstanceOutcome::Stored {
                sop_class_uid: SC.to_string(),
                sop_instance_uid: SI.to_string(),
                study_instance_uid: STU.to_string(),
                series_instance_uid: SER.to_string(),
                patient_id: "PAT-A".to_string(),
            },
            Some(build_referenced_sop_entry(SC, SI, STU, SER, false)),
            None,
        );
        mixed.record(
            &outcome,
            None,
            Some(build_study_conflict_failed_sop_entry(SC, SI)),
        );
        assert_eq!(
            determine_status(&mixed),
            202,
            "a partially stored batch is Accepted, not Conflict"
        );
    }

    /// The native entry is byte-exact with the same shape as the SOP conflict
    /// entry: only the Error Comment differs.
    #[test]
    fn study_conflict_entry_matches_the_conflict_shape() {
        assert_eq!(
            build_study_conflict_failed_sop_entry(SC, SI),
            "{\"00000902\":{\"Value\":[\"Study Instance UID already stored under a different patient\"],\"vr\":\"LO\"},\"00081150\":{\"Value\":[\"1.2.840.10008.5.1.4.1.1.2\"],\"vr\":\"UI\"},\"00081155\":{\"Value\":[\"1.2.3.4.5.6.7.1\"],\"vr\":\"UI\"},\"00081197\":{\"Value\":[272],\"vr\":\"US\"}}"
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
        let (status, _, _, warning) = build_stow_response(&warned, false);
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

    // -- Accept negotiation: the XML arm (Weasis "prolog" regression) -------

    /// `Accept: application/dicom+xml` must answer a parseable DICOM XML body.
    ///
    /// A dcm4che3-based sender (Weasis among them) parses the body of a 202 or
    /// 409 with `SAXReader.parse()`. Answering JSON to a request that declared
    /// XML threw `SAXParseException: Content is not allowed in prolog`: the
    /// instances were stored, but the client reported the export as failed.
    #[test]
    fn wants_xml_answers_a_native_dicom_model_document() {
        let mut accum = StowAccum::default();
        accum.record(
            &InstanceOutcome::Stored {
                sop_class_uid: SC.to_string(),
                sop_instance_uid: SI.to_string(),
                study_instance_uid: STU.to_string(),
                series_instance_uid: SER.to_string(),
                patient_id: "P1".to_string(),
            },
            None,
            None,
        );
        let (status, ct, body, _) = build_stow_response(&accum, true);
        assert_eq!(status, 200);
        assert_eq!(ct, crate::handlers::DICOM_XML_CONTENT_TYPE);
        let text = String::from_utf8(body).expect("utf-8");

        // The prolog is the FIRST thing in the body: dcm4che3 refuses anything
        // before it with exactly the error this fix removes.
        assert!(
            text.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"),
            "body must open with the XML declaration: {text}"
        );
        assert!(text.contains("<NativeDicomModel xmlns="), "{text}");
        assert!(text.contains("</NativeDicomModel>"), "{text}");
        // Both spellings the prompt pins: the keyword and the tag attribute.
        assert!(text.contains("tag=\"00081199\" vr=\"SQ\""), "{text}");
        assert!(text.contains("keyword=\"ReferencedSOPSequence\""), "{text}");
        assert!(text.contains("tag=\"00081150\""), "{text}");
        assert!(text.contains("tag=\"00081155\""), "{text}");
        assert!(text.contains("tag=\"00081190\""), "{text}");
        // The URI is the same one the JSON body builds.
        assert!(
            text.contains(
                "/dicomweb/studies/1.2.3.4.study/series/1.2.3.4.series/instances/1.2.3.4.5.6.7.1"
            ),
            "{text}"
        );
        // The envelope closes exactly once and every tag opened is closed:
        // a hand-built writer must not emit unbalanced markup.
        assert_eq!(text.matches("<DicomAttribute ").count(), 4, "{text}");
        assert_eq!(text.matches("</DicomAttribute>").count(), 4, "{text}");
        assert_eq!(text.matches("<Item number=\"1\">").count(), 1, "{text}");
        assert_eq!(text.matches("</Item>").count(), 1, "{text}");
    }

    /// The `FailedSOPSequence` arm, including the warn/failure reason values.
    #[test]
    fn wants_xml_renders_the_failed_sequence_and_its_reasons() {
        let mut accum = StowAccum::default();
        accum.record(
            &InstanceOutcome::Conflict {
                sop_class_uid: SC.to_string(),
                sop_instance_uid: SI.to_string(),
                existing_patient_id: "OTHER".to_string(),
                conflicting_patient_id: "P1".to_string(),
            },
            None,
            None,
        );
        let (status, ct, body, _) = build_stow_response(&accum, true);
        assert_eq!(status, 409);
        assert_eq!(ct, crate::handlers::DICOM_XML_CONTENT_TYPE);
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains("tag=\"00081198\" vr=\"SQ\""), "{text}");
        assert!(text.contains("keyword=\"FailedSOPSequence\""), "{text}");
        assert!(text.contains("tag=\"00081197\""), "{text}");
        // US is a NUMBER in the XML encoding (PS3.19 Table A.1-1).
        assert!(
            text.contains("<Value number=\"1\">272</Value>"),
            "failure reason must be the decimal 272: {text}"
        );
        // A pure conflict carries no Error Comment, so the standard tag must
        // be absent - nobody has anything to say about it.
        assert!(!text.contains("tag=\"00000902\""), "{text}");
        assert!(!text.contains("OTHER"), "no PHI on the wire: {text}");
    }

    /// PS3.18 I.2.1: Error Comment is (0000,0902) in EVERY representation.
    #[test]
    fn the_error_comment_uses_the_standard_tag_in_both_representations() {
        let mut accum = StowAccum::default();
        accum.record(
            &InstanceOutcome::StudyPatientConflict {
                sop_class_uid: SC.to_string(),
                sop_instance_uid: SI.to_string(),
                study_instance_uid: STU.to_string(),
            },
            None,
            None,
        );

        let (_, _, xml, _) = build_stow_response(&accum, true);
        let xml_text = String::from_utf8(xml).unwrap();
        assert!(
            xml_text.contains("tag=\"00000902\" vr=\"LO\" keyword=\"ErrorComment\""),
            "the standard Error Comment tag must be present: {xml_text}"
        );

        let (_, _, json, _) = build_stow_response(&accum, false);
        let json_text = String::from_utf8(json).unwrap();
        assert!(json_text.contains("\"00000902\""), "{json_text}");
        // The comment itself is identical in both, which is the point: the
        // field is one value rendered twice, not two independently written
        // strings that can drift.
        assert!(
            xml_text.contains(STUDY_PATIENT_CONFLICT_COMMENT),
            "{xml_text}"
        );
        assert!(
            json_text.contains(STUDY_PATIENT_CONFLICT_COMMENT),
            "{json_text}"
        );
    }

    /// PS3.19 A.1.1: item numbers are one-based WITHIN each sequence.
    #[test]
    fn xml_items_are_numbered_within_each_sequence() {
        let mut accum = StowAccum::default();
        for (i, sop) in ["1.2.3.4.1", "1.2.3.4.2"].iter().enumerate() {
            accum.record(
                &InstanceOutcome::Stored {
                    sop_class_uid: SC.to_string(),
                    sop_instance_uid: sop.to_string(),
                    study_instance_uid: STU.to_string(),
                    series_instance_uid: SER.to_string(),
                    patient_id: "P1".to_string(),
                },
                Some(format!("entry{i}")),
                None,
            );
        }
        for sop in ["1.2.3.4.9", "1.2.3.4.8"] {
            accum.record(
                &InstanceOutcome::Conflict {
                    sop_class_uid: SC.to_string(),
                    sop_instance_uid: sop.to_string(),
                    existing_patient_id: "OTHER".to_string(),
                    conflicting_patient_id: "P1".to_string(),
                },
                None,
                None,
            );
        }
        let (_, _, body, _) = build_stow_response(&accum, true);
        let text = String::from_utf8(body).unwrap();
        assert_eq!(text.matches("<Item number=\"1\">").count(), 2, "{text}");
        assert_eq!(text.matches("<Item number=\"2\">").count(), 2, "{text}");
        assert!(!text.contains("<Item number=\"3\">"), "{text}");
    }

    /// XML 1.0 section 2.2: a NUL is not a Char, so the document would be
    /// rejected whole - losing the successful references it also carries.
    #[test]
    fn xml_drops_characters_the_encoding_cannot_represent() {
        let mut accum = StowAccum::default();
        accum.record(
            &InstanceOutcome::Failed {
                sop_class_uid: None,
                sop_instance_uid: None,
                reason: "bad\u{0}byte\u{b}here".to_string(),
                class: FailureClass::InvalidInput,
            },
            None,
            None,
        );
        let (_, _, body, _) = build_stow_response(&accum, true);
        let text = String::from_utf8(body).unwrap();
        assert!(!text.contains('\u{0}'), "NUL leaked into the document");
        assert!(!text.contains('\u{b}'), "vertical tab leaked");
        // Replaced, not silently deleted: the reader can see something went.
        assert!(text.contains("bad\u{FFFD}byte\u{FFFD}here"), "{text}");
    }

    /// A request the server itself could not persist is not a bad request.
    #[test]
    fn an_all_store_failed_request_is_a_server_error_not_400() {
        let mut accum = StowAccum::default();
        accum.record(
            &InstanceOutcome::Failed {
                sop_class_uid: None,
                sop_instance_uid: None,
                reason: "the study's existing manifest is unreadable".to_string(),
                class: FailureClass::StoreFailure,
            },
            None,
            Some(build_failed_sop_entry(None, None, "unreadable manifest")),
        );
        assert_eq!(determine_status(&accum), 500);
        let (status, _, _, _) = build_stow_response(&accum, false);
        assert_eq!(status, 500);

        // A payload the client must fix still answers 400.
        let mut bad = StowAccum::default();
        bad.record(
            &InstanceOutcome::Failed {
                sop_class_uid: None,
                sop_instance_uid: None,
                reason: "not a data set".to_string(),
                class: FailureClass::InvalidInput,
            },
            None,
            Some(build_failed_sop_entry(None, None, "not a data set")),
        );
        assert_eq!(determine_status(&bad), 400);

        // And a mixed batch stays a partial success regardless of the class.
        let mut mixed = StowAccum::default();
        mixed.record(
            &InstanceOutcome::Stored {
                sop_class_uid: SC.to_string(),
                sop_instance_uid: SI.to_string(),
                study_instance_uid: STU.to_string(),
                series_instance_uid: SER.to_string(),
                patient_id: "P1".to_string(),
            },
            Some("ok".to_string()),
            None,
        );
        mixed.record(
            &InstanceOutcome::Failed {
                sop_class_uid: None,
                sop_instance_uid: None,
                reason: "manifest".to_string(),
                class: FailureClass::StoreFailure,
            },
            None,
            Some(build_failed_sop_entry(None, None, "manifest")),
        );
        assert_eq!(determine_status(&mixed), 202);
    }

    /// Demoting an item that had no JSON entry must not take the NEXT item's
    /// entry with it: XML and JSON have to agree about B.
    #[test]
    fn demoting_an_entryless_item_leaves_the_next_entry_intact() {
        let mut accum = StowAccum::default();
        // A: recorded without an entry.
        accum.record(
            &InstanceOutcome::Stored {
                sop_class_uid: SC.to_string(),
                sop_instance_uid: "1.2.3.4.A".to_string(),
                study_instance_uid: STU.to_string(),
                series_instance_uid: SER.to_string(),
                patient_id: "P1".to_string(),
            },
            None,
            None,
        );
        // B: recorded WITH an entry.
        accum.record(
            &InstanceOutcome::Stored {
                sop_class_uid: SC.to_string(),
                sop_instance_uid: "1.2.3.4.B".to_string(),
                study_instance_uid: STU.to_string(),
                series_instance_uid: SER.to_string(),
                patient_id: "P1".to_string(),
            },
            Some(build_referenced_sop_entry(SC, "1.2.3.4.B", STU, SER, false)),
            None,
        );
        assert!(accum.demote_to_failed("1.2.3.4.A", SC, "manifest"));

        let (_, _, json, _) = build_stow_response(&accum, false);
        let json_text = String::from_utf8(json).unwrap();
        assert!(
            json_text.contains("1.2.3.4.B"),
            "B must survive A's demotion: {json_text}"
        );

        let (_, _, xml, _) = build_stow_response(&accum, true);
        let xml_text = String::from_utf8(xml).unwrap();
        assert!(xml_text.contains("1.2.3.4.B"), "{xml_text}");
    }

    /// The sharper positional case: the entryless item sits FIRST and is NOT
    /// demoted, so any scheme that indexes entries by position walks off the
    /// list and silently drops B - while XML still reports it.
    #[test]
    fn an_entryless_item_does_not_shift_the_entries_of_later_items() {
        let mut accum = StowAccum::default();
        // A: no JSON entry, and it is DEMOTED.
        accum.record(
            &InstanceOutcome::Stored {
                sop_class_uid: SC.to_string(),
                sop_instance_uid: "1.2.3.4.A".to_string(),
                study_instance_uid: STU.to_string(),
                series_instance_uid: SER.to_string(),
                patient_id: "P1".to_string(),
            },
            None,
            None,
        );
        // C: with an entry, recorded BEFORE A is demoted.
        accum.record(
            &InstanceOutcome::Stored {
                sop_class_uid: SC.to_string(),
                sop_instance_uid: "1.2.3.4.C".to_string(),
                study_instance_uid: STU.to_string(),
                series_instance_uid: SER.to_string(),
                patient_id: "P1".to_string(),
            },
            Some(build_referenced_sop_entry(SC, "1.2.3.4.C", STU, SER, false)),
            None,
        );

        let (_, _, json, _) = build_stow_response(&accum, false);
        let json_text = String::from_utf8(json).unwrap();
        assert!(
            json_text.contains("1.2.3.4.C"),
            "C's entry belonged to C, not to its position: {json_text}"
        );
        let (_, _, xml, _) = build_stow_response(&accum, true);
        let xml_text = String::from_utf8(xml).unwrap();
        assert!(xml_text.contains("1.2.3.4.C"), "{xml_text}");
    }

    /// A coerced store carries Warn Reason (0008,1196) = 45056 in XML too.
    #[test]
    fn wants_xml_carries_the_warn_reason_for_a_coerced_instance() {
        let mut accum = StowAccum::default();
        accum.record(
            &InstanceOutcome::Stored {
                sop_class_uid: SC.to_string(),
                sop_instance_uid: SI.to_string(),
                study_instance_uid: STU.to_string(),
                series_instance_uid: SER.to_string(),
                patient_id: "P1".to_string(),
            },
            None,
            None,
        );
        accum.mark_last_referenced_coerced();
        // The caller bumps this alongside the mark (see the STOW path): the
        // accumulator does not infer "warned" from a flag it cannot see.
        accum.warned = 1;
        let (_, _, body, warning) = build_stow_response(&accum, true);
        let text = String::from_utf8(body).unwrap();
        assert!(
            text.contains("tag=\"00081196\" vr=\"US\" keyword=\"WarningReason\""),
            "{text}"
        );
        assert!(text.contains("<Value number=\"1\">45056</Value>"), "{text}");
        // The status/warning pair is unchanged by the body format.
        assert!(warning.is_some());
    }

    /// Untrusted text in a failure reason must be escaped, not injected.
    #[test]
    fn wants_xml_escapes_a_failure_reason() {
        let mut accum = StowAccum::default();
        accum.record(
            &InstanceOutcome::Failed {
                sop_class_uid: None,
                sop_instance_uid: None,
                reason: "bad </DicomAttribute><evil a=\"b\">&".to_string(),
                class: FailureClass::InvalidInput,
            },
            None,
            None,
        );
        let (_, _, body, _) = build_stow_response(&accum, true);
        let text = String::from_utf8(body).unwrap();
        assert!(!text.contains("<evil"), "markup must not survive: {text}");
        assert!(text.contains("&lt;/DicomAttribute&gt;&lt;evil"), "{text}");
        assert!(text.contains("&amp;"), "{text}");
        assert!(text.contains("&quot;b&quot;"), "{text}");
    }

    /// No Accept, or a JSON Accept, must keep the byte-exact JSON body.
    #[test]
    fn wants_json_keeps_the_frozen_body() {
        let mut accum = StowAccum::default();
        // The JSON body is assembled from the pre-rendered entries, exactly as
        // the STOW path supplies them, so the byte comparison is against the
        // real production shape rather than an empty accumulator.
        let entry = build_failed_sop_entry(None, None, "boom");
        accum.record(
            &InstanceOutcome::Failed {
                sop_class_uid: None,
                sop_instance_uid: None,
                reason: "boom".to_string(),
                class: FailureClass::InvalidInput,
            },
            None,
            Some(entry),
        );
        let (status, ct, body, _) = build_stow_response(&accum, false);
        assert_eq!(status, 400);
        assert_eq!(ct, crate::handlers::DICOM_JSON_CONTENT_TYPE);
        assert_eq!(
            String::from_utf8(body).unwrap(),
            "{\"00081198\":{\"Value\":[{\"00000902\":{\"Value\":[\"boom\"],\"vr\":\"LO\"},\"00081197\":{\"Value\":[272],\"vr\":\"US\"}}],\"vr\":\"SQ\"}}"
        );
    }
}
