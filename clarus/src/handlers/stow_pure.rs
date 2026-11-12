// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! STOW-RS pure synchronous core.
//!
//! Strangler-fig extraction: the per-instance storage pipeline (parse -> CAS ->
//! shadow -> CBOR manifest) moves here as a fully synchronous, axum-free,
//! `async`-free function. Both the axum wrappers (this stage) and the stage-8
//! dispatcher call [`ingest_instance`] from a blocking thread.
//!
//! The multipart **boundary** is invisible here. An instance arrives either as
//! in-memory `Bytes` (single `application/dicom`) or as an already-streamed
//! `TempFile` (multipart part, with its size computed by the wrapper DURING the
//! stream). These two paths are intentionally not merged - their semantics
//! differ (audit-embed only exists on the Bytes path).
//!
//! # Note on the hash
//! Every STOW instance is hashed once, with **BLAKE3**, streamed in 64 KiB
//! chunks (`clarus_core::hash::CasHasher`, the single content-address
//! facade). The digest is the CAS address (`cas_path(hex(blake3))`) and the
//! manifest instance key. The legacy/external SHA-256 was excised in
//! Part 3 (clean store, re-uploaded); the (0400,0561) audit copy is
//! BLAKE3-addressed too.

#![forbid(unsafe_code)]

use crate::handlers::json_common::{push_f64, push_json_str, push_u64};
use crate::state::AppState;
use clarus_adapters::dicom_rs::DicomRsParser;
use clarus_adapters::dicom_rs::frame::FrameTableError;
use clarus_adapters::storage::ContentAddressedStorage;
use clarus_ports::dicom::{DicomError, DicomParser, InstanceMeta};
use clarus_ports::stow_response::{InsertResult, InstanceOutcome};
use std::path::{Path, PathBuf};

// -- Error type ----------------------------------------------------------

/// A STOW error carrying an HTTP status, a body byte-identical to the current
/// `Json` error body (native bytes), and an optional `Retry-After` (preflights
/// only).
#[derive(Debug)]
pub struct StowError {
    pub status: u16,
    pub body: Vec<u8>,
    pub retry_after: Option<u64>,
}

impl StowError {
    pub fn plain(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            retry_after: None,
        }
    }
    pub fn with_retry(status: u16, body: Vec<u8>, retry_after: u64) -> Self {
        Self {
            status,
            body,
            retry_after: Some(retry_after),
        }
    }
}

/// `{"error":<error>,"message":<msg>}` (byte-exact with the legacy serde
/// `json!` render: BTreeMap sort puts error before message).
fn err_json(error: &str, msg: impl Into<String>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"{\"error\":");
    push_json_str(&mut out, error);
    out.extend_from_slice(b",\"message\":");
    push_json_str(&mut out, &msg.into());
    out.push(b'}');
    out
}

fn bad_request(msg: impl Into<String>) -> StowError {
    StowError::plain(400, err_json("Bad Request", msg))
}

fn internal_error(msg: impl Into<String>) -> StowError {
    StowError::plain(500, err_json("Internal Server Error", msg))
}

/// resurrection-guard: refuse a STOW into a tombstoned study.
///
/// IOCM 4.66.4.2.3.1 / 4.66.4.3.3: a receiver "shall not accept subsequent
/// occurrence of instances that have been hidden" under a patient-safety or
/// incorrect-MWL rejection.
///
/// The rule is scoped to exactly those two codes. 113039 (retention) is a
/// lifecycle marker - the data exists, its policy window lapsed - not a
/// statement that the content is wrong, so re-storing it is ordinary traffic.
/// A Quality hide is likewise re-storable: replacing a rejected AI result with
/// a corrected one is the intended workflow.
///
/// Returns the per-instance failure to report, or `None` when the store may
/// proceed. This is a PART-level refusal, never a request-level 4xx: PS3.18
/// 10.5.3.1 describes a mixture of stored and refused instances as a 202 whose
/// body carries the per-instance Failure Reason (0008,1197), and answering 4xx
/// would discard the parts of the same request that were acceptable.
fn refuse_if_hidden_from_store(
    state: &AppState,
    study_uid: &str,
    sop_instance_uid: &str,
    sop_class_uid: &str,
) -> Option<InstanceOutcome> {
    let manifest_dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
    let reason = clarus_core::stow::hidden_reason_of(&manifest_dir, study_uid, sop_instance_uid);
    // 113037 patient safety / 113038 incorrect MWL: no standard un-reject, and
    // the standard forbids re-accepting them.
    if reason != 113_037 && reason != 113_038 {
        return None;
    }
    crate::clog_warn!(
        sop_instance_uid = %sop_instance_uid,
        study_uid = %study_uid,
        reason = reason,
        "STOW refused: instance is hidden by an IOCM rejection that forbids re-acceptance"
    );
    // 272 (0110H, PS3.18 Table I.2-2 "Processing failure") is the honest
    // general code: the standard defines no code for this policy refusal, and
    // the Error Comment (0000,0902) names the reason so an operator can tell
    // it from an unrelated storage failure.
    Some(InstanceOutcome::Failed {
        sop_class_uid: Some(sop_class_uid.to_string()),
        sop_instance_uid: Some(sop_instance_uid.to_string()),
        reason: format!(
            "Instance is hidden by an IOCM rejection note (reason {reason}) that forbids re-acceptance"
        ),
    })
}

/// A deleted study carries a `{study_uid}.del` marker (the renamed manifest).
/// STOWing into it would resurrect a deleted study in the B-tree tier, so we
/// return 409 before any CAS write or DbActor insert. Once the erase-worker
/// lifts the tombstone, the UID is free again and STOW proceeds.
fn reject_tombstoned(state: &AppState, study_uid: &str) -> Result<(), StowError> {
    let manifest_dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
    if clarus_core::stow::tombstone_path(&manifest_dir, study_uid).is_file() {
        return Err(StowError::plain(
            409,
            err_json(
                "Conflict",
                format!("Study {study_uid} is deleted; re-ingest after erase completes"),
            ),
        ));
    }
    Ok(())
}

/// Error that includes SOP UIDs for FailedSOPSequence compliance.
///
/// Byte-exact with the legacy serde render: BTreeMap key order error, message,
/// sop_class_uid (when present), sop_instance_uid.
fn stow_error_with_sop(
    status: u16,
    msg: impl Into<String>,
    sop_class_uid: Option<&str>,
    sop_instance_uid: &str,
) -> StowError {
    let m = msg.into();
    let mut out = Vec::new();
    out.extend_from_slice(b"{\"error\":");
    push_json_str(&mut out, "STOW Error");
    out.extend_from_slice(b",\"message\":");
    push_json_str(&mut out, &m);
    if let Some(sc) = sop_class_uid {
        out.extend_from_slice(b",\"sop_class_uid\":");
        push_json_str(&mut out, sc);
    }
    out.extend_from_slice(b",\"sop_instance_uid\":");
    push_json_str(&mut out, sop_instance_uid);
    out.push(b'}');
    StowError::plain(status, out)
}

// -- Instance source -----------------------------------------------------

/// Where the raw DICOM instance comes from. The 2808 update core (decision,
/// canonical embed, manifest replace-merge) is shared; only the
/// materialization differs.
///
/// One variant: every STOW entry point pre-streams the part to a temp file.
/// The in-memory variant had no caller left, and a second ingest path with
/// its own guards is exactly how the two paths drifted apart.
pub enum InstanceSource<'a> {
    /// multipart part: already merged to a temp file with its size computed
    /// by the wrapper DURING the stream. `blake3` is the digest folded over
    /// the bytes AS WRITTEN (the wrapper's `CasWrite` tee) - it equals the
    /// final on-disk CAS bytes UNLESS `strip_trailing_crlf_file` truncates
    /// (the rare Weasis CRLF case), where `ingest_tempfile` recomputes.
    TempFile {
        path: &'a Path,
        size: u64,
        blake3: [u8; 32],
    },
}

// -- Defensive ingest normalization (field 07.09, study 8352) ------------

/// File variant: truncate bytes the DATASET does not own, returning the new
/// size. The temp file is written by the multipart wrapper right before this
/// call, so the tail check is two seeks + reads (no full pass).
///
/// Some STOW clients (field evidence: Weasis DICOMweb STOW) frame the
/// closing multipart part with a doubled CRLF, and the boundary scanner keeps
/// the first CRLF inside the part body. The stored blob then ends with
/// `\r\n`, which strict readers (dcm4che in Weasis) reject with EOFException.
///
/// The cut is bounded by where the LAST parsed element ends, never by the
/// shape of the tail alone. A native 16-bit sample of 0x0A0D is the bytes
/// `0D 0A` (PS3.5: PixelData is the final element of most files and its last
/// sample is arbitrary data), so a tail-shape test would delete up to 32 bytes
/// of real pixels for roughly one image in 65536 - storing a truncated blob
/// under a fresh content address and answering 200. Here a tail is only
/// removed when it lies BEYOND the dataset end; when the walk cannot decide,
/// nothing is touched.
fn strip_trailing_crlf_file(path: &Path, size: &mut u64) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let Some(dataset_end) = clarus_adapters::dicom_rs::frame::dataset_end_in_file(path) else {
        // Malformed or unreadable structure: leave the bytes exactly as they
        // came. Guessing is what loses pixels.
        return Ok(());
    };
    let mut new_size = *size;
    // Bytes past the dataset are not part of it. They can only be framing
    // junk, so they go; the loop keeps the historical tolerance for several
    // appended pairs.
    let mut guard = 0usize;
    loop {
        if new_size < 2 || new_size <= dataset_end || guard >= 16 {
            break;
        }
        let mut f = std::fs::OpenOptions::new().read(true).open(path)?;
        let mut tail = [0u8; 2];
        f.seek(SeekFrom::Start(new_size - 2))?;
        f.read_exact(&mut tail)?;
        if tail == *b"\r\n" {
            new_size -= 2;
            guard += 1;
        } else {
            break;
        }
    }
    if new_size != *size {
        let f = std::fs::OpenOptions::new().write(true).open(path)?;
        f.set_len(new_size)?;
        *size = new_size;
    }
    Ok(())
}

/// Result of a successful ingest - the `InstanceOutcome` plus the fields the
/// wrapper needs for the drive-by manifest group commit (P1).
pub struct IngestOutcome {
    pub outcome: InstanceOutcome,
    pub meta: InstanceMeta,
    /// True if FR-001 `validate_and_coerce` had to generate any Type 2 field
    /// (PatientID / PatientName / StudyInstanceUID). Drives the STOW Warn
    /// Reason `(0008,1196) = 45056` (B000, Coercion of Data Elements).
    pub coerced: bool,
    /// How many Type 2 fields the coercion generated (the journaled U-event's
    /// `changed_attrs` for a coerced INSERT).
    pub generated: usize,
    /// 3008: the incoming (0400,0550)/(0400,0561) carried identifying values
    /// and the served bytes were sanitized (the U-event's reason).
    pub hidden_phi_excised: bool,
}

// -- P1: request-scoped manifest group commit ----------------------------

/// One study's accepted (Inserted) instances accumulated in memory for the
/// group commit. The request pulls ALL of them into the study manifest with a
/// single `write_study_manifest_full` (O(N) instead of the old O(N^2) per-
/// instance read-merge-write).
pub struct StudyBatch {
    attrs: clarus_core::stow::StudyManifestAttrs,
    series: Vec<clarus_core::stow::SeriesModel>,
    /// Every modality (0008,0060) an accepted instance of this study declared.
    ///
    /// A `BTreeSet` rather than a `Vec`: the trigger wants the DISTINCT
    /// modalities and does not care about order, and the set keeps the
    /// per-request growth bounded by the number of modalities a study really
    /// has (a handful), not by its instance count.
    modalities: std::collections::BTreeSet<String>,
}

/// Request-scoped accumulator of accepted instances, grouped by study UID.
///
/// Populated per accepted (Inserted) instance during Stage A; flushed at the
/// end of the request by [`commit_manifests`]. The instance counters back the
/// `max_batch_instances` / `max_batch_bytes` memory guard: crossing either
/// forces an honest mid-request flush so a pathological batch degrades to
/// several bounded writes instead of unbounded in-memory growth.
#[derive(Default)]
pub struct ManifestBatch {
    studies: std::collections::HashMap<String, StudyBatch>,
    pub instances: u64,
    pub bytes: u64,
    /// Rejection notes accepted in this batch, as `(sop_class_uid,
    /// sop_instance_uid, reason_code)` of the instances each one names.
    ///
    /// Gathered here rather than applied inline because the referenced
    /// instances must already be committed before they can be flagged - a note
    /// and the images it rejects may arrive in the SAME request, and the note
    /// may even be listed first.
    ///
    /// The reason travels with the reference because the marking step needs
    /// it: the expose decision and the re-store rule both depend on WHICH
    /// reason fired, and re-deriving it from the table would mean the
    /// persisted code could differ from the one the note actually stated.
    ///
    /// The note's own SOP Instance UID travels too: the audit row names it so
    /// an auditor can open the KOS that justified the hide, and the batch is
    /// the last place that still knows which object the references came from.
    rejections: Vec<(String, String, u32, String)>,
    /// Study UIDs already flushed by a mid-request memory guard, with the
    /// per-study facts the follow-up work reads.
    ///
    /// `clear()` must not forget them: the post-STOW follow-up (CAD workitem,
    /// durability sync) derives its study list from this batch, and a flushed
    /// study that disappeared from the list would silently skip that work.
    ///
    /// A `HashMap` rather than a list because the follow-up does not only
    /// need the UID: it names the patient
    /// ([`Self::patient_identity`]) and picks the task from the modality set
    /// ([`Self::modalities`]). Once a study is flushed its `StudyBatch` is
    /// gone from `studies`, so those facts have to be kept HERE - a bare UID
    /// list answered `None` for exactly the studies that crossed the memory
    /// guard, which is why a large single-study upload lost its workitem.
    ///
    /// The key is the UID, so a study flushed twice within one request (its
    /// instances straddle the guard) still yields ONE entry.
    flushed_studies: std::collections::HashMap<String, FlushedStudy>,
    /// SOP Instance UIDs whose study manifest a mid-request flush could not
    /// write.
    ///
    /// They must survive [`Self::clear`] for the same reason as
    /// `flushed_studies`: the failed instance was already counted as accepted
    /// when it entered the batch, and the response is built from the
    /// accumulator AFTER the last flush. Reporting it here lets the caller move
    /// it into FailedSOPSequence (reason 272) instead of claiming a store the
    /// archive cannot serve back.
    manifest_failed: Vec<String>,
}

/// The study-level facts kept for a study a mid-request flush already wrote.
///
/// Same shape as the live `StudyBatch` where it matters - header attributes
/// plus the accumulated modality set - so the accessors below do not branch on
/// whether a study is still pending or already on disk.
pub struct FlushedStudy {
    attrs: clarus_core::stow::StudyManifestAttrs,
    modalities: std::collections::BTreeSet<String>,
}

impl ManifestBatch {
    /// Reset the accumulator after a commit.
    ///
    /// Rejections and the flushed-study list deliberately SURVIVE: the notes a
    /// request carried must still be applied at the end of the request (their
    /// referenced instances are on disk by then), and the studies already
    /// written must still be listed for the follow-up work. Only the pending
    /// write set is reset.
    fn clear(&mut self) {
        self.studies.clear();
        self.instances = 0;
        self.bytes = 0;
    }

    /// Take the UIDs of instances a mid-request flush could not commit.
    ///
    /// Draining rather than borrowing keeps a second flush from reporting the
    /// same instance twice - the caller records each one exactly once.
    pub fn take_manifest_failed(&mut self) -> Vec<String> {
        std::mem::take(&mut self.manifest_failed)
    }

    /// Study UIDs accepted into this request, flushed or still pending.
    ///
    /// One entry per study regardless of how many instances or series it
    /// contributed, which is exactly the granularity of the post-STOW
    /// per-study follow-up work. A study already written by a mid-request
    /// flush and one still pending appear ONCE each: the flushed set is keyed
    /// by UID, so instances of the same study on both sides of the guard do
    /// not produce a duplicate - the follow-up would otherwise mint the same
    /// workitem twice.
    pub fn study_uids(&self) -> impl Iterator<Item = &str> {
        self.flushed_studies.keys().map(|s| s.as_str()).chain(
            self.studies
                .keys()
                .filter(|uid| !self.flushed_studies.contains_key(uid.as_str()))
                .map(|s| s.as_str()),
        )
    }

    /// The study-level header attributes of one accepted study, pending or
    /// flushed.
    ///
    /// The header is identical across a study's instances, so whichever
    /// container answers is authoritative for the ATTRIBUTES - unlike the
    /// modality SET, which is per-instance and therefore a union of both
    /// containers (see [`Self::modalities`]).
    fn study_attrs(&self, study_uid: &str) -> Option<&clarus_core::stow::StudyManifestAttrs> {
        if let Some(study) = self.studies.get(study_uid) {
            return Some(&study.attrs);
        }
        self.flushed_studies.get(study_uid).map(|f| &f.attrs)
    }

    /// The patient identity of one accepted study, for the post-STOW
    /// follow-up work that has to NAME the patient.
    ///
    /// The values come from the study header the batch already carries (the
    /// study-level attributes are identical across a study's instances), so
    /// reading them costs a lookup rather than a second pass over the
    /// request. `None` for a study not in the batch, or a study whose
    /// header declared no Patient ID - the caller must not invent one.
    ///
    /// This is what AIW-I 4.80.4.1.2.1 needs the Task Requester to state:
    /// the Patient ID of the task, and its issuer.
    #[must_use]
    pub fn patient_identity(&self, study_uid: &str) -> Option<(&str, Option<&str>)> {
        let attrs = self.study_attrs(study_uid)?;
        let patient_id = attrs.patient_id.as_deref()?;
        Some((patient_id, attrs.patient_id_issuer.as_deref()))
    }

    /// The study's Modality (0008,0060) as the stored header declares it.
    ///
    /// Like [`Self::patient_identity`], this is a lookup in data the batch
    /// already holds (the study header, identical across a study's instances),
    /// so reading it costs nothing extra on the STOW path. `None` for a study
    /// not in the batch; an EMPTY string when the header declared no modality,
    /// which the caller distinguishes rather than papering over - the workitem
    /// trigger routes both cases to its configured fallback code.
    ///
    /// This is the header's ONE modality. The study's real modality SET is
    /// [`Self::modalities`], which is what the mint uses.
    #[must_use]
    pub fn modality(&self, study_uid: &str) -> Option<&str> {
        let attrs = self.study_attrs(study_uid)?;
        let modality = &attrs.modality;
        (!modality.is_empty()).then_some(modality.as_str())
    }

    /// The study's distinct modality SET, sorted, as accepted into this batch.
    ///
    /// A study is not one modality: a performer reads its images and writes a
    /// report back into the same study, so the set is routinely `{DX, SR}`.
    /// The trigger reads the SET, because choosing the task from whichever
    /// instance happened to arrive first makes the offered task depend on
    /// upload order.
    ///
    /// An empty modality (a header that declared none) contributes NOTHING
    /// rather than an empty string: the empty string is not a modality, and
    /// the caller distinguishes "no modality at all" from "a modality the
    /// table does not name" by the set being empty.
    ///
    /// Sorted so the caller's decision does not depend on insert order; the
    /// extraction cost is one small `Vec` per study per request.
    ///
    /// The set is the UNION of both containers, not whichever one answers
    /// first: at the end of a request whose guard fired, a study that received
    /// more instances afterwards sits in `studies` AND in `flushed_studies`.
    /// Reading only the live half would report `{SR}` for a study that is
    /// really `{DX, SR}`, and the trigger would pick its task from the
    /// residues of the upload rather than from the study.
    #[must_use]
    pub fn modalities(&self, study_uid: &str) -> Vec<String> {
        let mut set: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        if let Some(study) = self.studies.get(study_uid) {
            set.extend(study.modalities.iter().map(|m| m.as_str()));
        }
        if let Some(flushed) = self.flushed_studies.get(study_uid) {
            set.extend(flushed.modalities.iter().map(|m| m.as_str()));
        }
        let mut out: Vec<String> = set
            .into_iter()
            .filter(|m| !m.is_empty())
            .map(|m| m.to_string())
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}
/// Build the study-level manifest attributes from one inserted instance's
/// metadata (the study header - all instances of one study share it).
fn study_attrs_from_meta(meta: &InstanceMeta) -> clarus_core::stow::StudyManifestAttrs {
    clarus_core::stow::StudyManifestAttrs {
        patient_name: Some(meta.patient_name.clone()),
        patient_id: Some(meta.patient_id.clone()),
        patient_id_issuer: meta.patient_id_issuer.clone(),
        patient_birth_date: meta.patient_birth_date.clone(),
        patient_sex: meta.patient_sex.clone(),
        study_instance_uid: meta.study_instance_uid.clone(),
        study_date: meta.study_date.clone(),
        study_time: meta.study_time.clone(),
        study_description: meta.study_description.clone(),
        accession_number: meta.accession_number.clone(),
        referring_physician: meta.referring_physician.clone(),
        performing_physician: meta.performing_physician.clone(),
        name_of_reading_physicians: meta.name_of_reading_physicians.clone(),
        institution_name: meta.institution_name.clone(),
        requesting_physician: meta.requesting_physician.clone(),
        requested_procedure_description: meta.requested_procedure_description.clone(),
        responsible_person: meta.responsible_person.clone(),
        // (0010,1002 > 0010,0020) is multi-valued; the manifest stores it
        // comma-joined, the same shape ModalitiesInStudy uses.
        other_patient_ids: (!meta.other_patient_ids.is_empty())
            .then(|| meta.other_patient_ids.join(",")),
        modality: meta.modality.clone(),
        study_id: meta.study_id.clone(),
        num_series: 0,
        num_instances: 0,
        modalities_in_study: None,
    }
}

/// Map a `FrameChunk` stream onto manifest `FrameRecord`s.
///
/// The map (number, zeroed hash, offset, compressed_len) is defined ONCE and
/// shared by the bytes path ([`frame_records`]) and the streaming file path
/// ([`frame_records_from_file`]), so the two can never drift.
///
/// The per-frame BLAKE3 is left zero: nothing consumes it yet, and hashing
/// every tile would add a full pass over a multi-GB instance at STOW.
fn frame_chunks_to_records(
    chunks: Vec<clarus_adapters::dicom_rs::frame::FrameChunk>,
) -> Vec<clarus_core::manifest::FrameRecord> {
    chunks
        .into_iter()
        .map(|chunk| clarus_core::manifest::FrameRecord {
            // `compute_frame_chunks` numbers frames as u32, and the record
            // field is u32, so every frame is kept - a whole-slide instance
            // above 65535 tiles is tabled in full rather than truncated.
            number: chunk.number,
            hash: [0u8; 32],
            offset: chunk.offset,
            compressed_len: chunk.len,
        })
        .collect()
}

/// Compute an instance's manifest frame-chunk table from its stored bytes,
/// enforcing the F5 frame cap.
///
/// One walk of the PixelData item headers: no pixel decode, no pixel copy.
/// Returns an EMPTY table when the file is single-frame or its geometry is
/// unreadable; the manifest then keeps the whole-blob serve path, so a
/// failure here can never make a servable frame unservable.
///
/// `max_frames` is the F5 cap. `Err(TooManyFrames)` is a REJECT signal: the
/// caller must refuse the instance rather than store it without a table
/// (which would reopen the whole-blob serve path and the read amplification).
///
/// This is the IN-MEMORY path: used by the sanitize branch, which already
/// holds the sanitized bytes.
pub(crate) fn frame_records_capped(
    dicom_bytes: &[u8],
    max_frames: u32,
) -> Result<Vec<clarus_core::manifest::FrameRecord>, FrameTableError> {
    clarus_adapters::dicom_rs::frame::compute_frame_chunks_capped(dicom_bytes, max_frames)
        .map(frame_chunks_to_records)
}

/// Compute an instance's manifest frame-chunk table by STREAMING the stored
/// file (no whole-instance read-back), enforcing the F5 frame cap.
///
/// Same output as [`frame_records_capped`], but the header walk uses
/// `Read + Seek` over the CAS file, so a 13 GB WSI costs a bounded header walk
/// instead of a 13 GB `std::fs::read`. Any read/seek/truncation problem
/// becomes an EMPTY table inside the file variant - only `TooManyFrames` is an
/// `Err`, because that is the one case the caller must REJECT.
///
/// This is the path the STOW `Inserted` arm uses.
pub(crate) fn frame_records_from_file_capped(
    path: &std::path::Path,
    max_frames: u32,
) -> Result<Vec<clarus_core::manifest::FrameRecord>, FrameTableError> {
    clarus_adapters::dicom_rs::frame::compute_frame_chunks_from_file_capped(path, max_frames)
        .map(frame_chunks_to_records)
}

/// Uncapped convenience for callers that only need a table for an already
/// accepted instance (the rebuild worker, tests). Uses the ceiling as the cap,
/// so it can never return `TooManyFrames`.
pub(crate) fn frame_records(dicom_bytes: &[u8]) -> Vec<clarus_core::manifest::FrameRecord> {
    frame_chunks_to_records(clarus_adapters::dicom_rs::frame::compute_frame_chunks(
        dicom_bytes,
    ))
}

/// Map the F5 `TooManyFrames` signal onto a STOW reject (413 Payload Too
/// Large), naming the declared/walked counts and the configured cap.
///
/// F5 is a REJECT, never "store without a table": storing an over-cap
/// multi-frame instance without a frame table would hand every `/frames/{N}`
/// request the whole-blob read path - exactly the O(instance) amplification
/// this task removes.
fn too_many_frames_error(
    sop_class_uid: &str,
    sop_instance_uid: &str,
    e: FrameTableError,
) -> StowError {
    let (declared, walked, cap) = match e {
        FrameTableError::TooManyFrames {
            declared,
            walked,
            cap,
        } => (declared, walked, cap),
    };
    crate::clog_warn!(
        sop_instance_uid = %sop_instance_uid,
        declared,
        walked,
        cap,
        "STOW 413: frame count exceeds max_frames_per_instance"
    );
    stow_error_with_sop(
        413,
        format!(
            "Instance declares/holds more than max_frames_per_instance ({cap}): declared={declared}, walked={walked}"
        ),
        Some(sop_class_uid),
        sop_instance_uid,
    )
}

/// Build the single-instance `SeriesModel` (matches the pre-P1 per-instance
/// manifest shape: one series per instance, merged by SeriesInstanceUID).
fn instance_series_model(
    meta: &InstanceMeta,
    blake3: [u8; 32],
    file_size: u64,
    frames: Vec<clarus_core::manifest::FrameRecord>,
) -> clarus_core::stow::SeriesModel {
    let instance = clarus_core::stow::InstanceModel {
        sop_instance_uid: meta.sop_instance_uid.clone(),
        sop_class_uid: meta.sop_class_uid.clone(),
        instance_number: meta.instance_number.map(|n| n.to_string()),
        blake3,
        storage_tier: clarus_core::manifest::TIER_HOT,
        transfer_syntax_uid: meta
            .transfer_syntax_uid
            .clone()
            .unwrap_or_else(|| "1.2.840.10008.1.2.1".to_string()),
        file_size,
        referenced_series_uids: meta.referenced_series_uids.clone(),
        referenced_sop_instance_uids: meta.referenced_sop_instance_uids.clone(),
        frames,
    };
    clarus_core::stow::SeriesModel {
        series_instance_uid: meta.series_instance_uid.clone(),
        modality: meta.modality.clone(),
        series_number: meta.series_number.map(|n| n.to_string()),
        series_date: meta.series_date.clone(),
        series_time: meta.series_time.clone(),
        pps_start_date: meta.pps_start_date.clone(),
        pps_start_time: meta.pps_start_time.clone(),
        scheduled_procedure_step_id: meta.scheduled_procedure_step_id.clone(),
        requested_procedure_id: meta.requested_procedure_id.clone(),
        station_name: meta.station_name.clone(),
        manufacturer: meta.manufacturer.clone(),
        institutional_department_name: meta.institutional_department_name.clone(),
        operators_name: meta.operators_name.clone(),
        series_description: meta.series_description.clone(),
        body_part_examined: meta.body_part_examined.clone(),
        protocol_name: meta.protocol_name.clone(),
        laterality: meta.laterality.clone(),
        instances: vec![instance],
    }
}

/// Group a flat set of per-instance `SeriesModel`s into ONE consolidated
/// `SeriesModel` per SeriesInstanceUID.
///
/// `batch_add` pushes one `instance_series_model` per ACCEPTED instance onto
/// `StudyBatch.series` (one instance per model, not grouped). On a fresh study
/// `write_study_manifest_full` maps each model to a manifest series row 1:1,
/// so a single 432-instance series would otherwise split into 432 rows. This
/// consolidates the flat list by series UID, merging instance lists - the
/// series-level attributes are identical across a series, so the first-seen
/// model's header wins (field fix A, 24.08).
fn group_series_by_uid(
    series: &[clarus_core::stow::SeriesModel],
) -> Vec<clarus_core::stow::SeriesModel> {
    let mut by_uid: std::collections::HashMap<&str, clarus_core::stow::SeriesModel> =
        std::collections::HashMap::new();
    for s in series {
        if let Some(acc) = by_uid.get_mut(s.series_instance_uid.as_str()) {
            acc.instances.extend(s.instances.iter().cloned());
        } else {
            by_uid.insert(s.series_instance_uid.as_str(), s.clone());
        }
    }
    by_uid.into_values().collect()
}

/// Carry the per-study facts of the studies now being cleared into
/// `flushed_studies`.
///
/// Called by [`batch_add`] immediately before [`ManifestBatch::clear`]: at
/// that moment the live map still holds the studies being written, and after
/// `clear()` it holds nothing. Everything the post-STOW follow-up needs from a
/// flushed study - patient identity, modality set - has to be copied here,
/// because `studies` will no longer answer for it.
///
/// A study already recorded by an earlier flush MERGES rather than being
/// overwritten: its instances straddled the guard, and overwriting would drop
/// the modalities accepted before the first flush.
fn carry_flushed_facts(batch: &mut ManifestBatch) {
    for (study_uid, study) in &batch.studies {
        let entry = batch
            .flushed_studies
            .entry(study_uid.clone())
            .or_insert_with(|| FlushedStudy {
                attrs: study.attrs.clone(),
                modalities: std::collections::BTreeSet::new(),
            });
        entry.modalities.extend(study.modalities.iter().cloned());
    }
}

/// Push an accepted instance's manifest data into the request-scoped batch.
///
/// The memory guard (`max_batch_instances` / `max_batch_bytes`) is enforced
/// HERE: once the accumulated batch reaches a bound, it is flushed (one
/// `write_study_manifest_full` per study) and the accumulator resets, so an
/// oversized request keeps working in bounded chunks rather than growing
/// unbounded or failing the whole upload.
fn batch_add(
    state: &AppState,
    batch: &mut ManifestBatch,
    meta: &InstanceMeta,
    blake3: [u8; 32],
    file_size: u64,
    frames: Vec<clarus_core::manifest::FrameRecord>,
) {
    if batch.instances >= state.config.max_batch_instances
        || batch.bytes.saturating_add(file_size) > state.config.max_batch_bytes
    {
        // A flush mid-request must leave the request in the same state it
        // would have reached by flushing once at the end: the manifests are
        // committed AND synced here (the end-of-request sync would no longer
        // see them), and the studies stay listed for the follow-up work.
        // Instances the flush could not commit stay on the batch so the
        // response reports them as failed.
        let uncommitted = commit_manifests(state, batch);
        batch.manifest_failed.extend(uncommitted);
        sync_batch_manifests(state, batch);
        carry_flushed_facts(batch);
        batch.clear();
    }
    let study = batch
        .studies
        .entry(meta.study_instance_uid.clone())
        .or_insert_with(|| StudyBatch {
            attrs: study_attrs_from_meta(meta),
            series: Vec::new(),
            modalities: std::collections::BTreeSet::new(),
        });
    study
        .series
        .push(instance_series_model(meta, blake3, file_size, frames));
    // A rejection note leaves its references on the batch. Applied after the
    // commit, never here: the instances it names may not be stored yet.
    if !meta.rejection_referenced_sops.is_empty() {
        let reason = meta.rejection_reason_code;
        let note_uid = meta.sop_instance_uid.clone();
        batch
            .rejections
            .extend(
                meta.rejection_referenced_sops
                    .iter()
                    .map(|(sop_class, sop_instance)| {
                        (
                            sop_class.clone(),
                            sop_instance.clone(),
                            reason,
                            note_uid.clone(),
                        )
                    }),
            );
    }
    // The study's modality SET, accumulated per instance: the header's single
    // (0008,0060) describes one instance, not the study, and the trigger must
    // not depend on which instance arrived first.
    study.modalities.insert(meta.modality.clone());
    batch.instances = batch.instances.saturating_add(1);
    batch.bytes = batch.bytes.saturating_add(file_size);
}

/// Commit every study in the batch with a SINGLE manifest write per study.
///
/// Each study's accumulated series are written together via one atomic
/// `write_study_manifest_full` (it re-reads + merges prior instances under the
/// existing per-study coordinator lease, so parallel requests to the same
/// study still converge). The RAM study index is refreshed once per study via
/// Write one study manifest per study in the batch, returning the instance
/// UIDs whose manifest could not be written.
///
/// The response is built FROM the returned list - a study whose manifest write
/// failed is reported as `FailedSOPSequence` with reason 272 rather than
/// silently kept as `Stored`. The instance bytes are in the CAS either way, but
/// an instance no manifest names is not retrievable by any QIDO/WADO route, so
/// telling the client "stored" would be a claim the archive cannot honour.
///
/// The shadow-tier scanner is NOT told about a study that failed: its index
/// would then advertise instances no manifest backs.
pub fn commit_manifests(state: &AppState, batch: &ManifestBatch) -> Vec<String> {
    let manifest_dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
    let mut failed: Vec<String> = Vec::new();
    for (study_uid, study) in &batch.studies {
        // Rebuild fresh attrs so `write_study_manifest_full` stamps the final
        // merged counts (RAM-only) after merging all accumulated series.
        let mut attrs = study.attrs.clone();
        // Group the per-instance series models by SeriesInstanceUID so one
        // series yields ONE manifest row (field fix A; `study.series` is a flat
        // per-instance list from `batch_add`).
        let series = group_series_by_uid(&study.series);
        if let Err(e) =
            clarus_core::stow::write_study_manifest_full(&manifest_dir, &mut attrs, &series)
        {
            crate::clog_warn!(
                error = %e,
                study_uid = %study_uid,
                "B-tree study manifest write failed: instances of this study are reported failed"
            );
            failed.extend(
                study
                    .series
                    .iter()
                    .flat_map(|series| series.instances.iter())
                    .map(|instance| instance.sop_instance_uid.clone()),
            );
        } else {
            state.study_scanner.on_study_committed(study_uid, &attrs);
        }
    }
    failed
}

/// Apply the rejection notes a committed batch carried.
///
/// Runs AFTER `commit_manifests`, so the instances a note names are already on
/// disk whether they arrived in this request or an earlier one. For each
/// referenced SOP the owner study is resolved through the SOP->study index and
/// that study's manifest is mutated; a note may therefore cross studies.
///
/// Two independent guards, in this order:
///
/// 1. CONSENT. A Quality rejection stays inside the AI-result classes, so
///    honouring it automatically cannot hide a diagnostic image. The IOCM
///    reasons (`Scope::AnyClass`) can - they exist to hide the original CT/MR
///    a wrong-patient or wrong-worklist acquisition produced - and DICOM
///    defines no un-reject for them (PS3.16 CID 7010, IHE RAD TF-2 4.66.4).
///    They are therefore honoured only when the deployment has explicitly
///    enabled the feature (`[server] anyclass_enable`). A KOS is an ordinary
///    object any client able to reach STOW can write, and letting one hide
///    diagnostic images would be a denial of service against patient care.
///    The note itself is still STORED - it is a valid SOP - only its hiding
///    power is declined, and the event is logged and counted.
///
/// 2. SCOPE. A reference is flagged only when its SOP class is allowed under
///    the reason's [`clarus_ports::rejection::Scope`].
///
/// Best-effort throughout: a failure to flag is logged and never fails the
/// STOW response, because the stored instances are already durable and a
/// non-sequestered instance is a visibility issue, not data loss.
///
/// ap101: N/A. This loop runs only for a batch that contained a Key Object
/// Selection with a recognised reason code. A typical image STOW never enters
/// it - `batch.rejections` is empty and the function returns immediately.
pub fn apply_rejections(state: &AppState, batch: &ManifestBatch, user: &str) {
    if batch.rejections.is_empty() {
        return;
    }
    // Group by (study, reason) so one manifest is mutated once per reason per
    // request, and so the metric can attribute every applied hide.
    let mut by_study: std::collections::HashMap<String, Vec<(&str, u32, &str)>> =
        std::collections::HashMap::new();
    let mut skipped_scope = 0usize;
    let mut ignored_disabled = 0usize;
    for (sop_class_uid, sop_instance_uid, reason, note_uid) in &batch.rejections {
        // Guard 1: an AnyClass reason is destructive and has no standard
        // un-reject, so the deployment must opt into the feature explicitly.
        let any_class = clarus_ports::rejection::REASONS.iter().any(|r| {
            clarus_ports::rejection::reason_code_to_u32(r.code_value) == Some(*reason)
                && r.scope == clarus_ports::rejection::Scope::AnyClass
        });
        if any_class && !state.config.anyclass_enable {
            ignored_disabled += 1;
            continue;
        }
        // Guard 2: the class bound of the reason that actually fired.
        if !clarus_ports::rejection::may_hide_code(*reason, sop_class_uid) {
            skipped_scope += 1;
            continue;
        }
        match state.shadow.sop_owner_study(sop_instance_uid) {
            Some(study_uid) => by_study.entry(study_uid).or_default().push((
                sop_instance_uid.as_str(),
                *reason,
                note_uid.as_str(),
            )),
            None => {
                crate::clog_warn!(
                    sop_instance_uid = %sop_instance_uid,
                    "rejection note names an instance with no owner study (ignored)"
                );
            }
        }
    }

    if ignored_disabled > 0 {
        // One WARN per peer per window: a note that names hundreds of images
        // must not turn into hundreds of log lines, and the COUNT is what an
        // operator needs - the per-instance detail is in the metric.
        crate::metrics::record_iocm_anyclass_ignored(ignored_disabled as u64);
        crate::clog_warn!(
            instances = ignored_disabled,
            "IOCM_ANYCLASS_IGNORED: reason is an IOCM AnyClass code and [server] anyclass_enable is false, so hiding diagnostic images is not enabled on this deployment. The note is stored; the referenced instances stay visible. Set [server] anyclass_enable = true only when the STOW surface is restricted to trusted modalities.",
        );
    }

    let manifest_dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
    let audit_dir = std::path::PathBuf::from(&state.config.btree.audit_dir);
    let mut hidden_by_reason: std::collections::HashMap<u32, u64> =
        std::collections::HashMap::new();
    for (study_uid, entries) in by_study {
        // Several reasons may name the same instance in one batch. Applying
        // them in code order would make the stored reason depend on request
        // order, so group by reason and let the FIRST hide win: `mark_rejected`
        // never overwrites an existing reason, which is what keeps a Quality
        // hide from being replaced by an IOCM one (or vice versa).
        // A BTreeMap, not a HashMap: the note cited by the audit row is the
        // FIRST that named this (study, reason), and "first" has to be a
        // fixed order of the reasons rather than whatever the hasher happened
        // to produce for this process. With a HashMap the same batch could
        // cite a different note after a restart.
        let mut by_reason: std::collections::BTreeMap<u32, (Vec<&str>, &str)> =
            std::collections::BTreeMap::new();
        for (sop_instance_uid, reason, note_uid) in entries {
            let slot = by_reason.entry(reason).or_insert_with(|| (Vec::new(), ""));
            slot.0.push(sop_instance_uid);
            // The first note that named this (study, reason) is the one the
            // audit row cites; a second note for the same group carries no
            // extra evidence, and one row per group is the stated shape.
            if slot.1.is_empty() {
                slot.1 = note_uid;
            }
        }
        for (reason, (sop_uids, note_uid)) in by_reason {
            // FR-014 order: the durable record of the sequester lands BEFORE
            // the manifest mutation. A crash between the two then overstates
            // the trail (a row for a hide that did not finish) rather than
            // losing it, which is the same direction delete takes.
            append_reject_audit(
                &audit_dir,
                user,
                reason,
                &study_uid,
                note_uid,
                sop_uids.len() as u64,
            );
            // No `on_study_committed` here on purpose. That call upserts the
            // study's QIDO record from the attrs it is given; a rejection
            // changes no study-level attribute, and passing a placeholder
            // would overwrite a valid index entry with an empty one. The
            // reason lives in the manifest, which is what the instance-level
            // paths re-read.
            match clarus_core::stow::mark_rejected(&manifest_dir, &study_uid, &sop_uids, reason) {
                Ok(n) => {
                    *hidden_by_reason.entry(reason).or_insert(0) += n as u64;
                }
                Err(e) => {
                    crate::clog_warn!(
                        error = %e,
                        study_uid = %study_uid,
                        "rejection marking failed (study stays visible, client unaffected)"
                    );
                }
            }
        }
    }

    let hidden: u64 = hidden_by_reason.values().sum();
    if hidden > 0 || skipped_scope > 0 {
        crate::clog_info!(
            hidden = hidden,
            skipped_scope = skipped_scope,
            "rejection note applied"
        );
    }
    // Only the flip count feeds the metric: `hidden` is the number
    // `mark_rejected` actually changed, so a repeated note adds nothing and
    // the rate reflects state changes rather than references read out of a
    // KOS. The reason label is what makes a rise attributable to Quality
    // (reviewable) or to an IOCM hide (permanent).
    for (reason, count) in hidden_by_reason {
        crate::metrics::record_rejected_instances(reason, count);
    }
}

/// Record one (study, reason) sequester in the immutable trail.
///
/// Best-effort: a hide that cannot be journalled is logged and still applied.
/// The alternative - refusing to hide because the audit disk is full - would
/// leave a patient-safety image visible to satisfy a logging constraint, and
/// the hide is the clinically urgent half of the two.
fn append_reject_audit(
    audit_dir: &std::path::Path,
    user: &str,
    reason: u32,
    study_uid: &str,
    note_uid: &str,
    count: u64,
) {
    let event = crate::audit_log::AuditEvent::Reject {
        user: user.to_string(),
        reason_code: reason,
        study: study_uid.to_string(),
        note_uid: note_uid.to_string(),
        count,
        ts: crate::audit_log::utc_now_iso(),
    };
    if let Err(e) = crate::audit_log::append_record(audit_dir, &event) {
        crate::clog_warn!(
            error = %e,
            study_uid = %study_uid,
            reason = reason,
            "rejection audit append failed (hide still applied)"
        );
    }
}

/// Durability sync point for a committed batch: make the just-written study/// manifests durable before the 200/202 response leaves (P2 - "200 =
/// committed").
///
/// The sync-set is the manifest files this batch wrote (one per study - the
/// source of truth). The `.idx` n-gram accelerator is deliberately NOT synced:
/// it is best-effort and self-healing (a missing n-gram only widens the next
/// fuzzymatch back to a full manifest scan, which still converges).
///
/// `std::fs::File::sync_all` is the cross-platform honest barrier:
/// `FlushFileBuffers` on Windows, `fsync` on Unix. On Windows
/// `FlushFileBuffers` requires a handle opened with GENERIC_WRITE, so the
/// manifest is opened `write(true)` (Linux fsync forgivingly accepts O_RDONLY,
/// the write handle is harmless there too - a POSIX EBADF guard). Sync errors
/// are best-effort (a `clog_warn`, like the manifest-write path) and never
/// fail the accepted request.
///
/// Parent-dir sync is best-effort with an OS boundary: after the atomic
/// tmp+rename, making the DIRECTORY entry durable also needs the directory
/// flushed. Windows cannot open a directory for `FlushFileBuffers`, so that
/// part is skipped there (`#[cfg(unix)]` only) and documented - on Windows the
/// file is synced but the directory entry is the OS's zone.
pub fn sync_batch_manifests(state: &AppState, batch: &ManifestBatch) {
    let manifest_dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
    for study_uid in batch.studies.keys() {
        let path = clarus_core::stow::manifest_path(&manifest_dir, study_uid);
        let opened = std::fs::File::options().write(true).open(&path);
        if let Err(e) = opened.and_then(|f| f.sync_all()) {
            crate::clog_warn!(
                error = %e,
                study_uid = %study_uid,
                "study manifest sync failed (best-effort, client unaffected)"
            );
        }
    }
    // One directory sync for the whole batch on Unix (best-effort; Windows
    // cannot FlushFileBuffers a directory handle - documented OS boundary).
    #[cfg(unix)]
    if let Err(e) = std::fs::File::open(&manifest_dir).and_then(|d| d.sync_all()) {
        crate::clog_warn!(
            error = %e,
            "manifest dir sync failed (best-effort, client unaffected)"
        );
    }
}

// -- Preflights (already-sync, moved as-is) ------------------------------

/// The RETRY-AFTER seconds for every `507 Insufficient Storage` (preflight and
/// mid-stream ENOSPC alike).
pub const INSUFFICIENT_STORAGE_RETRY_AFTER: u64 = 300;

/// Build the `507 Insufficient Storage` error from the CURRENT free space.
///
/// ONE home for the 507 body shape, shared by [`preflight_disk`] and the STOW
/// wrapper's mid-stream ENOSPC path (`dispatch_streams.rs`). BTreeMap key
/// order `disk_free_mb`, `error`, `message`; `Retry-After` = 300.
pub fn insufficient_storage_error(free_mb: f64) -> StowError {
    let mut out = Vec::new();
    out.extend_from_slice(b"{\"disk_free_mb\":");
    push_u64(&mut out, free_mb.max(0.0) as u64);
    out.extend_from_slice(b",\"error\":\"Insufficient Storage\",\"message\":\"Disk full - storage expansion or cleanup required\"}");
    StowError::with_retry(507, out, INSUFFICIENT_STORAGE_RETRY_AFTER)
}

/// BUG-003: disk-space preflight -> 507 + Retry-After.
pub fn preflight_disk(state: &AppState) -> Result<(), StowError> {
    let min_mb = state.config.min_free_disk_mb;
    let min_pct = state.config.min_free_disk_pct;

    if min_mb == 0 && min_pct == 0.0 {
        return Ok(());
    }

    let free_mb = crate::disk_utils::available_disk_mb(state.cas.root_dir());
    let retry_after = 300u64;

    if min_mb > 0 && free_mb < min_mb as f64 {
        // BTreeMap order: disk_free_mb, error, message, min_required_mb.
        let mut out = Vec::new();
        out.extend_from_slice(b"{\"disk_free_mb\":");
        push_u64(&mut out, free_mb as u64);
        out.extend_from_slice(b",\"error\":\"Insufficient Storage\",\"message\":\"Disk full - storage expansion or cleanup required\",\"min_required_mb\":");
        push_u64(&mut out, min_mb);
        out.push(b'}');
        return Err(StowError::with_retry(507, out, retry_after));
    }
    if min_pct > 0.0 {
        let total_mb = crate::disk_utils::total_disk_mb(state.cas.root_dir());
        if total_mb > 0.0 {
            let free_pct = (free_mb / total_mb) * 100.0;
            if free_pct < min_pct {
                // BTreeMap order: disk_free_mb, disk_free_pct, error, message,
                // min_required_pct.
                let mut out = Vec::new();
                out.extend_from_slice(b"{\"disk_free_mb\":");
                push_u64(&mut out, free_mb as u64);
                out.extend_from_slice(b",\"disk_free_pct\":");
                push_f64(&mut out, free_pct);
                out.extend_from_slice(b",\"error\":\"Insufficient Storage\",\"message\":\"Disk critically low - storage expansion or cleanup required\",\"min_required_pct\":");
                push_f64(&mut out, min_pct);
                out.push(b'}');
                return Err(StowError::with_retry(507, out, retry_after));
            }
        }
    }

    Ok(())
}

// -- Ingest dispatcher ---------------------------------------------------

/// reject instances whose Study/Series/SOP UIDs violate the DICOM UID
/// grammar BEFORE any CAS write. WADO already validates UIDs on every read;
/// STOW must not accept an instance it can never address again (a later
/// QIDO/WADO on the same UID refuses it as invalid). The bridge forwards
/// modality traffic unchanged, so a real device sending invalid UIDs is a
/// data-quality signal, not a reason to archive unaddressable objects.
pub(crate) fn validate_instance_uids(meta: &InstanceMeta) -> Result<(), StowError> {
    for (kind, uid) in [
        ("StudyInstanceUID", meta.study_instance_uid.as_str()),
        ("SeriesInstanceUID", meta.series_instance_uid.as_str()),
        ("SOPInstanceUID", meta.sop_instance_uid.as_str()),
    ] {
        if let Err(e) = crate::handlers::wado::validate_dicom_uid(uid) {
            return Err(StowError::plain(
                400,
                err_json("Invalid DICOM UID", format!("{kind} '{uid}': {e}")),
            ));
        }
    }
    Ok(())
}

/// Ingest one instance through the storage pipeline.
///
/// Stage A is unchanged and per-instance: hash, CAS write/rename, shadow
/// `dual_write`, and the Stored/Idempotent/Conflict decision. Accepted
/// (Inserted) instances no longer write the study manifest here - they are
/// pushed into `batch` for the P1 request-scoped group commit
/// ([`commit_manifests`] once per study at request end).
pub fn ingest_instance(
    state: &AppState,
    src: InstanceSource<'_>,
    modifier: &str,
    batch: &mut ManifestBatch,
    scope: Option<&str>,
) -> Result<IngestOutcome, StowError> {
    let InstanceSource::TempFile { path, size, blake3 } = src;
    ingest_tempfile(state, path, size, blake3, modifier, batch, scope)
}

// -- TempFile path (multipart part, pre-streamed by wrapper) -------------

fn ingest_tempfile(
    state: &AppState,
    temp_path: &Path,
    size: u64,
    folded_blake3: [u8; 32],
    modifier: &str,
    batch: &mut ManifestBatch,
    scope: Option<&str>,
) -> Result<IngestOutcome, StowError> {
    let header =
        read_dicom_header(temp_path, state.config.max_part_header as usize).map_err(|e| {
            metrics_parse_error();
            crate::clog_warn!(error = %e, "STOW 400: Invalid DICOM (header)");
            bad_request(format!("Invalid DICOM: {e}"))
        })?;

    let mut meta = DicomRsParser.extract_metadata(&header).map_err(|e| {
        metrics_parse_error();
        crate::clog_warn!(error = %e, "STOW 400: Invalid DICOM (metadata)");
        bad_request(format!("Invalid DICOM: {e}"))
    })?;

    let coerce = validate_and_coerce(&mut meta).map_err(bad_request)?;

    // UID grammar gate BEFORE any CAS write (unaddressable objects
    // must never enter the archive).
    validate_instance_uids(&meta)?;

    // Study-scoped target: same rule as the bytes path (PS3.18 10.5.3-1).
    // A multipart part whose Study Instance UID differs from the URL study is
    // a request-level conflict, so it short-circuits the whole request.
    if let Some(scope) = scope
        && meta.study_instance_uid.as_str() != scope
    {
        return Err(StowError::plain(
            409,
            err_json(
                "Conflict",
                format!(
                    "Study Instance UID mismatch: instance {} does not match target {scope}",
                    meta.study_instance_uid
                ),
            ),
        ));
    }

    // resurrection-guard: 409 on a tombstoned study (before CAS).
    reject_tombstoned(state, &meta.study_instance_uid)?;

    // IOCM re-acceptance refusal, per part (see the helper for the scope).
    if let Some(outcome) = refuse_if_hidden_from_store(
        state,
        &meta.study_instance_uid,
        &meta.sop_instance_uid,
        meta.sop_class_uid.as_deref().unwrap_or(""),
    ) {
        return Ok(IngestOutcome {
            outcome,
            meta,
            coerced: false,
            generated: 0,
            hidden_phi_excised: false,
        });
    }

    let sop_instance_uid = meta.sop_instance_uid.clone();
    let sop_class_uid = meta.sop_class_uid.clone();

    // 07.09 field: strip client-framing CRLF junk from the merged part file
    // before it is hashed/renamed into CAS (Weasis DICOMweb STOW framing).
    let mut file_size = size;
    strip_trailing_crlf_file(temp_path, &mut file_size).map_err(|e| {
        let _ = std::fs::remove_file(temp_path);
        stow_error_with_sop(
            500,
            format!("Failed to normalize instance for CAS: {e}"),
            sop_class_uid.as_deref(),
            &sop_instance_uid,
        )
    })?;

    // BLAKE3 content address = the digest of the FINAL on-disk CAS bytes.
    // The wrapper already folded this hash over the bytes AS WRITTEN
    // (`CasWrite`), so in the common case (a conformant DICOM never ends in
    // CRLF) the folded digest IS the final digest and we skip a second full
    // read of the (possibly 13 GB) file. ONLY when `strip_trailing_crlf_file`
    // actually truncated (the rare Weasis CRLF-framed part) do the written
    // bytes differ from the stored bytes, and we recompute over the truncated
    // file to keep digest == bytes. SHA-256 stays the manifest key.
    // FR-001b: write the generated patient identity INTO the blob, before
    // PixelData, and BEFORE the content hash and the insert decision. The
    // order matters twice over: the CAS key must describe the bytes actually
    // served, and a repeated upload of the SAME coerced file must hash to the
    // same key it did the first time - if the splice ran after the decision,
    // the second upload would be compared against a stored blob that carries
    // the elements and be misfiled as an update.
    //
    // SKIPPED when the hidden-PHI sanitizer will run: that path excises the
    // original (0400,0550)/(0400,0561) ranges BY OFFSET into the incoming
    // bytes, so shifting them here would excise the wrong span. It splices
    // instead, on the sanitized buffer, where the offsets are already resolved.
    let will_sanitize = sanitize_hidden_phi(&meta, &coerce);
    if !will_sanitize && embeddable_transfer_syntax(&meta) {
        let patient_bytes = coerced_patient_elements(&coerce, &meta);
        if !patient_bytes.is_empty() {
            splice_patient_elements_in_file(temp_path, &patient_bytes, &meta, &sop_instance_uid)?;
        }
    }

    // The caller's folded digest was computed over the bytes AS RECEIVED, so
    // any splice or truncation invalidates it: re-hash the final file. The
    // common case (a conformant, non-coerced instance) still reuses the folded
    // digest and never re-reads the blob.
    let blake3 = if file_size != size || !coerce.generated.is_empty() {
        blake3_of_file(temp_path).map_err(|e| {
            let _ = std::fs::remove_file(temp_path);
            stow_error_with_sop(
                500,
                format!("Failed to hash instance for CAS: {e}"),
                sop_class_uid.as_deref(),
                &sop_instance_uid,
            )
        })?
    } else {
        folded_blake3
    };
    let file_size = std::fs::metadata(temp_path)
        .map(|m| m.len())
        .unwrap_or(file_size);

    // 2808 update-in-place: decide BEFORE any CAS materialization, exactly
    // like the bytes path (one blob per outcome, no orphan-on-idempotent).
    let insert_result = decide_insert_result(state, &meta, &blake3);

    let outcome = map_insert_tempfile(
        state,
        insert_result,
        &meta,
        &blake3,
        temp_path,
        file_size,
        modifier,
        &coerce,
        batch,
    )?;

    let generated = coerce.generated.len();
    let hidden_phi_excised = sanitize_hidden_phi(&meta, &coerce);
    Ok(IngestOutcome {
        outcome,
        meta,
        coerced: coerce.any,
        generated,
        hidden_phi_excised,
    })
}

// -- sop_owner + study-manifest PatientID ------

/// Decide the STOW insertion outcome from the B-tree `sop_owner` index plus
/// the study-manifest PatientID and a byte comparison against
/// the stored blob (2808 update-in-place):
/// - SOP never seen -> Inserted.
/// - SOP seen under a study whose manifest FILE IS GONE (01.09 erase gap:
///   the append-only sop_owner rows are NOT invalidated by admin delete)
///   -> Inserted: the owner study was deleted, the row is stale, and the
///   re-STOW must rebuild the study fresh.
/// - SOP seen under a study whose manifest is READABLE but does NOT contain
///   the SOP -> Inserted: the row is stale for a live study, the instance
///   merges into the existing study.
/// - SOP seen under SAME study + same PatientID -> Idempotent iff the
///   incoming bytes equal the stored blob's BLAKE3; else Updated.
/// - SOP seen under SAME study + different PatientID -> Conflict.
/// - SOP seen under DIFFERENT study -> Conflict (`existing_patient_id` =
///   that study's PatientID).
/// - SOP seen under a study whose manifest EXISTS but is unreadable ->
///   Idempotent no-op (today's exact semantics for a degraded study - a
///   full-write would clobber the unreadable manifest with only the
///   incoming instances).
///
/// An unreadable manifest degrades to Idempotent (today's exact semantics
/// for a degraded study - the instance is already unaddressable by WADO).
///
/// Pub for the ap101 gate. NOTE: the Inserted arm constructs an
/// owned `InsertResult` (5 String clones) - that part is BUDGETED (owned
/// enum by design); the gated window covers the warm decision lookup and
/// asserts the arm's exact 5-allocation budget.
pub fn decide_insert_result(
    state: &AppState,
    meta: &InstanceMeta,
    incoming_blake3: &[u8; 32],
) -> InsertResult {
    let sop = &meta.sop_instance_uid;
    let study = &meta.study_instance_uid;

    match state.shadow.sop_owner_study(sop) {
        None => InsertResult::Inserted {
            instance_id: 0,
            sop_class_uid: meta.sop_class_uid.clone().unwrap_or_default(),
            sop_instance_uid: sop.clone(),
            study_instance_uid: study.clone(),
            series_instance_uid: meta.series_instance_uid.clone(),
            patient_id: meta.patient_id.clone(),
        },
        Some(owner_study) => {
            decide_known_sop(state, meta, incoming_blake3, sop, study, &owner_study)
        }
    }
}

/// The known-SOP decision (01.09 rewrite: ONE manifest read instead of the
/// old two - `manifest_patient` + `stored_instance`).
///
/// `sop_owner` rows are append-only and admin delete does not invalidate
/// them, so a stale row is possible in two shapes (both -> Inserted):
/// - the OWNER study's manifest FILE IS GONE: the study was deleted, the
///   re-STOW rebuilds it fresh;
/// - the manifest is READABLE but does NOT contain the SOP: the instance
///   was deleted from a live study, it merges back in.
///
/// A manifest that EXISTS but is unreadable keeps today's degraded no-op
/// (Idempotent): a full build here would clobber the unreadable manifest
/// with only the incoming instances.
fn decide_known_sop(
    state: &AppState,
    meta: &InstanceMeta,
    incoming_blake3: &[u8; 32],
    sop: &str,
    study: &str,
    owner_study: &str,
) -> InsertResult {
    let inserted = || InsertResult::Inserted {
        instance_id: 0,
        sop_class_uid: meta.sop_class_uid.clone().unwrap_or_default(),
        sop_instance_uid: sop.to_string(),
        study_instance_uid: study.to_string(),
        series_instance_uid: meta.series_instance_uid.clone(),
        patient_id: meta.patient_id.clone(),
    };
    let idempotent = || InsertResult::Idempotent {
        sop_class_uid: meta.sop_class_uid.clone().unwrap_or_default(),
        sop_instance_uid: sop.to_string(),
        study_instance_uid: study.to_string(),
        series_instance_uid: meta.series_instance_uid.clone(),
        patient_id: meta.patient_id.clone(),
    };
    let conflict = |existing_patient_id: String| {
        // The wire carries only the FACT of the conflict (see
        // build_conflict_failed_sop_entry); the operator's log carries WHOSE
        // it was, so a sender colliding on a SOP UID cannot read another
        // patient's identifier while the operator can still diagnose it.
        crate::clog_warn!(
            sop_instance_uid = sop,
            study_uid = study,
            "STOW conflict: SOP {} (incoming patient {}) already stored under patient {}",
            sop,
            meta.patient_id,
            existing_patient_id
        );
        InsertResult::Conflict {
            sop_class_uid: meta.sop_class_uid.clone().unwrap_or_default(),
            sop_instance_uid: sop.to_string(),
            existing_patient_id,
            conflicting_patient_id: meta.patient_id.clone(),
        }
    };

    let dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
    let path = clarus_core::stow::manifest::manifest_path(&dir, owner_study);
    if !path.exists() {
        // 01.09 erase gap: deleted study, stale append-only row.
        return inserted();
    }
    let m = match clarus_core::manifest::read_manifest(&path) {
        Ok(m) => m,
        Err(_) => {
            // Exists but unreadable: degraded study - preserve the no-op.
            crate::clog_warn!(
                sop_instance_uid = sop,
                study_uid = study,
                "study manifest unreadable during STOW decision - degraded to idempotent no-op"
            );
            return idempotent();
        }
    };

    let owner_patient = m
        .study
        .attrs
        .iter()
        .find(|(t, _)| *t == clarus_core::query::tag::PATIENT_ID)
        .and_then(|(_, v)| match v {
            clarus_core::manifest::AttrValue::Text(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();

    // Membership decides FIRST. `sop_owner` is append-only, so a SOP that was
    // deleted and re-sent still has its old row pointing at the old study. The
    // ordinary correction flow - delete an instance stored under the wrong
    // patient, re-send it fixed - would then be refused forever by an ownership
    // check made before the archive noticed the instance is no longer there.
    let rec = match m.find_instance(sop) {
        // 01.09 erase gap: readable manifest, SOP absent - stale row for a
        // live study, the instance merges back in.
        None => return inserted(),
        Some(rec) => rec,
    };

    if owner_study != study {
        return conflict(owner_patient);
    }
    if owner_patient != meta.patient_id {
        return conflict(owner_patient);
    }
    if rec.blake3 == *incoming_blake3 {
        idempotent()
    } else {
        {
            let mut old_attrs = Vec::with_capacity(4);
            for (t, v) in &m.study.attrs {
                if UPDATE_ATTR_TAGS.contains(t) {
                    let value = match v {
                        clarus_core::manifest::AttrValue::Text(s) => s.clone(),
                        clarus_core::manifest::AttrValue::UInt(n) => n.to_string(),
                        _ => String::new(),
                    };
                    let tag = *t;
                    old_attrs.push(((tag >> 16) as u16, (tag & 0xFFFF) as u16, value));
                }
            }
            InsertResult::Updated {
                sop_class_uid: meta.sop_class_uid.clone().unwrap_or_default(),
                sop_instance_uid: sop.to_string(),
                study_instance_uid: study.to_string(),
                series_instance_uid: meta.series_instance_uid.clone(),
                patient_id: meta.patient_id.clone(),
                old_attrs,
                old_blake3: rec.blake3,
                old_tier: rec.storage_tier,
            }
        }
    }
}

/// The four `StudyManifestAttrs` tags that feed (0400,0561) CORRECT items
/// (2808: NOT instance-level capture tags; pixels are never diffed).
const UPDATE_ATTR_TAGS: [u32; 4] = [
    clarus_core::query::tag::PATIENT_NAME,
    clarus_core::query::tag::PATIENT_ID,
    clarus_core::query::tag::PATIENT_BIRTH_DATE,
    clarus_core::query::tag::PATIENT_SEX,
];

/// Render one (0400,0561) CORRECT item when the incoming value differs from
/// the archived one; returns `None` when unchanged.
fn correct_item(
    old_attrs: &[(u16, u16, String)],
    group: u16,
    element: u16,
    incoming: &str,
) -> Option<crate::validation::OriginalAttributeItem> {
    let old = old_attrs
        .iter()
        .find(|(g, e, _)| *g == group && *e == element)
        .map(|(_, _, v)| v.as_str())
        .unwrap_or("");
    if old == incoming {
        None
    } else {
        Some(crate::validation::OriginalAttributeItem {
            group,
            element,
            old_value: old.to_string(),
            reason: "CORRECT",
        })
    }
}

// -- InsertResult mapping (bytes + tempfile share the update core) --------

/// CAS target path from a 64-hex key - the canonical two-level geometry
/// rendered into a stack buffer via [`cas_rel_path_fill_buf`] + one `PathBuf`
/// join (1 allocation per instance instead of the old
/// `cas_path` String + format chain).
fn cas_target_of(cas: &ContentAddressedStorage, cas_key: &str) -> PathBuf {
    let mut rel_buf = [0u8; 128];
    let rel = clarus_adapters::storage::cas_rel_path_fill_buf(cas_key, &mut rel_buf)
        // safe: cas_key is a 64-hex blake3 render; fill_buf holds up to 119.
        .expect("64-hex CAS key always fits the 128-byte stack buffer");
    cas.root_dir().join(rel)
}

/// CAS dedup write of fully-materialized bytes (2808: the ONLY write for an
/// outcome arm - Inserted writes the original, Updated writes the canonical
/// embedded blob, Idempotent self-heals a manifest-referenced but lost blob).
///
/// 0710: this used to `std::fs::write` STRAIGHT to the final CAS path with no
/// sync. A crash mid-write left a truncated blob AT the content address, and
/// the `exists()` dedup above then treated it as "already stored" on every
/// retry - permanent corruption with no self-heal. The fix is atomic
/// tmp-sibling + rename (the final path is either the whole blob or absent)
/// PLUS a `sync_all` barrier before the rename (durable before the manifest
/// that references it). A write or sync failure drops the temp and FAILS the
/// instance (per-SOP `StowError`): the blob is the unrecoverable pixel data.
fn write_cas_dedup(
    state: &AppState,
    blake3: &[u8; 32],
    bytes: &[u8],
    sop_class_uid: Option<&str>,
    sop_instance_uid: &str,
) -> Result<(), StowError> {
    let cas_key = clarus_adapters::hex_encode(blake3);
    let cas_target = cas_target_of(&state.cas, &cas_key);
    if cas_target.exists() {
        return Ok(());
    }
    let bucket_dir = cas_target
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| state.cas.root_dir().to_path_buf());
    if let Err(e) = std::fs::create_dir_all(&bucket_dir) {
        return Err(internal_error(format!("Failed to create CAS bucket: {e}")));
    }
    // Write to a TEMP SIBLING in the SAME bucket dir: a same-directory rename
    // is atomic, so the final content address is never a partial file. The
    // name is unique per (pid, sequence) so two in-flight writers cannot
    // collide (a clock is NOT a uniqueness primitive - a monotonic counter is).
    let temp_path = bucket_dir.join(temp_sibling_name(&cas_key));
    let written = std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .and_then(|mut f| {
            use std::io::Write as _;
            f.write_all(bytes)?;
            // DURABILITY BARRIER on the OPEN fd (no re-open): flush the bytes
            // before the rename publishes them under the content address.
            f.sync_all()
        });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temp_path);
        return Err(cas_write_error(&e, sop_class_uid, sop_instance_uid));
    }
    if let Err(e) = std::fs::rename(&temp_path, &cas_target) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(cas_write_error(&e, sop_class_uid, sop_instance_uid));
    }
    // Content is durable; make the directory entry durable too (best-effort,
    // Unix only - see the helper for the Windows boundary).
    sync_cas_bucket_dir(&cas_target);
    // 0210: CAS write throughput. Counted only for a NEW blob (the dedup
    // early-return above means an already-stored object wrote nothing), so the
    // counter tracks bytes that actually hit the disk. Prometheus `rate()`
    // turns it into MB/s.
    crate::metrics::CAS_WRITE_BYTES
        .fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// Unique temp-sibling filename for a CAS blob (`.<key>.<pid>-<seq>.tmp`),
/// hidden by the leading dot and impossible to mistake for a content address.
fn temp_sibling_name(cas_key: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!(".{cas_key}.{}-{seq}.tmp", std::process::id())
}

/// Map a CAS write/rename I/O error to the right per-SOP `StowError`: ENOSPC
/// keeps the 507 "disk full" shape (retryable), everything else is a plain 500
/// carrying the SOP identity so the instance lands in FailedSOPSequence.
fn cas_write_error(
    e: &std::io::Error,
    sop_class_uid: Option<&str>,
    sop_instance_uid: &str,
) -> StowError {
    if is_enospc(e) {
        // RFC 9110 15.6.4: 507 is a TEMPORARY server condition and the
        // response SHOULD include Retry-After. A full disk is not fixed by the
        // client retrying sooner, but the header is what tells a sending SCU
        // that the failure is ours and transient - without it a PACS marks the
        // instance permanently failed and stops resending a study that would
        // succeed a minute later.
        StowError::with_retry(
            507,
            err_json("Insufficient Storage", "CAS write error: disk full"),
            60,
        )
    } else {
        stow_error_with_sop(
            500,
            format!("CAS write error: {e}"),
            sop_class_uid,
            sop_instance_uid,
        )
    }
}

/// 3008: append one `hex tier` line to a per-study reclaim sidecar
/// (`{study}.orphans` / `{study}.audited`). DEDUPES on append: the existing
/// file (small) is read and the write is skipped when the line is already
/// present - PRIMARY for `.audited` (it has no GC, so repeat uploads of a
/// sanitized file must not grow it unboundedly). Returns `Ok(true)` when the
/// line was actually appended; errors are the caller's WARN, never
/// client-visible.
pub(crate) fn append_sidecar_line(
    manifest_dir: &Path,
    study_uid: &str,
    suffix: &str,
    hex: &str,
    tier: u8,
) -> std::io::Result<bool> {
    let line = format!("{hex} {tier}\n");
    let sidecar = manifest_dir.join(format!("{study_uid}{suffix}"));
    if let Ok(text) = std::fs::read_to_string(&sidecar)
        && text.lines().any(|l| l.trim() == format!("{hex} {tier}"))
    {
        return Ok(false); // already recorded - dedup, no write
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sidecar)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(line.as_bytes())
        })?;
    Ok(true)
}

/// 3008 hidden-PHI sanitization: excise the UNION of the captured
/// (0400,0550)/(0400,0561) top-level byte ranges from `src`. Sorts the
/// ranges, merges overlaps AND adjacency (each shape covered exactly once,
/// never excised twice), then one deterministic byte copy skipping them - no
/// re-encode; bytes outside the spans stay byte-identical to the source.
pub(crate) fn excise_original_attrs(src: &[u8], ranges: &[(usize, usize)]) -> Vec<u8> {
    if ranges.is_empty() {
        return src.to_vec();
    }
    let mut sorted: Vec<(usize, usize)> = ranges
        .iter()
        .map(|&(s, e)| (s.min(src.len()), e.min(src.len())))
        .filter(|&(s, e)| s < e)
        .collect();
    sorted.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(sorted.len());
    for (s, e) in sorted {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e), // overlap or adjacency
            _ => merged.push((s, e)),
        }
    }
    let mut out = Vec::with_capacity(src.len());
    let mut pos = 0;
    for (s, e) in merged {
        out.extend_from_slice(&src[pos..s]);
        pos = e;
    }
    out.extend_from_slice(&src[pos..]);
    out
}

/// 3008 hidden-PHI rule: sanitize ONLY on the rare path - the patient fields
/// were empty at parse (validate_and_coerce fired) AND the file's own
/// (0400,0550)/(0400,0561) sequences carry identifying values.
fn sanitize_hidden_phi(meta: &InstanceMeta, coerce: &CoerceReport) -> bool {
    coerce.any && meta.hidden_identifying_attrs
}

/// Can an Explicit VR LE element be written into this instance's header?
///
/// Explicit VR Little Endian uncompressed (1.2.840.10008.1.2.1) and every
/// ENCAPSULATED syntax (JPEG baseline/extended/lossless, JPEG-LS, JPEG 2000,
/// RLE, HTJ2K). The encapsulated ones keep all of their NON-pixel elements in
/// Explicit VR LE - only the pixel data is wrapped in items - so a header
/// attribute written that way is exactly what a reader expects. This is what
/// makes the Weasis/RLE flow work: an RLE instance with an empty PatientID can
/// still carry the generated one.
///
/// Implicit VR LE (1.2.840.10008.1.2) and deflated explicit VR
/// (1.2.840.10008.1.2.1.99) are refused. Under Implicit VR the VR pair this
/// writer emits would be read as part of the length, shifting every following
/// element; after a deflate stream the raw bytes are not part of the dataset at
/// all. Big/little endian explicit (1.2.840.10008.1.2.2) is refused for the
/// same length problem. An absent syntax means the parser found none, and
/// guessing is worse than not embedding.
fn embeddable_transfer_syntax(meta: &InstanceMeta) -> bool {
    let Some(ts) = meta.transfer_syntax_uid.as_deref() else {
        return false;
    };
    matches!(
        ts,
        // Explicit VR Little Endian, uncompressed.
        "1.2.840.10008.1.2.1"
            // Explicit VR Big Endian is NOT listed: every length would need
            // byte-swapping, which this writer does not do.
            | "1.2.840.10008.1.2.4.50" // JPEG Baseline
            | "1.2.840.10008.1.2.4.51" // JPEG Extended
            | "1.2.840.10008.1.2.4.57" // JPEG Lossless (14)
            | "1.2.840.10008.1.2.4.70" // JPEG Lossless (14, SV1)
            | "1.2.840.10008.1.2.4.80" // JPEG-LS Lossless
            | "1.2.840.10008.1.2.4.81" // JPEG-LS Near-Lossless
            | "1.2.840.10008.1.2.4.90" // JPEG 2000 Lossless
            | "1.2.840.10008.1.2.4.91" // JPEG 2000
            | "1.2.840.10008.1.2.4.92" // JPEG 2000 Part 2 Multi-component Lossless
            | "1.2.840.10008.1.2.4.93" // JPEG 2000 Part 2 Multi-component
            | "1.2.840.10008.1.2.5" // RLE Lossless
            | "1.2.840.10008.1.2.4.201" // HTJ2K Lossless
            | "1.2.840.10008.1.2.4.202" // HTJ2K Lossless RPCL
            | "1.2.840.10008.1.2.4.203" // HTJ2K
    )
}

/// The generated-patient elements this instance must carry, or empty when the
/// coercion generated no patient identity.
///
/// FR-001 fills an empty PatientID/PatientName in the MANIFEST. Writing the
/// value ONLY there left the stored blob without `(0010,0020)`/`(0010,0010)`,
/// so QIDO found the instance and WADO-RS `/metadata` - which reads the blob -
/// did not: the bridge (FR-001a) writes the generated value into the dataset
/// AND records the COERCE audit, and this server had lost the first half.
fn coerced_patient_elements(coerce: &CoerceReport, meta: &InstanceMeta) -> Vec<u8> {
    let patient_coerced = coerce
        .generated
        .iter()
        .any(|&(g, e)| (g, e) == (0x0010, 0x0010) || (g, e) == (0x0010, 0x0020));
    if !patient_coerced {
        return Vec::new();
    }
    crate::validation::build_coerced_patient_elements(&meta.patient_name, &meta.patient_id)
}

/// Splice `elements` into the dataset header, immediately before PixelData.
///
/// Position comes from the parse walk (`InstanceMeta::pixel_data_offset`), never
/// from a byte search: pixel data whose bytes happen to spell the tag must not
/// move the seam. A file with no PixelData has no header worth splicing (the
/// attributes would land after the last element of an empty dataset), so the
/// bytes are returned unchanged.
///
/// The transfer syntax rule is the SAME one the audit embed follows
/// ([`embeddable_transfer_syntax`], explicit VR only): under Implicit VR LE the
/// VR pair this writes would be read as part of the length, and Big Endian
/// byte-swaps every length, so either insertion corrupts the dataset. Those
/// instances keep the manifest-only coercion (audit-log change), which is what
/// they had before.
fn splice_before_pixel_data(src: &[u8], elements: &[u8], offset: Option<u64>) -> Vec<u8> {
    if elements.is_empty() {
        return src.to_vec();
    }
    let Some(at) = offset.map(|o| o as usize).filter(|&o| o <= src.len()) else {
        return src.to_vec();
    };
    let mut out = Vec::with_capacity(src.len() + elements.len());
    out.extend_from_slice(&src[..at]);
    out.extend_from_slice(elements);
    out.extend_from_slice(&src[at..]);
    out
}

/// Removes a scratch file when it goes out of scope.///
/// The spill file holds the pixel-data tail of a possibly multi-gigabyte
/// instance. Every early return in the splice would otherwise leak it, and a
/// STOW that fails to embed would quietly fill the volume it was trying to
/// write to.
struct RemoveOnDrop<'a>(&'a Path);

impl Drop for RemoveOnDrop<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

/// Splice `elements` in front of PixelData in the file at `temp_path`.
///
/// Rewrites the temp file in place (write the prefix, the elements, then the
/// rest streamed through a fixed buffer) so a multi-gigabyte instance is never
/// held in memory: only the header prefix and a 64 KiB window are ever resident.
fn splice_patient_elements_in_file(
    temp_path: &Path,
    elements: &[u8],
    meta: &InstanceMeta,
    sop_instance_uid: &str,
) -> Result<(), StowError> {
    use std::io::{Read, Seek, SeekFrom, Write};

    // The metadata parse reads only `max_part_header` bytes, so a header larger
    // than that window reports no PixelData offset. Re-parse a wider window
    // rather than give up: the coercion is a rare path, and losing the patient
    // identity from the blob is the bug being fixed.
    let at: u64 = match meta.pixel_data_offset {
        Some(at) => at,
        None => match pixel_data_offset_from_file(temp_path) {
            Some(at) => at,
            None => {
                crate::clog_warn!(
                    sop_instance_uid,
                    "coerced patient elements not written: no PixelData found in the file"
                );
                return Ok(());
            }
        },
    };
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(temp_path)
        .map_err(|e| {
            stow_error_with_sop(
                500,
                format!("Failed to reopen the part for the patient embed: {e}"),
                None,
                sop_instance_uid,
            )
        })?;
    let len = file.metadata().map(|m| m.len()).map_err(|e| {
        stow_error_with_sop(
            500,
            format!("Failed to stat the part for the patient embed: {e}"),
            None,
            sop_instance_uid,
        )
    })?;
    if at > len {
        return Ok(());
    }
    // Move the pixel-data tail aside into a scratch file, then rewrite the
    // original as header + elements + tail. The scratch file keeps the peak
    // memory at one buffer regardless of instance size. It lives beside the
    // part (same volume, so no cross-device copy) and is removed on every
    // exit path by the guard below.
    let spill_path = temp_path.with_extension("pixspill");
    // Read+write, not `File::create`: the spill is written with the tail and
    // then READ BACK to restore it, and a write-only handle cannot be read.
    let mut tail = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&spill_path)
        .map_err(|e| {
            stow_error_with_sop(
                500,
                format!("Failed to open the spill file for the patient embed: {e}"),
                None,
                sop_instance_uid,
            )
        })?;
    let _spill_guard = RemoveOnDrop(&spill_path);
    file.seek(SeekFrom::Start(at)).map_err(|e| {
        stow_error_with_sop(
            500,
            format!("Failed to seek the pixel data: {e}"),
            None,
            sop_instance_uid,
        )
    })?;
    std::io::copy(&mut (&mut file).take(len - at), &mut tail).map_err(|e| {
        stow_error_with_sop(
            500,
            format!("Failed to spill the pixel data: {e}"),
            None,
            sop_instance_uid,
        )
    })?;
    tail.seek(SeekFrom::Start(0)).map_err(|e| {
        stow_error_with_sop(
            500,
            format!("Failed to rewind the spill file: {e}"),
            None,
            sop_instance_uid,
        )
    })?;
    // Truncate at the seam and write elements + tail.
    file.set_len(at).map_err(|e| {
        stow_error_with_sop(
            500,
            format!("Failed to truncate at the pixel data: {e}"),
            None,
            sop_instance_uid,
        )
    })?;
    file.seek(SeekFrom::Start(at)).map_err(|e| {
        stow_error_with_sop(
            500,
            format!("Failed to seek the insertion point: {e}"),
            None,
            sop_instance_uid,
        )
    })?;
    file.write_all(elements).map_err(|e| {
        stow_error_with_sop(
            500,
            format!("Failed to write the patient elements: {e}"),
            None,
            sop_instance_uid,
        )
    })?;
    std::io::copy(&mut tail, &mut file).map_err(|e| {
        stow_error_with_sop(
            500,
            format!("Failed to restore the pixel data: {e}"),
            None,
            sop_instance_uid,
        )
    })?;
    file.flush().map_err(|e| {
        stow_error_with_sop(
            500,
            format!("Failed to flush the patient embed: {e}"),
            None,
            sop_instance_uid,
        )
    })?;
    Ok(())
}

/// The common update path: derive everything from the TEMP FILE on disk.///
/// A corrected instance is a re-sent whole object - for a whole-slide study
/// that is gigabytes. Reading it back into memory to hash it, append the audit
/// embed and scan for frames allocated the entire instance for the duration of
/// the write; when hidden-PHI excision ran it allocated a second copy. The temp
/// file already holds the incoming bytes, so this variant:
///
/// 1. appends the (0400,0561) embed to that file (when the transfer syntax can
///    carry it),
/// 2. hashes it with `blake3_of_file`,
/// 3. builds the frame table with `frame_records_from_file_capped` (header
///    streaming, not a read-back), and
/// 4. renames it into CAS under the new key.
///
/// The hidden-PHI case still needs the original bytes twice (the alibi blob and
/// the sanitized blob), so it delegates to [`materialize_update`] with the file
/// read into memory - the documented exception rather than the common path.
#[allow(clippy::too_many_arguments)]
fn materialize_update_from_file(
    state: &AppState,
    meta: &InstanceMeta,
    temp_path: &Path,
    old_attrs: &[(u16, u16, String)],
    coerce: &CoerceReport,
    modifier: &str,
    sop_class_uid: String,
    sop_instance_uid: String,
    study_instance_uid: String,
    series_instance_uid: String,
    patient_id: String,
    old_blake3: [u8; 32],
    old_tier: u8,
) -> Result<InstanceOutcome, StowError> {
    // Hidden-PHI excision rewrites the bytes, so it needs them in memory (and
    // keeps the original as a second alibi blob). Hand off to the shared core.
    if sanitize_hidden_phi(meta, coerce) {
        let original = std::fs::read(temp_path).map_err(|e| {
            let _ = std::fs::remove_file(temp_path);
            stow_error_with_sop(
                500,
                format!("Failed to read part for update: {e}"),
                Some(sop_class_uid.as_str()),
                &sop_instance_uid,
            )
        })?;
        let _ = std::fs::remove_file(temp_path);
        return materialize_update(
            state,
            meta,
            original,
            old_attrs,
            coerce,
            modifier,
            sop_class_uid,
            sop_instance_uid,
            study_instance_uid,
            series_instance_uid,
            patient_id,
            old_blake3,
            old_tier,
        );
    }

    // CORRECT items: one per changed StudyManifestAttrs tag (fixed order), then
    // COERCE items for fields generated during THIS ingest (old value = "").
    let mut items: Vec<crate::validation::OriginalAttributeItem> = Vec::new();
    if let Some(item) = correct_item(old_attrs, 0x0010, 0x0010, &meta.patient_name) {
        items.push(item);
    }
    if let Some(item) = correct_item(old_attrs, 0x0010, 0x0020, &meta.patient_id) {
        items.push(item);
    }
    if let Some(item) = correct_item(
        old_attrs,
        0x0010,
        0x0030,
        meta.patient_birth_date.as_deref().unwrap_or(""),
    ) {
        items.push(item);
    }
    if let Some(item) = correct_item(
        old_attrs,
        0x0010,
        0x0040,
        meta.patient_sex.as_deref().unwrap_or(""),
    ) {
        items.push(item);
    }
    for &(g, e) in &coerce.generated {
        items.push(crate::validation::OriginalAttributeItem {
            group: g,
            element: e,
            old_value: String::new(),
            reason: "COERCE",
        });
    }

    let changed_attrs = items.len();
    // The embed is Explicit VR Little Endian by construction, so appending it
    // to a dataset in another transfer syntax would write bytes that dataset
    // cannot describe - the change then lives in the audit log only and the
    // blob keeps its original bytes. Same rule as the in-memory core.
    let can_embed = changed_attrs > 0 && embeddable_transfer_syntax(meta);
    if changed_attrs > 0 && !can_embed {
        crate::clog_warn!(
            sop_instance_uid = %sop_instance_uid,
            study_uid = %study_instance_uid,
            transfer_syntax = meta.transfer_syntax_uid.as_deref().unwrap_or(""),
            changed_attrs = changed_attrs,
            "attribute change recorded in the audit log only: the instance's \
             transfer syntax cannot carry an explicit-VR attribute sequence"
        );
    }
    if can_embed {
        let audit_bytes = crate::validation::build_original_attributes_sequence(
            &items,
            modifier,
            &state.config.resolved_modifying_system(),
        );
        // FR-001b: the generated PatientID/PatientName must reach the BLOB, not
        // only the manifest - QIDO read the manifest and WADO-RS /metadata read
        // the blob, so the two disagreed. They go BEFORE PixelData (the metadata
        // walk stops there); the audit embed stays on the tail.
        let patient_bytes = coerced_patient_elements(coerce, meta);
        if !patient_bytes.is_empty() {
            splice_patient_elements_in_file(temp_path, &patient_bytes, meta, &sop_instance_uid)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(temp_path)
            .map_err(|e| {
                let _ = std::fs::remove_file(temp_path);
                stow_error_with_sop(
                    500,
                    format!("Failed to open part for the attribute embed: {e}"),
                    Some(sop_class_uid.as_str()),
                    &sop_instance_uid,
                )
            })?;
        if let Err(e) = std::io::Write::write_all(&mut file, &audit_bytes) {
            let _ = std::fs::remove_file(temp_path);
            return Err(stow_error_with_sop(
                500,
                format!("Failed to append the attribute embed: {e}"),
                Some(sop_class_uid.as_str()),
                &sop_instance_uid,
            ));
        }
    }

    let new_blake3 = blake3_of_file(temp_path).map_err(|e| {
        let _ = std::fs::remove_file(temp_path);
        stow_error_with_sop(
            500,
            format!("Failed to hash the updated part: {e}"),
            Some(sop_class_uid.as_str()),
            &sop_instance_uid,
        )
    })?;
    let file_size = std::fs::metadata(temp_path).map(|m| m.len()).map_err(|e| {
        let _ = std::fs::remove_file(temp_path);
        stow_error_with_sop(
            500,
            format!("Failed to stat the updated part: {e}"),
            Some(sop_class_uid.as_str()),
            &sop_instance_uid,
        )
    })?;
    // F5 (0310): the frame cap applies to EVERY accepted blob. The cap is
    // enforced BEFORE the CAS rename so an over-cap update leaves no residue.
    let frame_table =
        frame_records_from_file_capped(temp_path, state.config.max_frames_per_instance)
            .map_err(|e| too_many_frames_error(&sop_class_uid, &sop_instance_uid, e))?;
    rename_temp_to_cas(
        state,
        temp_path,
        &new_blake3,
        &sop_class_uid,
        &sop_instance_uid,
    )?;

    commit_updated_manifest(
        state,
        meta,
        new_blake3,
        file_size,
        frame_table,
        old_blake3,
        old_tier,
        sop_class_uid,
        sop_instance_uid,
        study_instance_uid,
        series_instance_uid,
        patient_id,
        changed_attrs,
    )
}

/// Re-point the manifest at a replacement blob after the bytes are durable.
///
/// Shared by both update paths so the manifest/attribute/orphan bookkeeping
/// cannot differ between them. On a manifest write failure the correction is
/// reported as a per-SOP failure and NO orphan is queued: the manifest still
/// serves the old blob, so reclaiming it would delete the bytes in use.
#[allow(clippy::too_many_arguments)]
fn commit_updated_manifest(
    state: &AppState,
    meta: &InstanceMeta,
    new_blake3: [u8; 32],
    file_size: u64,
    frame_table: Vec<clarus_core::manifest::FrameRecord>,
    old_blake3: [u8; 32],
    old_tier: u8,
    sop_class_uid: String,
    sop_instance_uid: String,
    study_instance_uid: String,
    series_instance_uid: String,
    patient_id: String,
    changed_attrs: usize,
) -> Result<InstanceOutcome, StowError> {
    let manifest_dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
    let mut attrs = study_attrs_from_meta(meta);
    let series = vec![instance_series_model(
        meta,
        new_blake3,
        file_size,
        frame_table,
    )];
    match clarus_core::stow::write_study_manifest_update(&manifest_dir, &mut attrs, &series) {
        Ok(_) => {
            state
                .study_scanner
                .on_study_committed(&study_instance_uid, &attrs);
        }
        Err(e) => {
            crate::clog_warn!(
                error = %e,
                sop_instance_uid = %sop_instance_uid,
                study_uid = %study_instance_uid,
                "study manifest update failed: the correction was not applied"
            );
            return Ok(InstanceOutcome::Failed {
                sop_class_uid: Some(sop_class_uid),
                sop_instance_uid: Some(sop_instance_uid),
                reason: format!(
                    "Study manifest update failed, so the replacement was not committed: {e}"
                ),
            });
        }
    }

    // Orphan accounting (P1): remember the REPLACED blob in a per-study
    // sidecar so the erase worker reclaims it when the study is erased. No
    // deletion on the hot path (the old blob stays in CAS until erase).
    let old_hex = clarus_adapters::hex_encode(&old_blake3);
    match append_sidecar_line(
        &manifest_dir,
        &study_instance_uid,
        ".orphans",
        &old_hex,
        old_tier,
    ) {
        Ok(true) => {
            crate::metrics::ORPHANS_PENDING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(false) => {}
        Err(e) => {
            crate::clog_warn!(
                error = %e,
                study_uid = %study_instance_uid,
                "orphan sidecar append failed (old blob stays until manual reclaim, client unaffected)"
            );
        }
    }

    crate::clog_info!(
        sop_instance_uid = %sop_instance_uid,
        changed_attrs = changed_attrs,
        "Instance updated (bytes replaced)"
    );

    Ok(InstanceOutcome::Updated {
        sop_class_uid,
        sop_instance_uid,
        study_instance_uid,
        series_instance_uid,
        patient_id,
        changed_attrs,
        bytes_replaced: true,
    })
}

/// The shared 2808 update core: materialize ONE canonical blob (original
/// bytes + (0400,0561) with COERCE items for generated fields and CORRECT
/// items for changed StudyManifestAttrs), re-point the manifest instance
/// record, re-stamp study attrs, refresh the RAM QIDO index, and return the
/// `Updated` outcome. `changed_attrs` counts the embedded items (0 = a
/// pixel-only replacement - pixels are never diffed).
#[allow(clippy::too_many_arguments)]
fn materialize_update(
    state: &AppState,
    meta: &InstanceMeta,
    mut original: Vec<u8>,
    old_attrs: &[(u16, u16, String)],
    coerce: &CoerceReport,
    modifier: &str,
    sop_class_uid: String,
    sop_instance_uid: String,
    study_instance_uid: String,
    series_instance_uid: String,
    patient_id: String,
    old_blake3: [u8; 32],
    old_tier: u8,
) -> Result<InstanceOutcome, StowError> {
    // CORRECT items: one per changed StudyManifestAttrs tag (fixed order).
    let mut items: Vec<crate::validation::OriginalAttributeItem> = Vec::new();
    if let Some(item) = correct_item(old_attrs, 0x0010, 0x0010, &meta.patient_name) {
        items.push(item);
    }
    if let Some(item) = correct_item(old_attrs, 0x0010, 0x0020, &meta.patient_id) {
        items.push(item);
    }
    if let Some(item) = correct_item(
        old_attrs,
        0x0010,
        0x0030,
        meta.patient_birth_date.as_deref().unwrap_or(""),
    ) {
        items.push(item);
    }
    if let Some(item) = correct_item(
        old_attrs,
        0x0010,
        0x0040,
        meta.patient_sex.as_deref().unwrap_or(""),
    ) {
        items.push(item);
    }
    // COERCE items for fields generated during THIS ingest (old value = "").
    for &(g, e) in &coerce.generated {
        items.push(crate::validation::OriginalAttributeItem {
            group: g,
            element: e,
            old_value: String::new(),
            reason: "COERCE",
        });
    }

    let changed_attrs = items.len();
    // 3008 hidden-PHI sanitization: excise the incoming (0400,0550)/(0400,0561)
    // union BEFORE appending our own 0561 embed.
    let hidden_phi = sanitize_hidden_phi(meta, coerce);
    // The part arrives BY OWNERSHIP: on the common path (no hidden PHI) it is
    // moved straight into the CAS write instead of being cloned first. A
    // re-sent whole-slide part is already gigabytes, and `to_vec()` doubled
    // the peak for the duration of the write.
    let mut owned = if hidden_phi {
        // The alibi path needs the ORIGINAL bytes as well, so here the copy is
        // unavoidable - but only here.
        excise_original_attrs(&original, &meta.original_attrs_ranges)
    } else {
        std::mem::take(&mut original)
    };
    // The embed below is encoded Explicit VR Little Endian by construction
    // (see `build_original_attributes_sequence`), so appending it to a dataset
    // in another transfer syntax writes bytes that dataset cannot describe:
    // under Implicit VR LE the VR pair is read as part of the length, after a
    // deflate stream the raw tail is not even part of the dataset, and under
    // Big Endian every length is byte-swapped. In those cases the change is
    // recorded in the audit log only and the blob keeps its original bytes -
    // an unupdatable instance is better than a corrupt one.
    let can_embed = changed_attrs > 0 && embeddable_transfer_syntax(meta);
    if changed_attrs > 0 && !can_embed {
        crate::clog_warn!(
            sop_instance_uid = %sop_instance_uid,
            study_uid = %study_instance_uid,
            transfer_syntax = meta.transfer_syntax_uid.as_deref().unwrap_or(""),
            changed_attrs = changed_attrs,
            "attribute change recorded in the audit log only: the instance's \
             transfer syntax cannot carry an explicit-VR attribute sequence"
        );
    }
    if can_embed {
        let audit_bytes = crate::validation::build_original_attributes_sequence(
            &items,
            modifier,
            &state.config.resolved_modifying_system(),
        );
        // FR-001b: splice the generated patient elements BEFORE PixelData, then
        // append the audit sequence. Doing it in this order keeps the patient
        // attributes inside the header the metadata reader walks, while the
        // (0400,0561) audit stays on the tail where it is already written.
        let patient_bytes = coerced_patient_elements(coerce, meta);
        owned = splice_before_pixel_data(&owned, &patient_bytes, meta.pixel_data_offset);
        owned.extend_from_slice(&audit_bytes);
    }
    let new_blake3 = crate::shadow::blake3_of(&owned);
    let file_size = owned.len() as u64;
    // F5 (0310): the frame cap applies to EVERY accepted blob, not only to the
    // Inserted arm. A client could otherwise bypass `max_frames_per_instance`
    // by storing a small instance and then re-sending the same SOP with an
    // over-cap number of frames: the update path wrote the new blob and built
    // the table with the UNCAPPED reader, so the reject never fired and the
    // instance landed with a table the serve path would then trust. The check
    // runs BEFORE `write_cas_dedup` so an over-cap update leaves no CAS
    // residue (the same rule the Inserted arm follows when it unlinks).
    let frame_table = match frame_records_capped(&owned, state.config.max_frames_per_instance) {
        Ok(f) => f,
        Err(e) => return Err(too_many_frames_error(&sop_class_uid, &sop_instance_uid, e)),
    };
    write_cas_dedup(
        state,
        &new_blake3,
        &owned,
        Some(sop_class_uid.as_str()),
        &sop_instance_uid,
    )?;

    let manifest_dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
    // 3008 hidden-PHI alibi: the ORIGINAL incoming bytes stay in CAS and get
    // a `{study}.audited` line (hidden-PHI proof); the manifest points at the
    // sanitized blob above.
    if hidden_phi {
        let original_blake3 = crate::shadow::blake3_of(&original);
        write_cas_dedup(state, &original_blake3, &original, None, &sop_instance_uid)?;
        let hex = clarus_adapters::hex_encode(&original_blake3);
        match append_sidecar_line(
            &manifest_dir,
            &study_instance_uid,
            ".audited",
            &hex,
            clarus_core::manifest::TIER_HOT,
        ) {
            Ok(true) => {
                crate::clog_info!(
                    sop_instance_uid = %sop_instance_uid,
                    "Hidden-PHI alibi recorded (original bytes kept, sanitized bytes served)"
                );
            }
            Ok(false) => {}
            Err(e) => {
                crate::clog_warn!(
                    error = %e,
                    sop_instance_uid = %sop_instance_uid,
                    study_uid = %study_instance_uid,
                    "audited sidecar append failed (alibi stays until manual reclaim, client unaffected)"
                );
            }
        }
    }

    // Shadow dual-write (B-001) - best-effort, never surfaces to client.
    if let Err(e) = state.shadow.dual_write(
        new_blake3,
        &meta.modality,
        &study_instance_uid,
        &sop_instance_uid,
    ) {
        crate::clog_warn!(
            error = %e,
            sop_instance_uid = %sop_instance_uid,
            "shadow dual-write failed (shadow tier degraded, client unaffected)"
        );
    }

    // Manifest replace-merge: the instance record re-points to the new blob;
    // study attrs are RE-STAMPED from the corrected incoming values. The
    // frame table is rebuilt from `owned` - the exact bytes just written to
    // CAS - so the offsets match the blob the serve path will open.
    commit_updated_manifest(
        state,
        meta,
        new_blake3,
        file_size,
        frame_table,
        old_blake3,
        old_tier,
        sop_class_uid,
        sop_instance_uid,
        study_instance_uid,
        series_instance_uid,
        patient_id,
        changed_attrs,
    )
}

/// Multipart mapping: the decision ran BEFORE the rename, so the rename /
/// drop decision is per-outcome. The update core is shared with the bytes
/// path (one extra read of the temp file - updates are rare/cold).
#[allow(clippy::too_many_arguments)]
fn map_insert_tempfile(
    state: &AppState,
    insert_result: InsertResult,
    meta: &InstanceMeta,
    blake3: &[u8; 32],
    temp_path: &Path,
    size: u64,
    modifier: &str,
    coerce: &CoerceReport,
    batch: &mut ManifestBatch,
) -> Result<InstanceOutcome, StowError> {
    match insert_result {
        InsertResult::Inserted {
            sop_class_uid: sc,
            sop_instance_uid: si,
            study_instance_uid,
            series_instance_uid,
            patient_id,
            ..
        } => {
            rename_temp_to_cas(state, temp_path, blake3, &sc, &si)?;
            // 3008 hidden-PHI sanitization (tempfile path): read the original
            // back from CAS, excise the union, materialize the sanitized blob
            // and record the original as the `{study}.audited` alibi.
            let (stored_blake3, stored_size, stored_bytes) = if sanitize_hidden_phi(meta, coerce) {
                let original = std::fs::read(cas_target_of(
                    &state.cas,
                    &clarus_adapters::hex_encode(blake3),
                ))
                .map_err(|e| {
                    stow_error_with_sop(500, format!("CAS read for sanitize: {e}"), Some(&sc), &si)
                })?;
                let sanitized = excise_original_attrs(&original, &meta.original_attrs_ranges);
                // FR-001b on the hidden-PHI path: the patient elements are
                // spliced HERE because the excision above resolved the ranges
                // against the incoming bytes; the offset is still the incoming
                // PixelData position, which the excision never moves (the
                // excised spans all precede it).
                let sanitized = if embeddable_transfer_syntax(meta) {
                    splice_before_pixel_data(
                        &sanitized,
                        &coerced_patient_elements(coerce, meta),
                        meta.pixel_data_offset,
                    )
                } else {
                    sanitized
                };
                let s_blake3 = crate::shadow::blake3_of(&sanitized);
                write_cas_dedup(state, &s_blake3, &sanitized, Some(&sc), &si)?;
                let manifest_dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
                let hex = clarus_adapters::hex_encode(blake3);
                match append_sidecar_line(
                    &manifest_dir,
                    &study_instance_uid,
                    ".audited",
                    &hex,
                    clarus_core::manifest::TIER_HOT,
                ) {
                    Ok(true) => {
                        crate::clog_info!(
                            sop_instance_uid = %si,
                            "Hidden-PHI alibi recorded (original bytes kept, sanitized bytes served)"
                        );
                    }
                    Ok(false) => {}
                    Err(e) => {
                        crate::clog_warn!(
                            error = %e,
                            sop_instance_uid = %si,
                            study_uid = %study_instance_uid,
                            "audited sidecar append failed (alibi stays until manual reclaim, client unaffected)"
                        );
                    }
                }
                (s_blake3, sanitized.len() as u64, Some(sanitized))
            } else {
                (*blake3, size, None)
            };
            if let Err(e) =
                state
                    .shadow
                    .dual_write(stored_blake3, &meta.modality, &study_instance_uid, &si)
            {
                crate::clog_warn!(
                    error = %e,
                    sop_instance_uid = %si,
                    "shadow dual-write failed (shadow tier degraded, client unaffected)"
                );
            }
            // Same rule as the bytes path: the table describes the blob that
            // is actually served, so it is built from the sanitized bytes when
            // hidden-PHI removal ran. Otherwise the CAS blob is scanned by
            // STREAMING its header (Read + Seek), not read back into memory:
            // for the 13.09 GB RMS base level the old `std::fs::read` read all
            // ~13 GB to derive an ~8.6 MiB table. The file variant swallows
            // its own errors into an empty table, so an unreadable blob keeps
            // the whole-blob serve path without failing an instance that is
            // already persisted.
            // F5 (0310): the table is built with the frame cap. On an over-cap
            // instance the blob (already renamed into CAS by
            // `rename_temp_to_cas`) is UNLINKED and the instance is rejected -
            // no CAS residue, no manifest entry, no whole-blob serve path.
            let frames = match stored_bytes.as_deref() {
                Some(bytes) => frame_records_capped(bytes, state.config.max_frames_per_instance),
                None => frame_records_from_file_capped(
                    &cas_target_of(&state.cas, &clarus_adapters::hex_encode(&stored_blake3)),
                    state.config.max_frames_per_instance,
                ),
            };
            let frames = match frames {
                Ok(f) => f,
                Err(e) => {
                    // Unlink the just-stored blob: an over-cap instance must
                    // not leave a CAS object behind (dedup would otherwise
                    // resurrect it on the next STOW attempt).
                    let _ = std::fs::remove_file(cas_target_of(
                        &state.cas,
                        &clarus_adapters::hex_encode(&stored_blake3),
                    ));
                    return Err(too_many_frames_error(&sc, &si, e));
                }
            };
            batch_add(state, batch, meta, stored_blake3, stored_size, frames);
            Ok(InstanceOutcome::Stored {
                sop_class_uid: sc,
                sop_instance_uid: si,
                study_instance_uid,
                series_instance_uid,
                patient_id,
            })
        }
        InsertResult::Idempotent {
            sop_class_uid: sc,
            sop_instance_uid: si,
            study_instance_uid,
            series_instance_uid,
            patient_id,
        } => {
            rename_temp_to_cas(state, temp_path, blake3, &sc, &si)?;
            Ok(InstanceOutcome::Idempotent {
                sop_class_uid: sc,
                sop_instance_uid: si,
                study_instance_uid,
                series_instance_uid,
                patient_id,
            })
        }
        InsertResult::Updated {
            sop_class_uid: sc,
            sop_instance_uid: si,
            study_instance_uid,
            series_instance_uid,
            patient_id,
            old_attrs,
            old_blake3,
            old_tier,
        } => {
            // N1: the part is NOT read back into memory here.
            //
            // The previous version did `std::fs::read(temp_path)` before hashing
            // and frame-scanning, so a corrected multi-GB whole-slide instance
            // (a client re-sending a WSI it already stored) allocated the whole
            // thing once, and twice when hidden-PHI excision ran on a copy. The
            // temp file is already on disk with the incoming bytes; the update
            // core now appends the audit embed to THAT file and derives the
            // hash, the frame table and the CAS target from it.
            //
            // The hidden-PHI path still needs the original bytes in memory (it
            // writes TWO CAS blobs: the alibi original and the sanitized one),
            // so it keeps reading the file - that is the documented exception,
            // not the common path.
            materialize_update_from_file(
                state,
                meta,
                temp_path,
                &old_attrs,
                coerce,
                modifier,
                sc,
                si,
                study_instance_uid,
                series_instance_uid,
                patient_id,
                old_blake3,
                old_tier,
            )
        }
        InsertResult::Conflict {
            sop_class_uid: sc,
            sop_instance_uid: si,
            existing_patient_id,
            conflicting_patient_id,
        } => {
            let _ = std::fs::remove_file(temp_path);
            Ok(InstanceOutcome::Conflict {
                sop_class_uid: sc,
                sop_instance_uid: si,
                existing_patient_id,
                conflicting_patient_id,
            })
        }
    }
}

/// Atomic rename temp -> CAS under its BLAKE3 key (dedup: drop the temp when
/// the target already exists - the self-heal case for Idempotent).
///
/// 0710: the TempFile path wrote its bytes with D6 "no flush", so before this
/// rename they sit in the dirty page cache. The manifest that vouches for the
/// blob is synced LATER (end of request), so without a barrier here the server
/// would answer 200 with a durable manifest and a NON-durable blob. The fix is
/// a `sync_all` on the OPEN temp fd BEFORE the rename; on failure the instance
/// FAILS (per-SOP `StowError`, the client keeps its copy) - the blob is the
/// unrecoverable pixel data and "200 = successfully stored" must not lie.
fn rename_temp_to_cas(
    state: &AppState,
    temp_path: &Path,
    blake3: &[u8; 32],
    sop_class_uid: &str,
    sop_instance_uid: &str,
) -> Result<(), StowError> {
    let cas_key = clarus_adapters::hex_encode(blake3);
    let cas_target = cas_target_of(&state.cas, &cas_key);
    if cas_target.exists() {
        let _ = std::fs::remove_file(temp_path);
        return Ok(());
    }
    if let Some(parent) = cas_target.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        let _ = std::fs::remove_file(temp_path);
        return Err(stow_error_with_sop(
            500,
            format!("Failed to create CAS bucket: {e}"),
            Some(sop_class_uid),
            sop_instance_uid,
        ));
    }
    // DURABILITY BARRIER: flush the temp file's bytes while we still hold a
    // handle, before the rename publishes them under the content address.
    // Opened write(true) because FlushFileBuffers (Windows `sync_all`) needs a
    // GENERIC_WRITE handle; harmless on Unix. A sync failure means the blob
    // cannot be vouched for - drop the temp and fail THIS instance.
    let synced = std::fs::File::options()
        .write(true)
        .open(temp_path)
        .and_then(|f| f.sync_all());
    if let Err(e) = synced {
        let _ = std::fs::remove_file(temp_path);
        return Err(stow_error_with_sop(
            500,
            format!("CAS blob sync failed: {e}"),
            Some(sop_class_uid),
            sop_instance_uid,
        ));
    }
    if let Err(e) = std::fs::rename(temp_path, &cas_target) {
        let _ = std::fs::remove_file(temp_path);
        return Err(stow_error_with_sop(
            500,
            format!("Atomic rename failed: {e}"),
            Some(sop_class_uid),
            sop_instance_uid,
        ));
    }
    // The bytes are durable; make the new DIRECTORY entry durable too. Best
    // effort on Unix only - Windows cannot FlushFileBuffers a directory handle
    // (same documented OS boundary as the manifest dir).
    sync_cas_bucket_dir(&cas_target);
    Ok(())
}

/// Best-effort directory fsync for a CAS bucket after a blob rename.
///
/// The blob CONTENT is already durable when this runs; this only makes the new
/// `dir/xx/yyyy...` link durable. A failure is a `clog_warn`, never a
/// per-SOP error: the directory entry is recoverable (a re-upload simply
/// re-renames the same content address). Unix-only - on Windows a directory
/// cannot be opened for `FlushFileBuffers`, so the directory entry is the OS's
/// zone (the file itself is still synced by the caller).
fn sync_cas_bucket_dir(cas_target: &Path) {
    #[cfg(unix)]
    if let Some(bucket_dir) = cas_target.parent()
        && let Err(e) = std::fs::File::open(bucket_dir).and_then(|d| d.sync_all())
    {
        crate::clog_warn!(
            error = %e,
            "CAS bucket dir sync failed (best-effort, client unaffected)"
        );
    }
    #[cfg(not(unix))]
    {
        // No directory fsync on Windows (FlushFileBuffers needs a file handle).
        let _ = cas_target;
    }
}

// -- DICOM file reading (sync) -------------------------------------------

/// Read the DICOM file header (up to `max_header` bytes from
/// `config.max_part_header`) - sync `std::fs`.
fn read_dicom_header(path: &Path, max_header: usize) -> Result<Vec<u8>, DicomError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    let read_len = max_header.min(file_len as usize);
    let mut buf = vec![0u8; read_len];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// Locate PixelData in a file whose header exceeded the STOW parse window.
///
/// The parser is not asked to walk a whole multi-gigabyte file for one offset:
/// it is handed progressively larger PREFIXES until the pixel tag appears. The
/// offset it reports is already relative to the file start, and the size keeps
/// doubling only while the header genuinely has not ended, so a normal instance
/// answers on the first or second probe. Bounded at 64 MiB - a header larger
/// than that is not a header, and refusing is better than an unbounded read.
fn pixel_data_offset_from_file(path: &Path) -> Option<u64> {
    use clarus_ports::dicom::DicomParser;
    let len = std::fs::metadata(path).ok()?.len() as usize;
    let mut window = 8 * 1024 * 1024usize;
    loop {
        let probe = window.min(len);
        let header = read_dicom_header(path, probe).ok()?;
        if let Ok(meta) = clarus_adapters::dicom_rs::DicomRsParser.extract_metadata(&header)
            && let Some(at) = meta.pixel_data_offset
        {
            return Some(at);
        }
        if probe == len || window >= 64 * 1024 * 1024 {
            return None;
        }
        window *= 2;
    }
}

/// Counts how many times the STOW path falls back to a FULL file re-read to
/// compute the content hash (`blake3_of_file`). The folded `CasWrite` digest
/// means the common case must keep this at ZERO for the request that just
/// streamed the bytes; only a CRLF-truncated part recomputes. The wire test in
/// the dedicated `blake3_fold` binary asserts the delta.
pub static STOW_HASH_READS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Stream a file through a BLAKE3 hasher in 64 KiB chunks (never buffer the
/// whole file) - returns the 32-byte digest that addresses CAS.
///
/// Pub for the ap101 gate (stack buffer + the facade hasher, no heap). Routes
/// through [`clarus_core::hash::cas_hasher`] (never `blake3::Hasher`) so a
/// backend swap is transparent.
pub fn blake3_of_file(path: &Path) -> std::io::Result<[u8; 32]> {
    use std::io::Read;
    STOW_HASH_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut file = std::fs::File::open(path)?;
    let mut hasher = clarus_core::hash::cas_hasher();
    let mut buf = [0u8; crate::handlers::wado_range::WADO_CHUNK_SIZE];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        clarus_core::hash::CasHasher::update(&mut hasher, &buf[..n]);
    }
    Ok(clarus_core::hash::CasHasher::finalize(hasher))
}

fn metrics_parse_error() {
    crate::metrics::record_dicom_parse_error();
}

// -- FR-001: DICOM Type 2 field validation -------------------------------

/// FR-001 coercion report: which Type 2 fields had to be generated (feeds the
/// (0400,0561) COERCE items; `any` drives the B000 Warn Reason).
#[derive(Debug, Default)]
pub(crate) struct CoerceReport {
    pub any: bool,
    /// `(group, element)` of generated fields, in fixed order.
    pub generated: Vec<(u16, u16)>,
}

/// Validate and coerce empty DICOM Type 2 fields (must run BEFORE CAS write).
///
/// Returns the [`CoerceReport`] (`any` = true when at least one field had to
/// be generated) or `Err(reason)` on failure. The `any` flag drives the STOW
/// Warn Reason `(0008,1196) = 45056` (B000) in the response
/// ReferencedSOPSequence; the generated tags feed the
/// (0400,0561) COERCE items (2808).
fn validate_and_coerce(meta: &mut InstanceMeta) -> Result<CoerceReport, String> {
    let mut report = CoerceReport::default();

    if meta.study_instance_uid.is_empty() {
        let uid = clarus_core::stow::generate_study_instance_uid(
            &meta.patient_id,
            meta.study_date.as_deref(),
            meta.study_time.as_deref(),
            meta.accession_number.as_deref(),
            &meta.modality,
        );
        let uid = match uid {
            Ok(u) => u,
            Err(_) => generate_study_uid_without_patient(meta),
        };
        crate::clog_warn!(
            study_uid = %uid,
            "StudyInstanceUID empty - generated deterministically (B-007)"
        );
        meta.study_instance_uid = uid;
        report.any = true;
    }

    if meta.patient_id.is_empty() {
        let new_id = clarus_core::stow::generate_patient_id(&meta.study_instance_uid)
            .map_err(|e| e.to_string())?;
        crate::clog_warn!(
            patient_id = %new_id,
            study_uid = %meta.study_instance_uid,
            "PatientID empty - generated from StudyInstanceUID"
        );
        meta.patient_id = new_id;
        report.any = true;
        report.generated.push((0x0010, 0x0020));
    }

    if meta.patient_name.is_empty() {
        let new_name = clarus_core::stow::generate_patient_name(&meta.study_instance_uid)
            .map_err(|e| e.to_string())?;
        crate::clog_warn!(
            patient_name = %new_name,
            study_uid = %meta.study_instance_uid,
            "PatientName empty - generated from StudyInstanceUID"
        );
        meta.patient_name = new_name;
        report.any = true;
        report.generated.push((0x0010, 0x0010));
    }

    Ok(report)
}

/// StudyInstanceUID fallback when PatientID is empty too.
fn generate_study_uid_without_patient(meta: &InstanceMeta) -> String {
    let digest = clarus_core::stow::fallback_study_uid_key(
        meta.study_date.as_deref(),
        meta.study_time.as_deref(),
        meta.accession_number.as_deref(),
        &meta.modality,
    );
    format!("2.25.{digest}")
}

// -- Hex / ENOSPC helpers ------------------------------------------------

/// Detect a full volume.
///
/// `ErrorKind::StorageFull` (stable since 1.83) is the standard's canonical
/// classification and already covers both platforms: unix maps `ENOSPC` to it,
/// and Windows maps `ERROR_HANDLE_DISK_FULL` (39) and `ERROR_DISK_FULL` (112) to
/// it while leaving 28 (`ERROR_OUT_OF_PAPER`) as `Uncategorized`. So one arm
/// replaces the previous per-platform raw-code table, and the two-number pairing
/// that produced a false 507 for "the printer is out of paper" can no longer be
/// written down.
///
/// The direction is one-way: an error constructed FROM the kind reports
/// `raw_os_error() == None`, so a raw code is never recovered here.
fn is_enospc(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::StorageFull
}

#[cfg(test)]
mod enospc_tests {
    use std::io::{Error, ErrorKind};

    /// The kind of an error built from a raw OS code is what decides.
    fn kind_of(code: i32) -> ErrorKind {
        Error::from_raw_os_error(code).kind()
    }

    /// 28 (`ENOSPC`) is the one unix disk-full code, and the two numbers that
    /// share its shape on this platform are not.
    ///
    /// 39 is `ENOTEMPTY` on Linux and `EDESTADDRREQ` on macOS: under the old
    /// un-scoped `Some(28) | Some(39) | Some(112)` match it became a 507 "disk
    /// full" with `Retry-After: 60`.
    #[cfg(unix)]
    #[test]
    fn enospc_is_detected_and_look_alikes_are_not() {
        assert!(super::is_enospc(&Error::from_raw_os_error(28)));
        assert_eq!(kind_of(28), ErrorKind::StorageFull, "ENOSPC");
        assert!(
            !super::is_enospc(&Error::from_raw_os_error(39)),
            "39 is ENOTEMPTY/EDESTADDRREQ on unix, never a full disk"
        );
        assert!(
            !super::is_enospc(&Error::from_raw_os_error(112)),
            "112 is undefined on unix"
        );
        assert!(!super::is_enospc(&Error::from(ErrorKind::Other)));
    }

    /// Windows raises BOTH 39 and 112 for a full volume, and 28 there means the
    /// printer is out of paper.
    ///
    /// The defect on this platform was never a missed disk-full: the old code
    /// caught 39 and 112. It was that 28 was read as disk-full, so a paper-out
    /// answered 507 with `Retry-After: 60`.
    #[cfg(windows)]
    #[test]
    fn windows_disk_full_codes_are_detected_and_paper_out_is_not() {
        assert!(
            super::is_enospc(&Error::from_raw_os_error(39)),
            "39 is ERROR_HANDLE_DISK_FULL"
        );
        assert!(
            super::is_enospc(&Error::from_raw_os_error(112)),
            "112 is ERROR_DISK_FULL"
        );
        assert_eq!(kind_of(39), ErrorKind::StorageFull);
        assert_eq!(kind_of(112), ErrorKind::StorageFull);
        assert!(
            !super::is_enospc(&Error::from_raw_os_error(28)),
            "28 is ERROR_OUT_OF_PAPER on Windows, never a full disk"
        );
        assert!(
            !super::is_enospc(&Error::from_raw_os_error(145)),
            "145 is ERROR_DIR_NOT_EMPTY"
        );
        assert!(!super::is_enospc(&Error::from(ErrorKind::Other)));
    }
}

#[cfg(test)]
mod stow_uid_tests {
    use super::excise_original_attrs;
    use super::validate_instance_uids;
    use clarus_ports::dicom::{DicomParser, InstanceMeta};

    fn meta(study: &str, series: &str, sop: &str) -> InstanceMeta {
        InstanceMeta {
            study_instance_uid: study.into(),
            series_instance_uid: series.into(),
            sop_instance_uid: sop.into(),
            ..Default::default()
        }
    }

    #[test]
    fn valid_uids_pass() {
        assert!(
            validate_instance_uids(&meta(
                "1.3.6.1.4.1.25403.246788402477788.12876.20260812120435.1",
                "1.3.6.1.4.1.25403.246788402477788.12876.20260812120641.3.2",
                "1.3.6.1.4.1.25403.246788402477788.12876.20260812120641.3.2.1",
            ))
            .is_ok()
        );
    }

    #[test]
    fn non_numeric_uid_component_rejected() {
        let err = validate_instance_uids(&meta(
            "1.3.6.1.4.1.25403.246788402477788.12876.20260812120435.1",
            "1.3.6.1.4.1.25403.246788402477788.12876.20260812120641.3.j2k",
            "1.3.6.1.4.1.25403.246788402477788.12876.20260812120641.3.j2k.1",
        ))
        .unwrap_err();
        assert_eq!(err.status, 400);
        assert!(String::from_utf8_lossy(&err.body).contains("Invalid DICOM UID"));
        assert!(String::from_utf8_lossy(&err.body).contains("SeriesInstanceUID"));
    }

    #[test]
    fn consecutive_dots_rejected() {
        assert!(validate_instance_uids(&meta("1.2..3", "1.2.3", "1.2.3.4")).is_err());
    }

    // -- 3008 excision known-answer tests -------------------------------

    #[test]
    fn excise_union_overlap_and_adjacency_collapse() {
        // Overlapping [4,8)/[6,10) collapse to [4,10); adjacent [12,14)/[14,16)
        // collapse to [12,16); no span is excised twice.
        let src: Vec<u8> = (0..20).collect();
        let out = excise_original_attrs(&src, &[(4, 8), (6, 10), (12, 14), (14, 16)]);
        assert_eq!(out, vec![0, 1, 2, 3, 10, 11, 16, 17, 18, 19]);
        // Unsorted input works too (sorted internally).
        let out2 = excise_original_attrs(&src, &[(14, 16), (4, 8), (12, 14), (6, 10)]);
        assert_eq!(out2, out);
        // Empty ranges = identity.
        assert_eq!(excise_original_attrs(&src, &[]), src);
    }

    #[test]
    fn excise_never_covers_pixel_bytes() {
        // A hidden range before pixels: the pixel element bytes stay intact.
        let mut bytes = Vec::new();
        let mut kept = Vec::new();
        bytes.extend_from_slice(b"PRE ");
        kept.extend_from_slice(b"PRE ");
        let s = bytes.len();
        bytes.extend_from_slice(b"<0400,0550 hidden>");
        let e = bytes.len();
        bytes.extend_from_slice(b" MID ");
        kept.extend_from_slice(b" MID ");
        let pixels: Vec<u8> = (0..64).map(|i| (i * 7) as u8).collect();
        bytes.extend_from_slice(&pixels);
        kept.extend_from_slice(&pixels);
        let out = excise_original_attrs(&bytes, &[(s, e)]);
        assert_eq!(out, kept, "pixels byte-identical");
    }

    /// Known-answer: an explicit-VR-LE dataset with a top-level 0561 AFTER
    /// pixels (our own Updated shape) - parse + excise reproduce the dataset
    /// rebuilt without the element, byte-exact.
    #[test]
    fn kat_explicit_le_0561_after_pixels() {
        let mut bytes = Vec::new();
        let mut kept = Vec::new();
        bytes.extend_from_slice(&[0u8; 128]);
        bytes.extend_from_slice(b"DICM");
        kept.extend_from_slice(&[0u8; 128]);
        kept.extend_from_slice(b"DICM");
        let add = |bytes: &mut Vec<u8>, group: u16, elem: u16, vr: &[u8; 2], value: &[u8]| {
            bytes.extend_from_slice(&group.to_le_bytes());
            bytes.extend_from_slice(&elem.to_le_bytes());
            bytes.extend_from_slice(vr);
            let long = matches!(
                std::str::from_utf8(vr).unwrap_or(""),
                "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
            );
            if long {
                bytes.extend_from_slice(&[0u8, 0u8]);
                bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
            } else {
                bytes.extend_from_slice(&(value.len() as u16).to_le_bytes());
            }
            bytes.extend_from_slice(value);
            if !value.len().is_multiple_of(2) {
                bytes.push(0x00);
            }
        };
        // Meta header (0002,0010).
        add(&mut bytes, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
        add(&mut kept, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
        add(
            &mut bytes,
            0x0008,
            0x0016,
            b"UI",
            b"1.2.840.10008.5.1.4.1.1.2",
        );
        add(
            &mut kept,
            0x0008,
            0x0016,
            b"UI",
            b"1.2.840.10008.5.1.4.1.1.2",
        );
        add(&mut bytes, 0x0010, 0x0010, b"PN", b"");
        add(&mut kept, 0x0010, 0x0010, b"PN", b"");
        add(&mut bytes, 0x0020, 0x000D, b"UI", b"1.2.3.3301");
        add(&mut kept, 0x0020, 0x000D, b"UI", b"1.2.3.3301");
        add(&mut bytes, 0x0020, 0x000E, b"UI", b"1.2.3.3301.1");
        add(&mut kept, 0x0020, 0x000E, b"UI", b"1.2.3.3301.1");
        add(&mut bytes, 0x0008, 0x0018, b"UI", b"1.2.3.3301.1.1");
        add(&mut kept, 0x0008, 0x0018, b"UI", b"1.2.3.3301.1.1");
        add(&mut bytes, 0x0008, 0x0060, b"CS", b"CT");
        add(&mut kept, 0x0008, 0x0060, b"CS", b"CT");
        add(&mut bytes, 0x7FE0, 0x0010, b"OB", &[0x00, 0x01]);
        add(&mut kept, 0x7FE0, 0x0010, b"OB", &[0x00, 0x01]);
        // Hidden 0561 AFTER pixels: item with PN HIDDEN^NAME.
        let mut inner = Vec::new();
        let mut item = Vec::new();
        item.extend_from_slice(&0x0010u16.to_le_bytes());
        item.extend_from_slice(&0x0010u16.to_le_bytes());
        item.extend_from_slice(b"PN");
        item.extend_from_slice(&11u16.to_le_bytes()); // "HIDDEN^NAME" = 11 bytes
        item.extend_from_slice(b"HIDDEN^NAME");
        item.push(0x00); // pad
        inner.extend_from_slice(&0xFFFE_u16.to_le_bytes());
        inner.extend_from_slice(&0xE000_u16.to_le_bytes());
        inner.extend_from_slice(&(item.len() as u32).to_le_bytes());
        inner.extend_from_slice(&item);
        add(&mut bytes, 0x0400, 0x0561, b"SQ", &inner);

        let parser = clarus_adapters::dicom_rs::DicomRsParser;
        let meta = parser.extract_metadata(&bytes).expect("fixture parses");
        assert!(
            meta.hidden_identifying_attrs,
            "identifying value inside the sequence"
        );
        assert_eq!(meta.original_attrs_ranges.len(), 1);
        let sanitized = excise_original_attrs(&bytes, &meta.original_attrs_ranges);
        assert_eq!(sanitized, kept, "byte-exact known answer");
    }

    /// `frame_records` must map EVERY frame, with no drop at the old u16
    /// ceiling. A 65537-frame instance is what a whole-slide pyramid produces
    /// once it exceeds 65535 tiles, and silently truncating it there would
    /// degrade tile serving to a whole-blob read exactly where it hurts most.
    #[test]
    fn frame_records_keeps_numbers_above_u16_max() {
        use super::frame_records;

        const COUNT: u32 = 65_537;
        // Minimal encapsulated PixelData: an empty Basic Offset Table followed
        // by COUNT 8-byte items, then the sequence delimiter.
        let mut pixel = Vec::with_capacity(8 + COUNT as usize * 16 + 8);
        pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
        for i in 0..COUNT {
            pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
            pixel.extend_from_slice(&8u32.to_le_bytes());
            pixel.extend_from_slice(&(i as u64).to_le_bytes());
        }
        pixel.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);

        let mut file = vec![0u8; 128];
        file.extend_from_slice(b"DICM");
        let mut us = |g: u16, e: u16, v: u16| {
            file.extend_from_slice(&g.to_le_bytes());
            file.extend_from_slice(&e.to_le_bytes());
            file.extend_from_slice(b"US");
            file.extend_from_slice(&2u16.to_le_bytes());
            file.extend_from_slice(&v.to_le_bytes());
        };
        us(0x0028, 0x0010, 2); // Rows
        us(0x0028, 0x0011, 2); // Columns
        file.extend_from_slice(&0x0028u16.to_le_bytes());
        file.extend_from_slice(&0x0008u16.to_le_bytes());
        file.extend_from_slice(b"IS");
        let nf = COUNT.to_string();
        file.extend_from_slice(&(nf.len() as u16).to_le_bytes());
        file.extend_from_slice(nf.as_bytes());
        if !nf.len().is_multiple_of(2) {
            file.push(0x00);
        }
        file.extend_from_slice(&0x7FE0u16.to_le_bytes());
        file.extend_from_slice(&0x0010u16.to_le_bytes());
        file.extend_from_slice(b"OB");
        file.extend_from_slice(&[0, 0]);
        file.extend_from_slice(&(pixel.len() as u32).to_le_bytes());
        file.extend_from_slice(&pixel);

        let records = frame_records(&file);
        assert_eq!(
            records.len(),
            COUNT as usize,
            "no frame may be dropped at the u16 ceiling"
        );
        assert_eq!(records.first().unwrap().number, 1);
        assert_eq!(records.last().unwrap().number, COUNT);
        assert!(records.last().unwrap().number > u32::from(u16::MAX));
        // Numbering is contiguous: none was skipped or duplicated.
        for (i, rec) in records.iter().enumerate() {
            assert_eq!(rec.number, i as u32 + 1, "contiguous numbering");
        }
    }
}

#[cfg(test)]
mod crlf_strip_tests {
    use super::strip_trailing_crlf_file;
    use std::io::Write;

    /// A minimal explicit-VR LE file: preamble, DICM, then `extra` appended
    /// after one (0028,0010) US Rows element whose value is `rows_value`.
    fn file_with_tail(dir: &std::path::Path, rows_value: &[u8], tail: &[u8]) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("inst.dcm");
        let mut data = vec![0u8; 128];
        data.extend_from_slice(b"DICM");
        data.extend_from_slice(&0x0028u16.to_le_bytes());
        data.extend_from_slice(&0x0010u16.to_le_bytes());
        data.extend_from_slice(b"US");
        data.extend_from_slice(&(rows_value.len() as u16).to_le_bytes());
        data.extend_from_slice(rows_value);
        data.extend_from_slice(tail);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&data).unwrap();
        path
    }

    #[test]
    fn file_variant_truncates_in_place() {
        let dir = std::env::temp_dir().join("clarus_crlf_test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = file_with_tail(&dir, &512u16.to_le_bytes(), b"\r\n\r\n");
        let full = std::fs::metadata(&path).unwrap().len();
        let mut size = full;
        strip_trailing_crlf_file(&path, &mut size).unwrap();
        assert_eq!(size, full - 4, "the framing junk is gone");
        let stored = std::fs::read(&path).unwrap();
        assert!(
            stored.ends_with(&512u16.to_le_bytes()),
            "the last element's value survives: {stored:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The regression the dataset-end bound exists for: the LAST element's
    /// value can itself end in `0D 0A` (a 16-bit sample of 0x0A0D, or a text
    /// value with a trailing CRLF). Those bytes are inside the dataset, so the
    /// strip must leave them alone - cutting by tail shape would delete real
    /// data and store a truncated blob under a fresh content address.
    #[test]
    fn a_dataset_whose_last_value_ends_in_crlf_is_not_truncated() {
        let dir = std::env::temp_dir().join("clarus_crlf_test3");
        let _ = std::fs::remove_dir_all(&dir);
        // A 4-byte value whose last two bytes are CRLF.
        let value = [0x01u8, 0x02, 0x0D, 0x0A];
        let path = file_with_tail(&dir, &value, b"");
        let full = std::fs::metadata(&path).unwrap().len();
        let before = std::fs::read(&path).unwrap();
        let mut size = full;
        strip_trailing_crlf_file(&path, &mut size).unwrap();
        assert_eq!(size, full, "bytes inside the last value are not junk");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_variant_no_change_when_clean() {
        let dir = std::env::temp_dir().join("clarus_crlf_test2");
        let _ = std::fs::remove_dir_all(&dir);
        let path = file_with_tail(&dir, &512u16.to_le_bytes(), b"");
        let full = std::fs::metadata(&path).unwrap().len();
        let mut size = full;
        strip_trailing_crlf_file(&path, &mut size).unwrap();
        assert_eq!(size, full);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// 0710: CAS write durability + atomicity (the blob fsync hole).
///
/// Each test drives the REAL write helpers against a throwaway `AppState`
/// rooted in a temp dir, so the assertions are about the actual file layout,
/// not a mock. The sync barrier itself is proven by the failure-injection
/// tests: a write/sync error must leave NO partial file at the content address
/// and must surface a per-SOP error (never a silent success).
#[cfg(test)]
mod cas_durability_tests {
    use super::{cas_target_of, rename_temp_to_cas, write_cas_dedup};
    use crate::handlers::readiness::Readiness;
    use crate::state::AppState;
    use crate::state::TranscodePool;
    use std::path::{Path, PathBuf};
    use std::sync::RwLock as StdRwLock;

    /// A minimal `AppState` rooted under a fresh temp dir (fixtures only; the
    /// CAS writers under test touch nothing else).
    fn app_state(root: &Path) -> AppState {
        let btree_root = root.join("btree");
        let manifest_dir = btree_root.join("manifests");
        let idx_dir = btree_root.join("idx");
        std::fs::create_dir_all(&manifest_dir).expect("manifests dir");
        std::fs::create_dir_all(&idx_dir).expect("idx dir");
        let storage_dir = root.join("cas");
        std::fs::create_dir_all(&storage_dir).expect("cas dir");

        let mut config = crate::config::Config::test_default();
        config.storage_dir = storage_dir.to_string_lossy().to_string();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;

        AppState {
            cas: clarus_adapters::storage::ContentAddressedStorage::new(storage_dir)
                .expect("open cas"),
            codec_registry: clarus_adapters::codec::CodecRegistry::default(),
            ups_events: crate::sse_fanout::DualEventSink::new(),
            start_time: std::time::Instant::now(),
            metrics_ring: std::sync::Arc::new(StdRwLock::new(std::collections::VecDeque::new())),
            log_buffer: std::sync::Arc::new(StdRwLock::new(std::collections::VecDeque::new())),
            maintenance: std::sync::Arc::new(StdRwLock::new(
                crate::state::MaintenanceState::default(),
            )),
            #[cfg(feature = "s3")]
            s3: std::sync::Arc::new(std::sync::OnceLock::new()),
            cold_cas: None,
            shadow: crate::shadow::ShadowTier::new(idx_dir.clone()),
            study_scanner: clarus_core::query::StudyScanner::new(manifest_dir, idx_dir),
            config,
            workitems_dir: btree_root.join("workitems"),
            commitments_dir: btree_root.join("commitments"),
            transcode_pool: TranscodePool::new(32),
            readiness: Readiness::new(),
        }
    }

    /// Unique temp root per test (pid + a monotonic counter, never a clock).
    fn unique_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("clarus_cas_dur_{tag}_{}_{seq}", std::process::id()))
    }

    /// Count files directly under a bucket dir (excludes subdirs).
    fn bucket_file_count(bucket: &Path) -> usize {
        std::fs::read_dir(bucket)
            .map(|it| {
                it.filter_map(Result::ok)
                    .filter(|e| e.path().is_file())
                    .count()
            })
            .unwrap_or(0)
    }

    /// A successful dedup write puts the FULL bytes at the content address and
    /// leaves no temp sibling behind.
    #[test]
    fn write_cas_dedup_writes_the_full_blob_and_no_temp_leftover() {
        let root = unique_root("ok");
        let _ = std::fs::remove_dir_all(&root);
        let app = app_state(&root);
        let bytes = b"the whole blob".repeat(64);
        let key = [0x11u8; 32];

        write_cas_dedup(&app, &key, &bytes, None, "1.2.3.4").expect("write");

        let target = cas_target_of(&app.cas, &clarus_adapters::hex_encode(&key));
        assert_eq!(std::fs::read(&target).expect("read back"), bytes);
        let bucket = target.parent().expect("bucket dir");
        assert_eq!(
            bucket_file_count(bucket),
            1,
            "exactly one file: the blob (no .tmp sibling left behind)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The dedup early-return writes NOTHING: an existing target is left
    /// untouched and no sync/temp work happens (the counters/bytes are the
    /// observable proof - the file mtime and content are unchanged).
    #[test]
    fn write_cas_dedup_dedup_early_return_does_not_write() {
        let root = unique_root("dedup");
        let _ = std::fs::remove_dir_all(&root);
        let app = app_state(&root);
        let key = [0x22u8; 32];
        let target = cas_target_of(&app.cas, &clarus_adapters::hex_encode(&key));
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"already here").unwrap();
        let before = std::fs::read(&target).unwrap();

        // Different bytes, same key: dedup must keep the existing file.
        write_cas_dedup(&app, &key, b"DIFFERENT", None, "1.2.3.4").expect("dedup ok");

        assert_eq!(std::fs::read(&target).unwrap(), before, "untouched");
        assert_eq!(
            bucket_file_count(target.parent().unwrap()),
            1,
            "no temp sibling created on the dedup path"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A write that cannot reach the content address (the bucket dir is a FILE,
    /// so `create_dir_all` fails) must return an error and leave NOTHING at the
    /// final path - the tmp+rename guarantee: final is whole or absent.
    #[test]
    fn write_cas_dedup_failure_leaves_no_partial_file_at_final_path() {
        let root = unique_root("fail");
        let _ = std::fs::remove_dir_all(&root);
        let app = app_state(&root);
        let key = [0x33u8; 32];
        let target = cas_target_of(&app.cas, &clarus_adapters::hex_encode(&key));
        // Block the bucket dir: create a FILE where the directory would be.
        let bucket = target.parent().unwrap();
        std::fs::create_dir_all(bucket.parent().unwrap()).unwrap();
        std::fs::write(bucket, b"not a directory").unwrap();

        let err = write_cas_dedup(&app, &key, b"cannot be written", None, "1.2.3.4")
            .expect_err("write must fail when the bucket dir cannot be created");
        assert_eq!(err.status, 500, "per-SOP failure, not a silent 200");
        assert!(
            !target.exists(),
            "the final content address must be ABSENT, never partial"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `rename_temp_to_cas` on a missing temp returns a per-SOP error and
    /// leaves no file at the content address (the sync barrier opens the temp
    /// FIRST, so a vanished temp is a sync/open failure).
    #[test]
    fn rename_temp_to_cas_missing_temp_fails_without_publishing() {
        let root = unique_root("renfail");
        let _ = std::fs::remove_dir_all(&root);
        let app = app_state(&root);
        let key = [0x44u8; 32];
        let temp = root.join("does-not-exist.dcm");

        let err = rename_temp_to_cas(&app, &temp, &key, "1.2.3", "1.2.3.4")
            .expect_err("a missing temp must fail the instance");
        assert_eq!(err.status, 500);
        let target = cas_target_of(&app.cas, &clarus_adapters::hex_encode(&key));
        assert!(!target.exists(), "nothing published at the content address");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The happy path for the TempFile writer: the temp is published at the
    /// content address with the full bytes, the temp is GONE, and no stray
    /// file remains in the bucket.
    #[test]
    fn rename_temp_to_cas_publishes_full_blob_and_consumes_temp() {
        let root = unique_root("renok");
        let _ = std::fs::remove_dir_all(&root);
        let app = app_state(&root);
        let key = [0x55u8; 32];
        let bytes = b"streamed temp bytes".repeat(32);
        let temp = root.join("uploaded.tmp");
        std::fs::write(&temp, &bytes).unwrap();

        rename_temp_to_cas(&app, &temp, &key, "1.2.3", "1.2.3.4").expect("rename");

        let target = cas_target_of(&app.cas, &clarus_adapters::hex_encode(&key));
        assert_eq!(std::fs::read(&target).unwrap(), bytes, "full blob durable");
        assert!(!temp.exists(), "temp consumed by the rename");
        assert_eq!(
            bucket_file_count(target.parent().unwrap()),
            1,
            "only the blob lives in the bucket"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Dedup on the TempFile path: an existing target drops the temp and does
    /// NOT overwrite the stored blob.
    #[test]
    fn rename_temp_to_cas_dedup_drops_temp_and_keeps_existing() {
        let root = unique_root("rendedup");
        let _ = std::fs::remove_dir_all(&root);
        let app = app_state(&root);
        let key = [0x66u8; 32];
        let target = cas_target_of(&app.cas, &clarus_adapters::hex_encode(&key));
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"stored first").unwrap();
        let temp = root.join("second.tmp");
        std::fs::write(&temp, b"second upload").unwrap();

        rename_temp_to_cas(&app, &temp, &key, "1.2.3", "1.2.3.4").expect("dedup");

        assert_eq!(std::fs::read(&target).unwrap(), b"stored first");
        assert!(!temp.exists(), "the redundant temp is dropped");
        let _ = std::fs::remove_dir_all(&root);
    }
}

// -- IOCM trust gate and re-store refusal --------------------------------

#[cfg(test)]
mod iocm_anyclass_tests {
    use crate::state::AppState;
    use crate::state::TranscodePool;
    use clarus_core::manifest::{
        AttrValue, InstanceRecord, Manifest, NOT_REJECTED_REASON, QUALITY_REASON_CODE,
        SeriesRecord, StudyQido, SysHeader, TIER_HOT,
    };
    use std::path::{Path, PathBuf};

    const STUDY: &str = "1.2.840.10008.5.1.4.1.1.88.22.1";
    const SERIES: &str = "1.2.840.10008.5.1.4.1.1.88.22.2";
    const INSTANCE: &str = "1.2.840.10008.5.1.4.1.1.88.22.3";
    const CT: &str = "1.2.840.10008.5.1.4.1.1.2";
    /// SOP Instance UID of the Key Object Selection note the batch cites.
    const NOTE: &str = "1.2.840.10008.5.1.4.1.1.88.22.9";

    const PATIENT_SAFETY: u32 = 113_037;
    const INCORRECT_MWL: u32 = 113_038;
    const RETENTION: u32 = 113_039;

    /// Unique temp root per test (pid + counter, never a clock - a clock is
    /// not a uniqueness primitive when tests run in parallel).
    fn unique_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("clarus_iocm_{tag}_{}_{seq}", std::process::id()))
    }

    fn build_state(root: &Path, anyclass_enable: bool) -> AppState {
        let manifest_dir = root.join("manifests");
        let idx_dir = root.join("idx");
        std::fs::create_dir_all(&manifest_dir).expect("manifest dir");
        std::fs::create_dir_all(&idx_dir).expect("idx dir");
        std::fs::create_dir_all(root.join("storage")).expect("storage dir");

        let mut config = crate::config::Config::test_default();
        let audit_dir = root.join("audit");
        std::fs::create_dir_all(&audit_dir).expect("audit dir");
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.btree.audit_dir = audit_dir.to_string_lossy().to_string();
        config.storage_dir = root.join("storage").to_string_lossy().to_string();
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;
        config.anyclass_enable = anyclass_enable;

        AppState {
            cas: clarus_adapters::storage::ContentAddressedStorage::new(root.join("cas"))
                .expect("open cas"),
            codec_registry: clarus_adapters::codec::CodecRegistry::default(),
            ups_events: crate::sse_fanout::DualEventSink::new(),
            start_time: std::time::Instant::now(),
            metrics_ring: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::VecDeque::new(),
            )),
            log_buffer: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::VecDeque::new(),
            )),
            maintenance: std::sync::Arc::new(std::sync::RwLock::new(
                crate::state::MaintenanceState::default(),
            )),
            #[cfg(feature = "s3")]
            s3: std::sync::Arc::new(std::sync::OnceLock::new()),
            cold_cas: None,
            shadow: crate::shadow::ShadowTier::new(idx_dir.clone()),
            study_scanner: clarus_core::query::StudyScanner::new(manifest_dir, idx_dir),
            config,
            workitems_dir: root.join("workitems"),
            commitments_dir: root.join("commitments"),
            transcode_pool: TranscodePool::new(8),
            readiness: crate::handlers::readiness::Readiness::new(),
        }
    }

    /// Hide the seeded instance with `reason`, straight into the manifest.
    ///
    /// Used where the reason's own scope would refuse the reference (Quality
    /// is class-bounded and never hides a CT), so the hide cannot be produced
    /// by `apply_rejections` - but the re-store rule must still be exercised
    /// against whatever a manifest can actually hold.
    fn force_hidden(state: &AppState, reason: u32) {
        let dir = PathBuf::from(&state.config.btree.manifest_dir);
        let path = clarus_core::stow::manifest_path(&dir, STUDY);
        let mut manifest = clarus_core::manifest::read_manifest(&path).expect("read");
        manifest.series[0].instances[0].rejection_reason = reason;
        std::fs::write(&path, manifest.encode().expect("encode")).expect("rewrite");
    }

    /// A visible CT instance in its own study manifest.
    fn seed_visible_instance(state: &AppState) {
        let manifest = Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 0,
                arrived_at: 1,
            },
            study: StudyQido {
                attrs: vec![(
                    clarus_core::query::tag::STUDY_INSTANCE_UID,
                    AttrValue::Text(STUDY.to_string()),
                )],
                ..Default::default()
            },
            series: vec![SeriesRecord {
                attrs: vec![(
                    clarus_core::query::tag::SERIES_INSTANCE_UID,
                    AttrValue::Text(SERIES.to_string()),
                )],
                instances: vec![InstanceRecord {
                    blake3: [0u8; 32],
                    attrs: vec![
                        (
                            clarus_core::query::tag::SOP_INSTANCE_UID,
                            AttrValue::Text(INSTANCE.to_string()),
                        ),
                        (
                            clarus_core::query::tag::SOP_CLASS_UID,
                            AttrValue::Text(CT.to_string()),
                        ),
                    ],
                    transfer_syntax: "1.2.840.10008.1.2.1".to_string(),
                    storage_tier: TIER_HOT,
                    rejection_reason: NOT_REJECTED_REASON,
                    file_size: 64,
                    frames: Vec::new(),
                }],
            }],
        };
        let dir = PathBuf::from(&state.config.btree.manifest_dir);
        let path = clarus_core::stow::manifest_path(&dir, STUDY);
        std::fs::write(&path, manifest.encode().expect("encode")).expect("write manifest");
        // The rejection marking step resolves the owner study through the
        // SOP->study index, which a real STOW writes. Seeding only the
        // manifest would make every hide look like "no owner study" and let a
        // test pass that never needed the hide to land.
        state
            .shadow
            .dual_write([0u8; 32], "CT", STUDY, INSTANCE)
            .expect("seed sop_owner");
    }

    /// The stored reason for the seeded instance.
    fn stored_reason(state: &AppState) -> u32 {
        let dir = PathBuf::from(&state.config.btree.manifest_dir);
        clarus_core::stow::hidden_reason_of(&dir, STUDY, INSTANCE)
    }

    /// With `anyclass_enable = false` (the default) the AnyClass hide is NOT
    /// applied - a KOS is a DICOM object any client able to reach STOW can
    /// write, so honouring it by default would let a stranger hide diagnostic
    /// images on every deployment.
    #[test]
    fn an_any_class_hide_is_ignored_when_the_feature_is_disabled() {
        let root = unique_root("disabled");
        let state = build_state(&root, false);
        seed_visible_instance(&state);

        let mut batch = super::ManifestBatch::default();
        batch.rejections.push((
            CT.to_string(),
            INSTANCE.to_string(),
            PATIENT_SAFETY,
            NOTE.to_string(),
        ));
        super::apply_rejections(&state, &batch, "tester");

        assert_eq!(
            stored_reason(&state),
            NOT_REJECTED_REASON,
            "a disabled AnyClass feature must not hide a diagnostic image"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A mid-request flush that could not write the manifest must hand the
    /// affected UIDs to the response builder exactly once.
    ///
    /// `clear()` wipes the pending write set, so a UID parked on the batch is
    /// the ONLY remaining evidence that the instance is not retrievable. Two
    /// flushes in one request must not report the same instance twice, hence
    /// the drain semantics.
    #[test]
    fn a_flush_reports_its_manifest_failures_once() {
        let mut batch = super::ManifestBatch::default();
        batch.manifest_failed.push("1.2.3.4.5.6.7.1".to_string());
        batch.manifest_failed.push("1.2.3.4.5.6.7.2".to_string());

        let first = batch.take_manifest_failed();
        assert_eq!(
            first,
            vec!["1.2.3.4.5.6.7.1".to_string(), "1.2.3.4.5.6.7.2".to_string()],
            "every instance the flush could not commit is reported"
        );
        assert!(
            batch.take_manifest_failed().is_empty(),
            "a UID is never reported by two flushes of the same request"
        );
    }

    /// `clear()` must keep the manifest failures: it runs AFTER the flush that
    /// recorded them, and before the end-of-request response is built.
    #[test]
    fn clearing_a_batch_keeps_the_manifest_failures() {
        let mut batch = super::ManifestBatch::default();
        batch.manifest_failed.push("1.2.3.4.5.6.7.1".to_string());

        batch.clear();

        assert_eq!(
            batch.take_manifest_failed(),
            vec!["1.2.3.4.5.6.7.1".to_string()],
            "an instance the archive cannot serve back stays reported as failed"
        );
    }

    /// Positive half: with `anyclass_enable = true` the hide applies, and it
    /// applies to a diagnostic class - which is the whole point of the
    /// AnyClass scope.
    #[test]
    fn an_any_class_hide_applies_when_the_feature_is_enabled() {
        let root = unique_root("enabled");
        let state = build_state(&root, true);
        seed_visible_instance(&state);

        let mut batch = super::ManifestBatch::default();
        batch.rejections.push((
            CT.to_string(),
            INSTANCE.to_string(),
            INCORRECT_MWL,
            NOTE.to_string(),
        ));
        super::apply_rejections(&state, &batch, "tester");

        assert_eq!(
            stored_reason(&state),
            INCORRECT_MWL,
            "an enabled AnyClass feature hides the diagnostic image it names"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A sequester is a clinical act, so it lands in the immutable trail with
    /// the code applied, the study, the citing note and the number of
    /// instances the note named. Without the row, an image could disappear
    /// from every search with nothing but a counter behind it.
    #[test]
    fn a_hide_appends_a_reject_row_naming_the_reason_and_the_note() {
        let root = unique_root("reject_audit");
        let state = build_state(&root, true);
        seed_visible_instance(&state);

        let mut batch = super::ManifestBatch::default();
        batch.rejections.push((
            CT.to_string(),
            INSTANCE.to_string(),
            INCORRECT_MWL,
            NOTE.to_string(),
        ));
        super::apply_rejections(&state, &batch, "op-carol");

        assert_eq!(stored_reason(&state), INCORRECT_MWL, "the hide landed");

        let dir = PathBuf::from(&state.config.btree.audit_dir);
        let mut found = String::new();
        for entry in std::fs::read_dir(&dir).expect("audit dir").flatten() {
            found.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
        }
        assert!(
            found.contains("\"event\":\"reject\""),
            "a reject row is appended: {found}"
        );
        assert!(
            found.contains("\"reason_code\":113038"),
            "the row names the code applied: {found}"
        );
        assert!(
            found.contains("\"study\":\"1.2.840.10008.5.1.4.1.1.88.22.1\""),
            "the row names the study: {found}"
        );
        assert!(
            found.contains("1.2.840.10008.5.1.4.1.1.88.22.9"),
            "the row cites the KOS note: {found}"
        );
        assert!(
            found.contains("\"count\":1"),
            "the row counts the instances the note named: {found}"
        );
        assert!(
            found.contains("op-carol"),
            "the row names the effective user: {found}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The trail is evidence, not a precondition: a hide whose audit append
    /// fails must still hide. Refusing the sequester because the audit disk is
    /// full would leave a patient-safety image visible to satisfy a logging
    /// constraint.
    #[test]
    fn a_hide_still_applies_when_the_audit_append_fails() {
        let root = unique_root("reject_audit_fail");
        let state = build_state(&root, true);
        seed_visible_instance(&state);
        // Point the trail at a path that cannot be a directory, so every
        // append fails.
        let blocked = root.join("audit-is-a-file");
        std::fs::write(&blocked, b"not a dir").expect("blocker");
        let mut state = state;
        state.config.btree.audit_dir = blocked.to_string_lossy().to_string();

        let mut batch = super::ManifestBatch::default();
        batch.rejections.push((
            CT.to_string(),
            INSTANCE.to_string(),
            PATIENT_SAFETY,
            NOTE.to_string(),
        ));
        super::apply_rejections(&state, &batch, "tester");

        assert_eq!(
            stored_reason(&state),
            PATIENT_SAFETY,
            "the hide must not depend on the audit append succeeding"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The trust gate must NOT touch Quality: an AI-result rejection stays
    /// class-bounded, so it can never hide a diagnostic image whatever the
    /// deployment's trust setting.
    #[test]
    fn a_quality_note_is_never_gated_and_never_hides_a_ct() {
        let root = unique_root("quality");
        let state = build_state(&root, false);
        seed_visible_instance(&state);

        let mut batch = super::ManifestBatch::default();
        batch.rejections.push((
            CT.to_string(),
            INSTANCE.to_string(),
            QUALITY_REASON_CODE,
            NOTE.to_string(),
        ));
        super::apply_rejections(&state, &batch, "tester");

        assert_eq!(
            stored_reason(&state),
            NOT_REJECTED_REASON,
            "Quality is class-bounded: a CT reference is out of scope"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// T2.4: a re-store of an instance hidden by patient safety or incorrect
    /// MWL is refused per instance; the retention reason is not.
    #[test]
    fn re_store_is_refused_only_for_the_two_codes_the_standard_names() {
        for (reason, refused) in [
            (PATIENT_SAFETY, true),
            (INCORRECT_MWL, true),
            (RETENTION, false),
            (QUALITY_REASON_CODE, false),
        ] {
            let root = unique_root(&format!("restore_{reason}"));
            let state = build_state(&root, true);
            seed_visible_instance(&state);

            // Hide it, then ask whether a re-store would be refused. The
            // hide is written directly: the point under test is the re-store
            // rule for a REASON, not how that reason came to be recorded.
            force_hidden(&state, reason);
            assert_eq!(stored_reason(&state), reason, "seed hide for {reason}");

            let outcome = super::refuse_if_hidden_from_store(&state, STUDY, INSTANCE, CT);
            assert_eq!(
                outcome.is_some(),
                refused,
                "re-store refusal for reason {reason} must be {refused}"
            );
            if let Some(clarus_ports::stow_response::InstanceOutcome::Failed {
                sop_instance_uid,
                reason: message,
                ..
            }) = outcome
            {
                // A per-instance failure names the instance and the reason, so
                // an operator can tell a policy refusal from a storage error.
                assert_eq!(sop_instance_uid.as_deref(), Some(INSTANCE));
                assert!(
                    message.contains(&reason.to_string()),
                    "the failure must name the reason: {message}"
                );
            }
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// A visible instance is always storable.
    #[test]
    fn a_visible_instance_is_never_refused() {
        let root = unique_root("visible");
        let state = build_state(&root, true);
        seed_visible_instance(&state);

        assert!(
            super::refuse_if_hidden_from_store(&state, STUDY, INSTANCE, CT).is_none(),
            "an instance with no rejection reason is ordinary traffic"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// The batch accumulator's bookkeeping across a mid-request flush.
///
/// A single study whose instances cross `max_batch_instances` is written
/// twice; the follow-up work (CAD workitem, durability sync) reads the study
/// list from the batch AFTER the request, so everything it needs must survive
/// the flush. A bare UID list lost the study's attributes and modality set at
/// exactly that point.
#[cfg(test)]
mod manifest_batch_tests {
    use super::{FlushedStudy, ManifestBatch, StudyBatch, carry_flushed_facts};
    use clarus_core::stow::StudyManifestAttrs;

    const STUDY: &str = "1.2.3";

    fn attrs(modality: &str) -> StudyManifestAttrs {
        StudyManifestAttrs {
            patient_name: Some("Doe^Jane".to_string()),
            patient_id: Some("PID-1".to_string()),
            patient_id_issuer: Some("ISSUER".to_string()),
            modality: modality.to_string(),
            study_instance_uid: STUDY.to_string(),
            ..Default::default()
        }
    }

    fn live_study(modalities: &[&str]) -> StudyBatch {
        StudyBatch {
            attrs: attrs(modalities.first().copied().unwrap_or("")),
            series: Vec::new(),
            modalities: modalities.iter().map(|m| (*m).to_string()).collect(),
        }
    }

    /// A study whose instances cross the memory guard is written twice. The
    /// follow-up runs after the request, so its identity and modality set must
    /// outlive the flush - and instances on the far side of the guard must not
    /// erase what the first flush recorded.
    #[test]
    fn a_study_flushed_twice_keeps_one_entry_and_the_whole_modality_set() {
        let mut batch = ManifestBatch::default();

        // First half of the request: DX accepted, guard fires, flush, clear.
        batch.studies.insert(STUDY.to_string(), live_study(&["DX"]));
        carry_flushed_facts(&mut batch);
        batch.clear();

        // Second half: SR accepted for the SAME study, guard fires again.
        batch.studies.insert(STUDY.to_string(), live_study(&["SR"]));
        carry_flushed_facts(&mut batch);
        batch.clear();

        let uids: Vec<&str> = batch.study_uids().collect();
        assert_eq!(
            uids,
            vec![STUDY],
            "one entry per study, not one per flush: {uids:?}"
        );

        let (patient_id, issuer) = batch
            .patient_identity(STUDY)
            .expect("a flushed study must still name its patient");
        assert_eq!(patient_id, "PID-1", "identity survives the flush");
        assert_eq!(issuer, Some("ISSUER"));

        assert_eq!(
            batch.modalities(STUDY),
            vec!["DX".to_string(), "SR".to_string()],
            "the set spans both halves of the request"
        );
    }

    /// A study still pending and the same study already flushed in one request
    /// (its instances straddled the guard) is listed once.
    #[test]
    fn a_pending_study_does_not_duplicate_a_flushed_one() {
        let mut batch = ManifestBatch::default();
        batch.flushed_studies.insert(
            STUDY.to_string(),
            FlushedStudy {
                attrs: attrs("DX"),
                modalities: ["DX".to_string()].into_iter().collect(),
            },
        );
        batch.studies.insert(STUDY.to_string(), live_study(&["SR"]));

        let uids: Vec<&str> = batch.study_uids().collect();
        assert_eq!(uids, vec![STUDY], "listed once: {uids:?}");
    }

    /// The state the batch is really read in at the end of a request whose
    /// guard fired: the study that crossed it is in `flushed_studies`, and the
    /// instances accepted afterwards are still in `studies`. Reading one
    /// container would report half the study.
    #[test]
    fn the_modality_set_of_a_study_present_in_both_half_flushed_is_the_union() {
        let mut batch = ManifestBatch::default();
        batch.flushed_studies.insert(
            STUDY.to_string(),
            FlushedStudy {
                attrs: attrs("DX"),
                modalities: ["DX".to_string()].into_iter().collect(),
            },
        );
        batch.studies.insert(STUDY.to_string(), live_study(&["SR"]));

        assert_eq!(
            batch.modalities(STUDY),
            vec!["DX".to_string(), "SR".to_string()],
            "the flushed half and the live half are both part of the study"
        );
    }

    /// The workitem trigger names the patient. Before the facts were carried,
    /// a study that crossed the guard was still listed by `study_uids()` but
    /// answered `None` from `patient_identity` - the trigger then had the study
    /// and not the patient, so the mint could not state the requester.
    #[test]
    fn a_flushed_study_still_answers_the_patient_identity() {
        let mut batch = ManifestBatch::default();
        batch.studies.insert(STUDY.to_string(), live_study(&["DX"]));

        carry_flushed_facts(&mut batch);
        batch.clear();
        assert!(
            batch.studies.is_empty(),
            "precondition: the study is out of the live map"
        );

        let (patient_id, issuer) = batch
            .patient_identity(STUDY)
            .expect("the follow-up must still reach the patient of a flushed study");
        assert_eq!(patient_id, "PID-1");
        assert_eq!(issuer, Some("ISSUER"));
        assert_eq!(
            batch.modality(STUDY),
            Some("DX"),
            "the single-modality accessor reads flushed studies too"
        );
    }
}
