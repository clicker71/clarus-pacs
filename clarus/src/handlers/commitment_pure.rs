// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DICOMweb Storage Commitment, PS3.18 Section 13 - pure synchronous core.
//!
//! Two transactions on one resource path (Table 13.3-1):
//! `POST /dicomweb/commitment-requests/{TransactionUID}` (Commit, 13.4) and
//! `GET` on the same path (Check Commit Result, 13.5). The Transaction UID
//! lives in the resource path and nowhere else (13.4.1.1); neither request
//! carries query parameters (13.4.1.2, 13.5.1.2).
//!
//! # What the commitment is
//!
//! PS3.4 J.1.1 defines storage commitment as the promise "that the SOP
//! Instances will be kept for an implementation specific period of time and
//! can be retrieved", and adds the binding condition that "the possession of a
//! link to access pixel data shall not be sufficient ... a copy of the entire
//! pixel data is required". For this archive the verifiable form of that
//! condition is presence in the study manifest: STOW answers 200 only after
//! the manifests are durable, and the manifest's instance table embeds the CAS
//! frame-chunk table, so an instance in the manifest means the full pixel data
//! is in CAS. That is exactly what [`evaluate_instances`] checks.
//!
//! # Deliberate choices
//!
//! - **Synchronous Done (P1).** Commit answers 200 with the complete result.
//!   202 is never sent. 13.4.2 leaves the choice of "either returns a 200 ...
//!   or a 202" to the origin, and an all-failed result is still a 200 - see
//!   the Note in 13.4.2.
//! - **No 410 tombstone (P4).** After the retention sweep deletes a result,
//!   Check answers 404, which Table 13.5.3-1 explicitly allows for "the
//!   origin server may have deleted the applicable result". 410 is optional.
//! - **Extra query parameters are refused (400).** The standard defines no
//!   query parameters for either transaction, so there is no silent-ignore
//!   case to preserve; refusing matches the house rule of an honest failure
//!   over a quiet reinterpretation.
//! - **A duplicated SOP Instance UID in one request is refused (400).**
//!   PS3.4 J.3.2.1.1.3 states "a SOP Instance may be referenced only once
//!   within the Referenced SOP Sequence" for the DIMSE Action Information.
//!   Annex J.1 does not repeat it, but the semantics are declared to be the
//!   same service class (13.1), so the DIMSE rule is the reading we enforce.
//!
//! This module is fully synchronous: no `axum::`, no `async`, no `tokio`.

#![forbid(unsafe_code)]

use clarus_core::commitment::record::{
    CommitmentOutcome, CommitmentRecord, FailureReason, InstanceResult, RequestForm,
};
use clarus_core::commitment::store::{CommitmentStore, StoreError};

use crate::state::AppState;

use super::json_common::{JsonVal, err_msg_json, parse_json, push_json_str};

// -- Wire constants -------------------------------------------------------

/// Referenced SOP Sequence - the committed instances (Annex J.2).
const T_REFERENCED_SOP_SEQUENCE: &str = "00081199";
/// Failed SOP Sequence - the instances that were not committed (Annex J.2).
const T_FAILED_SOP_SEQUENCE: &str = "00081198";
/// Failure Reason, PS3.3 C.14.1.1 (Annex J.2, type 1 inside Failed SOP).
const T_FAILURE_REASON: &str = "00081197";
/// Referenced Study Sequence - the study form of a request and a result.
const T_REFERENCED_STUDY_SEQUENCE: &str = "00081110";
/// Failed Study Sequence - the study form of a failed result (Annex J.2).
const T_FAILED_STUDY_SEQUENCE: &str = "0008119B";
/// Referenced Series Sequence (study form, Annex J.1/J.2).
const T_REFERENCED_SERIES_SEQUENCE: &str = "00081115";
/// Referenced Instances by SOP Class Sequence (study form).
const T_REFERENCED_INSTANCES_BY_SOP_CLASS_SEQUENCE: &str = "00081112";
/// Referenced Instance Sequence (study form, innermost).
const T_REFERENCED_INSTANCE_SEQUENCE: &str = "0008114A";
/// Referenced SOP Class UID - the SOP Instance Reference Macro.
const T_REFERENCED_SOP_CLASS_UID: &str = "00081150";
/// Referenced SOP Instance UID - the SOP Instance Reference Macro.
const T_REFERENCED_SOP_INSTANCE_UID: &str = "00081155";

/// A six-hex-digit CBOR fault is impossible here; the tag names above are the
/// only keys Annex J.1/J.2 define. Anything else in a request payload is an
/// unknown key and is refused, per the strict-parser doctrine: `0088,0130`,
/// `0088,0140` and `0008,0054` belong to the DIMSE Action Information
/// (PS3.4 Table J.3-1), not to the DICOMweb module.
const REQUEST_KEYS: [&str; 5] = [
    T_REFERENCED_SOP_SEQUENCE,
    T_REFERENCED_STUDY_SEQUENCE,
    "00080005", // SpecificCharacterSet - permitted by 8.7.3 for any dataset.
    "vr",       // tolerated: some serializers echo a "vr" key at top level.
    "00100010", // PatientName - Annex J.1 has no PN, but 8.7.3 allows the
                // request's own patient context; accepted and ignored.
];

// -- Response types -------------------------------------------------------

/// A fully-rendered Storage Commitment response.
///
/// `content_type` is a `String` rather than `&'static str` because a
/// multipart/related answer carries a per-response boundary
/// (Table 13.1.3-1 marks both multipart forms Required at service level).
/// `headers` carries the extra headers the wrapper copies verbatim (the
/// `Warning` header used to report a DICOM status code, 13.4.3.2).
pub struct CommitmentResp {
    pub status: u16,
    pub content_type: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

/// A fully-rendered Storage Commitment error: status + `{"error":...}` body.
#[derive(Debug)]
pub struct CommitmentErr {
    pub status: u16,
    pub body: Vec<u8>,
}

fn bad_request(message: &str) -> CommitmentErr {
    CommitmentErr {
        status: 400,
        body: err_msg_json("Bad Request", message),
    }
}

fn conflict(message: &str) -> CommitmentErr {
    CommitmentErr {
        status: 409,
        body: err_msg_json("Conflict", message),
    }
}

fn not_found(message: &str) -> CommitmentErr {
    CommitmentErr {
        status: 404,
        body: err_msg_json("Not Found", message),
    }
}

fn service_unavailable(message: &str) -> CommitmentErr {
    CommitmentErr {
        status: 503,
        body: err_msg_json("Service Unavailable", message),
    }
}

fn internal_err(message: &str) -> CommitmentErr {
    CommitmentErr {
        status: 500,
        body: err_msg_json("Internal Server Error", message),
    }
}

// -- Accept negotiation ---------------------------------------------------

/// The media types this service answers in (Table 13.1.3-1).
///
/// `application/dicom+json` is the Default and `application/dicom+xml` is
/// Required; both multipart/related forms are Required at SERVICE level, so
/// even though neither payload here is naturally a list, a conforming origin
/// must accept those Accept headers and answer with a one-part
/// multipart/related body (8.7.3.2). An unrecognised Accept falls back to the
/// Default rather than a 406: 8.5 makes "unsupported media type" conditional
/// on the request naming a media type the server cannot produce, and the
/// client here is a machine whose next step on 406 would be to retry.
enum AcceptKind {
    Json,
    Xml,
    MultipartJson,
    MultipartXml,
}

fn classify_accept(accept: Option<&str>) -> AcceptKind {
    let Some(a) = accept else {
        return AcceptKind::Json;
    };
    let lower = a.to_ascii_lowercase();
    // `*/*` and `application/*` mean "anything", which is the Default.
    if lower.contains("multipart/related") {
        if lower.contains("application/dicom+xml") {
            return AcceptKind::MultipartXml;
        }
        return AcceptKind::MultipartJson;
    }
    if lower.contains("application/dicom+xml") {
        return AcceptKind::Xml;
    }
    AcceptKind::Json
}

/// A boundary that is unique per response.
///
/// The value only has to be absent from the body; the Transaction UID plus a
/// process-unique counter gives that for the small DICOM JSON payloads here
/// without pulling in a random source.
fn boundary_for(transaction_uid: &str, seq: u64) -> String {
    let sanitized: String = transaction_uid
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("clarus-commitment-{sanitized}-{seq}")
}

/// Wrap a single-part body in `multipart/related` (8.7.3.2).
///
/// The transferred payload is one dataset, so the multipart body has exactly
/// one part. The `Content-Type` of that part is the negotiated concrete type.
fn wrap_multipart(boundary: &str, part_type: &str, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + boundary.len() * 2 + 128);
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(format!("Content-Type: {part_type}\r\n\r\n").as_bytes());
    out.extend_from_slice(body);
    // The closing delimiter is preceded by CRLF; a one-part multipart has no
    // intervening CRLF before it, matching the WADO writers' layout.
    out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    out
}

fn finish(kind: AcceptKind, transaction_uid: &str, json: Vec<u8>, rel: u64) -> CommitmentResp {
    // The XML rendering can refuse a body (PS3.19 conversion has no answer for
    // some shapes). Answering 200 with `application/dicom+xml` and zero bytes
    // tells the client the response IS an empty document, which is a lie about
    // a successful request; the same conversion in `dispatch.rs:1615` answers
    // 500, and one of the two had to change.
    let render_xml = |json: &[u8]| match super::instance_xml::json_metadata_to_xml(json) {
        Ok(xml) => Some(xml),
        Err(e) => {
            crate::clog_warn!(error = %e, "commitment: DICOM XML rendering failed");
            None
        }
    };
    match kind {
        AcceptKind::Json => CommitmentResp {
            status: 200,
            content_type: "application/dicom+json; charset=utf-8".to_string(),
            headers: Vec::new(),
            body: json,
        },
        AcceptKind::Xml => {
            let Some(xml) = render_xml(&json) else {
                return internal_error();
            };
            CommitmentResp {
                status: 200,
                content_type: "application/dicom+xml".to_string(),
                headers: Vec::new(),
                body: xml,
            }
        }
        AcceptKind::MultipartJson | AcceptKind::MultipartXml => {
            let xml = matches!(kind, AcceptKind::MultipartXml);
            let (inner_type, part) = if xml {
                let Some(rendered) = render_xml(&json) else {
                    return internal_error();
                };
                ("application/dicom+xml", rendered)
            } else {
                ("application/dicom+json", json)
            };
            let boundary = boundary_for(transaction_uid, rel);
            let body = wrap_multipart(&boundary, inner_type, &part);
            CommitmentResp {
                status: 200,
                content_type: format!(
                    "multipart/related; type=\"{inner_type}\"; boundary={boundary}"
                ),
                headers: Vec::new(),
                body,
            }
        }
    }
}

/// The 500 the XML conversion failure answers, shaped like every other
/// Storage Commitment error body so the client has one error format to read.
fn internal_error() -> CommitmentResp {
    let mut json = Vec::from(&b"{\"error\":"[..]);
    push_json_str(&mut json, "Internal Server Error");
    json.push(b'}');
    CommitmentResp {
        status: 500,
        content_type: "application/dicom+json; charset=utf-8".to_string(),
        headers: Vec::new(),
        body: json,
    }
}

// -- Request parsing (Annex J.1) ------------------------------------------

/// The parsed request: either a list of SOP instances, or studies of them.
struct CommitmentRequest {
    form: RequestForm,
    /// `(sop_class_uid, sop_instance_uid, study_uid)` in request order.
    /// `study_uid` is `None` for the instance form (Annex J.1 carries no
    /// study reference there, so it is not invented).
    flattened: Vec<(String, String, Option<String>)>,
}

/// Read a DICOM JSON `Value` array of one tag as a list of objects.
fn tag_sequence_items<'a>(obj: &'a JsonVal, tag: &str) -> Option<&'a Vec<JsonVal>> {
    obj.obj_get(tag)?.obj_get("Value")?.as_array()
}

/// Extract the single string value of one tag from a DICOM JSON item.
fn tag_single_string(item: &JsonVal, tag: &str) -> Option<String> {
    let v = item.obj_get(tag)?.obj_get("Value")?;
    let arr = v.as_array()?;
    let first = arr.first()?;
    first.as_str().map(str::to_string)
}

/// Collect an instance reference (`0008,1150` + `0008,1155`) from one item.
fn read_instance_ref(
    item: &JsonVal,
    study_uid: Option<&str>,
) -> Result<(String, String, Option<String>), CommitmentErr> {
    let sop_class = tag_single_string(item, T_REFERENCED_SOP_CLASS_UID)
        .ok_or_else(|| bad_request("Referenced SOP Class UID (0008,1150) is missing"))?;
    let sop_instance = tag_single_string(item, T_REFERENCED_SOP_INSTANCE_UID)
        .ok_or_else(|| bad_request("Referenced SOP Instance UID (0008,1155) is missing"))?;
    if !is_valid_uid(&sop_class) {
        return Err(bad_request(
            "Referenced SOP Class UID (0008,1150) is not a UID",
        ));
    }
    if !is_valid_uid(&sop_instance) {
        return Err(bad_request(
            "Referenced SOP Instance UID (0008,1155) is not a UID",
        ));
    }
    Ok((sop_class, sop_instance, study_uid.map(str::to_string)))
}

/// A DICOM UID: non-empty and `[0-9.]` only, at most 64 characters.
///
/// The same shape the router enforces on path UIDs, repeated here because
/// these values arrive in the payload and are later written into a CBOR record.
fn is_valid_uid(uid: &str) -> bool {
    !uid.is_empty()
        && uid.len() <= 64
        && uid.chars().all(|c| c.is_ascii_digit() || c == '.')
        && !uid.starts_with('.')
        && !uid.ends_with('.')
}

/// Parse the Storage Commitment Request Module (Annex J.1, Table J.1-1).
///
/// The module is `0008,1199` (instance form) OR `0008,1110` (study form),
/// marked 1C in both directions, so exactly one of the two must be present.
fn parse_request(body: &[u8]) -> Result<CommitmentRequest, CommitmentErr> {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Err(bad_request(
            "An empty request body carries no Storage Commitment Request Module",
        ));
    }
    let root = parse_json(body)
        .ok_or_else(|| bad_request("Request body is not well-formed DICOM JSON"))?;

    // A single-dataset request body has two conformant spellings: the bare
    // object (PS3.18 F.2.2) and an array of one (F.2.1, which is what
    // dcm4chee-arc accepts and what a strict client sends). Anything else
    // names no dataset and keeps the 400.
    let root = super::ups_pure::single_dataset(&root)
        .ok_or_else(|| bad_request("Request body must be a DICOM JSON object"))?;

    // Unknown top-level keys are refused: Annex J.1 is a closed module table
    // and the DIMSE-only attributes must not be silently accepted.
    if let JsonVal::Obj(pairs) = root {
        for (k, _) in pairs {
            if !REQUEST_KEYS.contains(&k.as_str()) {
                return Err(bad_request(
                    "Request body carries a key that is not part of the Storage Commitment Request Module",
                ));
            }
        }
    } else {
        return Err(bad_request("Request body must be a DICOM JSON object"));
    }

    let sop_seq = tag_sequence_items(root, T_REFERENCED_SOP_SEQUENCE);
    let study_seq = tag_sequence_items(root, T_REFERENCED_STUDY_SEQUENCE);

    match (sop_seq, study_seq) {
        (Some(_), Some(_)) => Err(bad_request(
            "Referenced SOP Sequence (0008,1199) and Referenced Study Sequence (0008,1110) are mutually exclusive (1C)",
        )),
        (None, None) => Err(bad_request(
            "Request must carry Referenced SOP Sequence (0008,1199) or Referenced Study Sequence (0008,1110)",
        )),
        (Some(items), None) => {
            if items.is_empty() {
                return Err(bad_request("Referenced SOP Sequence (0008,1199) is empty"));
            }
            let mut flattened = Vec::with_capacity(items.len());
            for item in items {
                flattened.push(read_instance_ref(item, None)?);
            }
            Ok(CommitmentRequest {
                form: RequestForm::Instance,
                flattened,
            })
        }
        (None, Some(studies)) => {
            if studies.is_empty() {
                return Err(bad_request(
                    "Referenced Study Sequence (0008,1110) is empty",
                ));
            }
            let mut flattened = Vec::new();
            for study in studies {
                let study_uid = tag_single_string(study, "0020000D")
                    .or_else(|| tag_single_string(study, "00081155"))
                    .ok_or_else(|| {
                        bad_request(
                            "StudyInstanceUID is missing from the Referenced Study Sequence item",
                        )
                    })?;
                if !is_valid_uid(&study_uid) {
                    return Err(bad_request(
                        "StudyInstanceUID in the Referenced Study Sequence is not a UID",
                    ));
                }
                let series =
                    tag_sequence_items(study, T_REFERENCED_SERIES_SEQUENCE).ok_or_else(|| {
                        bad_request("Referenced Series Sequence (0008,1115) is missing")
                    })?;
                for s in series {
                    let by_class = tag_sequence_items(
                        s,
                        T_REFERENCED_INSTANCES_BY_SOP_CLASS_SEQUENCE,
                    )
                    .ok_or_else(|| {
                        bad_request(
                            "Referenced Instances by SOP Class Sequence (0008,1112) is missing",
                        )
                    })?;
                    for class_item in by_class {
                        let sop_class = tag_single_string(class_item, T_REFERENCED_SOP_CLASS_UID)
                            .ok_or_else(|| {
                            bad_request("Referenced SOP Class UID (0008,1150) is missing")
                        })?;
                        if !is_valid_uid(&sop_class) {
                            return Err(bad_request(
                                "Referenced SOP Class UID (0008,1150) is not a UID",
                            ));
                        }
                        let instances =
                            tag_sequence_items(class_item, T_REFERENCED_INSTANCE_SEQUENCE)
                                .ok_or_else(|| {
                                    bad_request(
                                        "Referenced Instance Sequence (0008,114A) is missing",
                                    )
                                })?;
                        for inst in instances {
                            let sop_instance = tag_single_string(
                                inst,
                                T_REFERENCED_SOP_INSTANCE_UID,
                            )
                            .ok_or_else(|| {
                                bad_request("Referenced SOP Instance UID (0008,1155) is missing")
                            })?;
                            if !is_valid_uid(&sop_instance) {
                                return Err(bad_request(
                                    "Referenced SOP Instance UID (0008,1155) is not a UID",
                                ));
                            }
                            flattened.push((
                                sop_class.clone(),
                                sop_instance,
                                Some(study_uid.clone()),
                            ));
                        }
                    }
                }
            }
            if flattened.is_empty() {
                return Err(bad_request(
                    "The Referenced Study Sequence resolved to no SOP Instances",
                ));
            }
            Ok(CommitmentRequest {
                form: RequestForm::Study,
                flattened,
            })
        }
    }
}

/// Refuse a request that references the same SOP Instance UID twice.
///
/// PS3.4 J.3.2.1.1.3: "A SOP Instance may be referenced only once within the
/// Referenced SOP Sequence". The DIMSE module states the rule the service
/// semantics inherit (13.1 declares the same service class).
fn reject_duplicate_instances(
    flattened: &[(String, String, Option<String>)],
) -> Result<(), CommitmentErr> {
    // A hash set, not a nested scan: the sequence length is caller-controlled,
    // so the pairwise version was quadratic in a field a client fills.
    let mut seen: std::collections::HashSet<&str> =
        std::collections::HashSet::with_capacity(flattened.len());
    for item in flattened {
        if !seen.insert(item.1.as_str()) {
            return Err(bad_request(
                "A SOP Instance may be referenced only once within one Storage Commitment request",
            ));
        }
    }
    Ok(())
}

// -- Archive evaluation ---------------------------------------------------

/// What the archive knows about one referenced SOP Instance.
///
/// The instance form of the request (Annex J.1) carries no study reference, so
/// a match is also the only moment the study becomes known. Returning the
/// study UID here rather than re-scanning later is what lets `commit` group
/// the committed set by study without a second directory walk: the walk
/// already had to find the manifest to vouch for the instance.
pub struct InstancePresence {
    /// Whether the archive holds the instance with its pixel data.
    present: bool,
    /// (0020,000D) of the manifest that vouched for it, when one did.
    study_uid: Option<String>,
}

/// Does the archive hold this SOP Instance with its full pixel data?
///
/// Presence in the study manifest IS the J.1.1 condition: the manifest is the
/// durable per-study record written before STOW's 200, and its instance table
/// embeds the CAS frame-chunk table, so a manifest row means the pixel data is
/// in CAS. The study UID is not part of the instance-form request (Annex J.1
/// carries no study reference there), so the search walks the manifests and
/// stops at the first match.
///
/// The returned [`InstancePresence`] carries the study the instance was found
/// under, which the caller needs to group a commit by study. A study-shaped
/// request supplies the study itself, so the answer is that UID whenever the
/// instance is present.
/// Decoded manifests for one request, keyed by study UID.
///
/// A commit names many instances of the same few studies (a study-shaped
/// request names ONE), and `promote_committed_studies` re-reads the same
/// manifests afterwards. Decoding a multi-megabyte manifest once per
/// instance is pure waste on the synchronous request thread.
#[derive(Default)]
pub struct ManifestCache {
    by_study: std::collections::HashMap<String, Option<clarus_core::manifest::Manifest>>,
}

impl ManifestCache {
    /// The manifest of `study_uid`, decoded at most once per request.
    ///
    /// `None` is cached too: a study named by a hint but absent from the
    /// archive would otherwise be probed once per instance.
    fn get(
        &mut self,
        manifest_dir: &std::path::Path,
        study_uid: &str,
    ) -> Option<&clarus_core::manifest::Manifest> {
        self.by_study
            .entry(study_uid.to_string())
            .or_insert_with(|| {
                let path = manifest_dir.join(format!("{study_uid}.manifest"));
                clarus_core::manifest::read_manifest(&path).ok()
            })
            .as_ref()
    }
}

/// Resolve several SOP Instance UIDs against the archive in ONE pass.
///
/// The index answers the common case per instance. Whatever it cannot answer
/// is collected first and then resolved by a SINGLE directory scan that
/// decodes each manifest once and checks every still-unresolved UID against
/// it. Doing the scan per instance made a commit with N unknown UIDs cost N
/// full decodes of the archive - the pathological case a client can trigger
/// by naming instances that do not exist.
///
/// Returns one presence per input UID, in input order.
pub fn evaluate_instances(
    state: &AppState,
    manifest_dir: &std::path::Path,
    uids: &[(&str, Option<&str>)],
) -> Result<Vec<InstancePresence>, CommitmentErr> {
    let mut out: Vec<InstancePresence> = Vec::with_capacity(uids.len());
    // UIDs the index (or the study hint) could not place, with their input
    // positions so the answers land back in order.
    let mut pending: Vec<(usize, &str)> = Vec::new();
    let mut cache = ManifestCache::default();

    for (index, (sop, hint)) in uids.iter().enumerate() {
        let resolved = match hint {
            // A study-shaped request names the study: one probe, no scan.
            Some(study_uid) => cache
                .get(manifest_dir, study_uid)
                .filter(|m| m.find_instance(sop).is_some())
                .map(|m| m.study_instance_uid().unwrap_or(study_uid).to_string()),
            // The index names the owning study in the common case.
            None => state.shadow.sop_owner_study(sop).and_then(|owner| {
                cache
                    .get(manifest_dir, &owner)
                    .filter(|m| m.find_instance(sop).is_some())
                    .map(|m| m.study_instance_uid().unwrap_or(&owner).to_string())
            }),
        };
        match resolved {
            Some(study_uid) => out.push(InstancePresence {
                present: true,
                study_uid: Some(study_uid),
            }),
            None => {
                out.push(InstancePresence {
                    present: false,
                    study_uid: None,
                });
                pending.push((index, *sop));
            }
        }
    }

    if pending.is_empty() {
        return Ok(out);
    }

    let entries = match std::fs::read_dir(manifest_dir) {
        Ok(entries) => entries,
        // A fresh archive with no manifest directory holds nothing: every
        // pending UID is honestly absent rather than a server error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => {
            return Err(internal_err(&format!(
                "manifest directory is unreadable: {e}"
            )));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|_| internal_err("manifest directory listing failed"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name.strip_suffix(".manifest") else {
            continue;
        };
        let path = manifest_dir.join(format!("{stem}.manifest"));
        let Ok(m) = clarus_core::manifest::read_manifest(&path) else {
            // A single unreadable manifest must not fail the whole check; it
            // simply cannot vouch for any instance.
            continue;
        };
        // The manifest's OWN study UID, not the file stem: the stem is a
        // name, and grouping a commit by a name would silently mis-file a
        // manifest whose name disagreed with its content.
        let owner = m.study_instance_uid().unwrap_or(stem).to_string();
        let mut still_pending = Vec::with_capacity(pending.len());
        for (index, sop) in pending {
            if m.find_instance(sop).is_some() {
                out[index] = InstancePresence {
                    present: true,
                    study_uid: Some(owner.clone()),
                };
            } else {
                still_pending.push((index, sop));
            }
        }
        pending = still_pending;
        if pending.is_empty() {
            break;
        }
    }
    Ok(out)
}

/// Promote every study this transaction fully covers, or re-arm its debounce.
///
/// The rule is the owner's "Storage Commitment = I sent everything", read
/// strictly: a study is promoted only when the committed set of THIS
/// transaction covers every instance the study's manifest currently lists.
/// The archive cannot know about instances it has not received, so the
/// manifest is the complete received set by definition; a commit that covers
/// part of it means the sender is still going, and the study stays INCOMPLETE
/// with its quiet period re-armed.
///
/// A study whose manifest cannot be read, or that has no instances, is left
/// alone: nothing can be concluded about coverage, and the debounce still
/// covers it.
fn promote_committed_studies(
    state: &AppState,
    record: &CommitmentRecord,
    manifest_dir: &std::path::Path,
) {
    let mut committed: std::collections::HashMap<&str, std::collections::HashSet<&str>> =
        std::collections::HashMap::new();
    for instance in &record.instances {
        if instance.outcome != CommitmentOutcome::Committed {
            continue;
        }
        let Some(study_uid) = instance.study_uid.as_deref() else {
            // An instance-form failure cannot happen here (a committed
            // instance always found a manifest, and finding one supplies the
            // study), but a record read back from an older store might carry
            // none. Skipping is the safe reading: no study, no promotion.
            continue;
        };
        committed
            .entry(study_uid)
            .or_default()
            .insert(instance.sop_instance_uid.as_str());
    }

    for (study_uid, instance_uids) in committed {
        let path = manifest_dir.join(format!("{study_uid}.manifest"));
        let Ok(manifest) = clarus_core::manifest::read_manifest(&path) else {
            continue;
        };
        let stored = manifest.sop_instance_uids();
        if stored.is_empty() {
            continue;
        }
        let covered = stored.iter().all(|uid| instance_uids.contains(uid));
        if covered {
            super::readiness::promote(state, study_uid);
        } else {
            // Partial: the sender is still going. Re-arm so the quiet period
            // restarts from this commit rather than from the last STOW - the
            // commit is activity, and a study must not be declared READY on
            // the strength of a transaction that admitted it is incomplete.
            state.readiness.arm(study_uid, std::time::Instant::now());
        }
    }
}

/// The manifest root this store is configured with.
fn manifest_dir(state: &AppState) -> std::path::PathBuf {
    state.study_scanner.manifests_dir.clone()
}

// -- Rendering (Annex J.2) ------------------------------------------------

/// Write one item of the SOP Instance Reference Macro.
///
/// The item carries `(0008,1150)` and `(0008,1155)` (PS3.3 Table 10-11), and
/// `(0008,1197)` when the instance failed. Every attribute object opened by
/// [`write_tag_key`] must be closed with `}` - a missing one yields a body
/// that still contains every expected tag string but is not JSON.
fn write_instance_ref(
    out: &mut Vec<u8>,
    sop_class: &str,
    sop_instance: &str,
    failure: Option<FailureReason>,
) {
    out.extend_from_slice(b"{");
    write_tag_key(out, T_REFERENCED_SOP_CLASS_UID, "UI");
    out.push(b'[');
    push_json_str(out, sop_class);
    out.extend_from_slice(b"]},");
    write_tag_key(out, T_REFERENCED_SOP_INSTANCE_UID, "UI");
    out.push(b'[');
    push_json_str(out, sop_instance);
    out.extend_from_slice(b"]}");
    if let Some(reason) = failure {
        out.push(b',');
        write_tag_key(out, T_FAILURE_REASON, "US");
        out.push(b'[');
        // DICOM JSON carries every value as a string, including numeric VRs
        // (8.7.3): the failure reason is `"0112"`, not `0112`.
        push_json_str(out, reason.code());
        out.extend_from_slice(b"]}");
    }
    out.extend_from_slice(b"}");
}

/// Write `"TAG":{"vr":"VR","Value":` - the DICOM JSON attribute prefix.
fn write_tag_key(out: &mut Vec<u8>, tag: &str, vr: &str) {
    out.push(b'"');
    out.extend_from_slice(tag.as_bytes());
    out.extend_from_slice(b"\":{\"vr\":\"");
    out.extend_from_slice(vr.as_bytes());
    out.extend_from_slice(b"\",\"Value\":");
}

/// Render the committed / failed sequences for the instance form.
fn render_instance_form(out: &mut Vec<u8>, results: &[InstanceResult]) {
    let committed: Vec<&InstanceResult> = results
        .iter()
        .filter(|r| r.outcome == CommitmentOutcome::Committed)
        .collect();
    let failed: Vec<&InstanceResult> = results
        .iter()
        .filter(|r| r.outcome == CommitmentOutcome::Failed)
        .collect();

    out.extend_from_slice(b"{");
    let mut need_comma = false;
    if !committed.is_empty() {
        write_tag_key(out, T_REFERENCED_SOP_SEQUENCE, "SQ");
        out.push(b'[');
        for (i, r) in committed.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            write_instance_ref(out, &r.sop_class_uid, &r.sop_instance_uid, None);
        }
        out.extend_from_slice(b"]}");
        need_comma = true;
    }
    if !failed.is_empty() {
        if need_comma {
            out.push(b',');
        }
        write_tag_key(out, T_FAILED_SOP_SEQUENCE, "SQ");
        out.push(b'[');
        for (i, r) in failed.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            write_instance_ref(out, &r.sop_class_uid, &r.sop_instance_uid, r.failure_reason);
        }
        out.extend_from_slice(b"]}");
    }
    out.extend_from_slice(b"}");
}

/// Render the study form: Referenced Study Sequence + Failed Study Sequence.
fn render_study_form(out: &mut Vec<u8>, results: &[InstanceResult]) {
    // Group by the study each instance was requested under. A study is
    // committed only when every instance under it was committed, which is the
    // reading Table J.2-1 implies: the study sequence lists the studies whose
    // referenced instances were all committed, and the failed study sequence
    // carries the rest with the per-instance failure reason nested.
    let mut by_study: Vec<(&str, Vec<&InstanceResult>)> = Vec::new();
    for r in results {
        let uid = r.study_uid.as_deref().unwrap_or("");
        if let Some(slot) = by_study.iter_mut().find(|(u, _)| *u == uid) {
            slot.1.push(r);
        } else {
            by_study.push((uid, vec![r]));
        }
    }

    // A study is reported as committed only when EVERY instance requested
    // under it was committed. A study with any failure goes into the Failed
    // Study Sequence carrying ONLY its failed instances: the failed sequence
    // is the list of what was not committed, so a committed instance under a
    // partly-failed study has no place in it (it is already accounted for by
    // the study-level answer, and listing it as failed would be a false
    // statement about that instance).
    let mut committed: Vec<&str> = Vec::new();
    let mut failed: Vec<(&str, Vec<&InstanceResult>)> = Vec::new();
    for (uid, items) in &by_study {
        if items
            .iter()
            .all(|r| r.outcome == CommitmentOutcome::Committed)
        {
            committed.push(uid);
        } else {
            let failures: Vec<&InstanceResult> = items
                .iter()
                .copied()
                .filter(|r| r.outcome == CommitmentOutcome::Failed)
                .collect();
            failed.push((uid, failures));
        }
    }

    out.extend_from_slice(b"{");
    let mut need_comma = false;
    if !committed.is_empty() {
        write_tag_key(out, T_REFERENCED_STUDY_SEQUENCE, "SQ");
        out.push(b'[');
        for (i, uid) in committed.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.extend_from_slice(b"{\"0020000D\":{\"vr\":\"UI\",\"Value\":[");
            push_json_str(out, uid);
            out.extend_from_slice(b"]}}");
        }
        out.extend_from_slice(b"]}");
        need_comma = true;
    }
    if !failed.is_empty() {
        if need_comma {
            out.push(b',');
        }
        write_tag_key(out, T_FAILED_STUDY_SEQUENCE, "SQ");
        out.push(b'[');
        for (i, (uid, items)) in failed.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.extend_from_slice(b"{\"0020000D\":{\"vr\":\"UI\",\"Value\":[");
            push_json_str(out, uid);
            out.extend_from_slice(b"]},");
            write_tag_key(out, T_FAILED_SOP_SEQUENCE, "SQ");
            out.push(b'[');
            for (j, r) in items.iter().enumerate() {
                if j > 0 {
                    out.push(b',');
                }
                write_instance_ref(out, &r.sop_class_uid, &r.sop_instance_uid, r.failure_reason);
            }
            out.extend_from_slice(b"]}}");
        }
        out.extend_from_slice(b"]}");
    }
    out.extend_from_slice(b"}");
}

/// Render the Storage Commitment Response Module (Annex J.2, Table J.2-1).
fn render_result(record: &CommitmentRecord) -> Vec<u8> {
    let mut out = Vec::with_capacity(128 + record.instances.len() * 192);
    match record.request_form {
        RequestForm::Instance => render_instance_form(&mut out, &record.instances),
        RequestForm::Study => render_study_form(&mut out, &record.instances),
    }
    out
}

// -- Public entry points --------------------------------------------------

/// The commitment store rooted at the configured directory.
fn store(state: &AppState) -> CommitmentStore {
    CommitmentStore::new(state.commitments_dir.clone())
}

fn map_store_error(e: StoreError) -> CommitmentErr {
    match e {
        StoreError::Duplicate(uid) => conflict(&format!(
            "The provided Transaction UID is already in use: {uid}"
        )),
        StoreError::Io(io) => {
            service_unavailable(&format!("commitment store is unavailable: {io}"))
        }
        StoreError::Decode(d) => {
            internal_err(&format!("stored commitment result is unreadable: {d}"))
        }
    }
}

/// `POST /dicomweb/commitment-requests/{TransactionUID}` - Commit (13.4).
///
/// Answers 200 with the complete per-instance result. There is no 202 path:
/// see P1 in the module documentation.
pub fn commit(
    state: &AppState,
    transaction_uid: &str,
    body: &[u8],
    accept: Option<&str>,
    query: Option<&str>,
) -> Result<CommitmentResp, CommitmentErr> {
    if !is_valid_uid(transaction_uid) {
        return Err(bad_request(
            "The Transaction UID in the resource path is not a UID",
        ));
    }
    // 13.4.1.2: "The request has no Query Parameters."
    if let Some(q) = query
        && !q.is_empty()
    {
        return Err(bad_request(
            "The Commit request has no query parameters (PS3.18 13.4.1.2)",
        ));
    }
    let request = parse_request(body)?;
    reject_duplicate_instances(&request.flattened)?;

    let dir = manifest_dir(state);
    // One resolution pass for the whole request: the index answers what it
    // knows and everything else is settled by a SINGLE directory scan. Calling
    // the per-instance form in this loop made N unknown UIDs cost N full
    // decodes of the archive, on the request thread.
    let wanted: Vec<(&str, Option<&str>)> = request
        .flattened
        .iter()
        .map(|(_, sop, study)| (sop.as_str(), study.as_deref()))
        .collect();
    let presences = evaluate_instances(state, &dir, &wanted)?;

    let mut instances = Vec::with_capacity(request.flattened.len());
    for ((sop_class, sop_instance, study_uid), presence) in request.flattened.iter().zip(presences)
    {
        let (outcome, failure_reason) = if presence.present {
            (CommitmentOutcome::Committed, None)
        } else {
            // 0112H "No such SOP Instance": the instance is not available,
            // which is the verifiable fact. 0213H would describe the moment of
            // eviction, which is not what is being checked here.
            (
                CommitmentOutcome::Failed,
                Some(FailureReason::NoSuchSopInstance),
            )
        };
        instances.push(InstanceResult {
            sop_class_uid: sop_class.clone(),
            sop_instance_uid: sop_instance.clone(),
            outcome,
            failure_reason,
            // A study-shaped request NAMES the study, so every result it
            // produces - committed or failed - is reported under it, and the
            // stored record can be grouped back into the study form on Check
            // (Annex J.2) without walking the archive again.
            //
            // The instance form carries no study (Annex J.1), so there the
            // study is whatever the manifest that vouched for the instance
            // declares; a FAILED instance found no manifest and therefore has
            // none, which is also what keeps it out of the coverage
            // comparison below.
            study_uid: study_uid.clone().or(presence.study_uid),
        });
    }

    let record = CommitmentRecord {
        transaction_uid: transaction_uid.to_string(),
        requested_at: now_timestamp(),
        request_form: request.form,
        instances,
    };

    let store = store(state);
    // A duplicate Transaction UID is the 409 of Table 13.4.3-1. The first
    // result is left untouched.
    store.insert(&record).map_err(map_store_error)?;

    // AFTER the record is durable, and before the response is rendered: the
    // archive has accepted this commitment, so any study it fully covers is
    // now ready for a performer. Doing it after `insert` means a 409 (the
    // duplicate path above) promotes nothing, and doing it here rather than
    // in a thread keeps the one publish path (`readiness::promote`) inline
    // with the acceptance that justified it.
    promote_committed_studies(state, &record, &dir);

    let json = render_result(&record);
    let kind = classify_accept(accept);
    Ok(finish(kind, transaction_uid, json, 0))
}

/// `GET /dicomweb/commitment-requests/{TransactionUID}` - Check (13.5).
///
/// 200 with the stored result, or 404 when there is none - including after the
/// retention sweep has removed it, which Table 13.5.3-1 allows. No 410
/// tombstone (P4).
pub fn check(
    state: &AppState,
    transaction_uid: &str,
    accept: Option<&str>,
    query: Option<&str>,
) -> Result<CommitmentResp, CommitmentErr> {
    if !is_valid_uid(transaction_uid) {
        return Err(bad_request(
            "The Transaction UID in the resource path is not a UID",
        ));
    }
    // 13.5.1.2: "The request has no Query Parameters."
    if let Some(q) = query
        && !q.is_empty()
    {
        return Err(bad_request(
            "The Check Commit Result request has no query parameters (PS3.18 13.5.1.2)",
        ));
    }
    let store = store(state);
    let record = store.find(transaction_uid).map_err(map_store_error)?;
    let Some(record) = record else {
        return Err(not_found(
            "No Storage Commitment result is available for this Transaction UID",
        ));
    };
    let json = render_result(&record);
    let kind = classify_accept(accept);
    Ok(finish(kind, transaction_uid, json, 0))
}

/// The record's `requested_at`, in the crate's own ISO-8601 UTC format.
///
/// `crate::time::local_now()` is the single clock the server already uses for
/// every timestamp it writes, and `commitment::store` parses exactly this
/// shape when it applies the retention window.
fn now_timestamp() -> String {
    let now = clarus_core::time::local_now();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year, now.month, now.day, now.hour, now.minute, now.second
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clarus_core::commitment::record::{CommitmentOutcome, RequestForm};

    fn instance(uid: &str, outcome: CommitmentOutcome, study: Option<&str>) -> InstanceResult {
        InstanceResult {
            sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".to_string(),
            sop_instance_uid: uid.to_string(),
            outcome,
            failure_reason: match outcome {
                CommitmentOutcome::Committed => None,
                CommitmentOutcome::Failed => Some(FailureReason::NoSuchSopInstance),
            },
            study_uid: study.map(str::to_string),
        }
    }

    /// Read `obj[TAG].Value[0]` as a string - the DICOM JSON single-value shape.
    fn attr(obj: &JsonVal, tag: &str) -> Option<String> {
        obj.obj_get(tag)?
            .obj_get("Value")?
            .as_str_array_first()
            .map(str::to_string)
    }

    /// Read `obj[TAG].Value` as the item array of a sequence.
    fn seq_items<'a>(obj: &'a JsonVal, tag: &str) -> Option<&'a Vec<JsonVal>> {
        obj.obj_get(tag)?.obj_get("Value")?.as_array()
    }

    /// The rendered result must be VALID JSON, not merely the right bytes.
    ///
    /// This is the guard for the whole renderer: a missing closing brace
    /// produces a body that still contains every expected tag string, so only
    /// a parse catches it.
    #[test]
    fn the_instance_form_renders_parseable_json() {
        let record = CommitmentRecord {
            transaction_uid: "1.2.3".to_string(),
            requested_at: "2026-09-23T10:00:00Z".to_string(),
            request_form: RequestForm::Instance,
            instances: vec![
                instance("2.25.1", CommitmentOutcome::Committed, None),
                instance("2.25.2", CommitmentOutcome::Failed, None),
            ],
        };
        let json = render_result(&record);
        let parsed = parse_json(&json)
            .unwrap_or_else(|| panic!("body must parse: {}", String::from_utf8_lossy(&json)));

        let committed = seq_items(&parsed, "00081199").expect("committed sequence");
        assert_eq!(committed.len(), 1);
        assert_eq!(attr(&committed[0], "00081155").as_deref(), Some("2.25.1"));
        assert_eq!(
            attr(&committed[0], "00081150").as_deref(),
            Some("1.2.840.10008.5.1.4.1.1.2")
        );

        let failed = seq_items(&parsed, "00081198").expect("failed sequence");
        assert_eq!(failed.len(), 1);
        assert_eq!(attr(&failed[0], "00081155").as_deref(), Some("2.25.2"));
        assert_eq!(attr(&failed[0], "00081197").as_deref(), Some("0112"));
    }

    /// A fully committed answer carries no Failed SOP Sequence, and an
    /// all-failed answer carries no Referenced SOP Sequence.
    #[test]
    fn only_the_non_empty_sequences_are_emitted() {
        let committed_only = CommitmentRecord {
            transaction_uid: "1.2.3".to_string(),
            requested_at: "2026-09-23T10:00:00Z".to_string(),
            request_form: RequestForm::Instance,
            instances: vec![instance("2.25.1", CommitmentOutcome::Committed, None)],
        };
        let parsed = parse_json(&render_result(&committed_only)).expect("parse");
        assert!(parsed.obj_get("00081199").is_some());
        assert!(parsed.obj_get("00081198").is_none());

        let failed_only = CommitmentRecord {
            transaction_uid: "1.2.3".to_string(),
            requested_at: "2026-09-23T10:00:00Z".to_string(),
            request_form: RequestForm::Instance,
            instances: vec![instance("2.25.1", CommitmentOutcome::Failed, None)],
        };
        let parsed = parse_json(&render_result(&failed_only)).expect("parse");
        assert!(parsed.obj_get("00081199").is_none());
        assert!(parsed.obj_get("00081198").is_some());
    }

    /// The study form must be parseable too, and must nest the failed
    /// instances under their study.
    #[test]
    fn the_study_form_renders_parseable_json() {
        let record = CommitmentRecord {
            transaction_uid: "1.2.3".to_string(),
            requested_at: "2026-09-23T10:00:00Z".to_string(),
            request_form: RequestForm::Study,
            instances: vec![
                instance("2.25.1", CommitmentOutcome::Committed, Some("1.2.3.4")),
                instance("2.25.2", CommitmentOutcome::Failed, Some("1.2.3.4")),
            ],
        };
        let json = render_result(&record);
        let parsed = parse_json(&json)
            .unwrap_or_else(|| panic!("body must parse: {}", String::from_utf8_lossy(&json)));

        // Every study here has a failure, so the Failed Study Sequence is the
        // only outer key present.
        assert!(parsed.obj_get("00081110").is_none());
        let studies = seq_items(&parsed, "0008119B").expect("failed study sequence");
        assert_eq!(studies.len(), 1);
        assert_eq!(attr(&studies[0], "0020000D").as_deref(), Some("1.2.3.4"));
        let failed = seq_items(&studies[0], "00081198").expect("nested failed sop sequence");
        assert_eq!(failed.len(), 1);
        assert_eq!(attr(&failed[0], "00081155").as_deref(), Some("2.25.2"));
        assert_eq!(attr(&failed[0], "00081197").as_deref(), Some("0112"));
    }

    /// An all-committed study goes in the Referenced Study Sequence.
    #[test]
    fn a_committed_study_lands_in_the_study_sequence() {
        let record = CommitmentRecord {
            transaction_uid: "1.2.3".to_string(),
            requested_at: "2026-09-23T10:00:00Z".to_string(),
            request_form: RequestForm::Study,
            instances: vec![instance(
                "2.25.1",
                CommitmentOutcome::Committed,
                Some("1.2.3.4"),
            )],
        };
        let parsed = parse_json(&render_result(&record)).expect("parse");
        let studies = seq_items(&parsed, "00081110").expect("referenced study sequence");
        assert_eq!(attr(&studies[0], "0020000D").as_deref(), Some("1.2.3.4"));
        assert!(parsed.obj_get("0008119B").is_none());
    }

    // -- Accept negotiation ------------------------------------------------

    #[test]
    fn accept_classification_picks_the_expected_media_type() {
        assert!(matches!(classify_accept(None), AcceptKind::Json));
        assert!(matches!(classify_accept(Some("*/*")), AcceptKind::Json));
        assert!(matches!(
            classify_accept(Some("application/dicom+json")),
            AcceptKind::Json
        ));
        assert!(matches!(
            classify_accept(Some("application/dicom+xml")),
            AcceptKind::Xml
        ));
        assert!(matches!(
            classify_accept(Some("multipart/related; type=\"application/dicom+json\"")),
            AcceptKind::MultipartJson
        ));
        assert!(matches!(
            classify_accept(Some("multipart/related; type=\"application/dicom+xml\"")),
            AcceptKind::MultipartXml
        ));
    }

    #[test]
    fn the_multipart_wrapper_produces_one_well_formed_part() {
        let body = wrap_multipart("BOUND", "application/dicom+json", b"{\"a\":1}");
        let text = String::from_utf8(body).expect("utf8");
        assert!(text.starts_with("--BOUND\r\n"));
        assert!(text.contains("Content-Type: application/dicom+json\r\n\r\n"));
        assert!(text.contains("{\"a\":1}"));
        assert!(text.ends_with("\r\n--BOUND--\r\n"));
        // Exactly one opening delimiter (the closer has the trailing --).
        assert_eq!(text.matches("--BOUND\r\n").count(), 1);
    }

    // -- Request parsing ---------------------------------------------------

    #[test]
    fn a_uid_must_be_numeric_and_dotted() {
        assert!(is_valid_uid("1.2.840.10008.5.1.4.1.1.2"));
        assert!(is_valid_uid("2.25.1"));
        assert!(!is_valid_uid(""));
        assert!(!is_valid_uid("abc"));
        assert!(!is_valid_uid(".1.2"));
        assert!(!is_valid_uid("1.2."));
        assert!(!is_valid_uid("1.2.3/../4"));
    }

    #[test]
    fn the_instance_form_parses_into_flat_references() {
        let body = br#"{"00081199":{"vr":"SQ","Value":[
            {"00081150":{"vr":"UI","Value":["1.2.3"]},
             "00081155":{"vr":"UI","Value":["2.25.1"]}}]}}"#;
        let req = parse_request(body).expect("parse");
        assert!(matches!(req.form, RequestForm::Instance));
        assert_eq!(req.flattened.len(), 1);
        assert_eq!(req.flattened[0].1, "2.25.1");
        assert!(
            req.flattened[0].2.is_none(),
            "the instance form has no study reference"
        );
    }

    /// The single-dataset request body has two conformant spellings, and both
    /// must reach the same request.
    ///
    /// PS3.18 F.2.2 writes a single Data Set as the bare object, which is what
    /// this tree's own client sends; F.2.1's array form is what dcm4chee-arc
    /// accepts and what a strict client sends. A server that takes only one
    /// breaks on the other, and the symptom is a 400 on a conformant request.
    #[test]
    fn the_instance_form_also_parses_when_wrapped_in_an_array_of_one() {
        let inner = r#"{"00081199":{"vr":"SQ","Value":[
            {"00081150":{"vr":"UI","Value":["1.2.3"]},
             "00081155":{"vr":"UI","Value":["2.25.1"]}}]}}"#;
        let object = parse_request(inner.as_bytes()).expect("the object form parses");
        let array =
            parse_request(format!("[{inner}]").as_bytes()).expect("the array form parses too");

        assert!(matches!(array.form, RequestForm::Instance));
        assert_eq!(array.flattened.len(), 1);
        assert_eq!(array.flattened[0].1, object.flattened[0].1);
        assert_eq!(array.flattened[0].2, object.flattened[0].2);
    }

    /// The array form is unwrapped, not tolerated: everything after the
    /// unwrap - the closed key table, the sequences, the UID checks - still
    /// applies to the dataset inside.
    #[test]
    fn the_array_form_is_checked_like_the_object_form() {
        // An unknown top-level key inside the array is refused exactly as it
        // is at the top level of the object form (0008,1195 is not in J.1).
        let bad_key = br#"[{"00081199":{"vr":"SQ","Value":[
            {"00081150":{"vr":"UI","Value":["1.2.3"]},"00081155":{"vr":"UI","Value":["2.25.1"]}}]},
            "00081195":{"vr":"UI","Value":["1.2.3"]}}]"#;
        assert!(
            parse_request(bad_key).is_err(),
            "the key table still applies"
        );

        // A body that names no sequence is refused in the array form too.
        assert!(parse_request(br#"[{"00081150":{"vr":"UI","Value":["1.2.3"]}}]"#).is_err());
    }

    /// Only an array of exactly one object is unwrapped.
    ///
    /// An empty array names no dataset, and an array of two is a search
    /// answer rather than a single-dataset request body. Both keep the same
    /// 400 the object-only reader gave, because neither identifies WHICH
    /// dataset the commitment is about.
    #[test]
    fn an_empty_array_and_a_two_element_array_are_still_refused() {
        let first = r#"{"00081199":{"vr":"SQ","Value":[
            {"00081150":{"vr":"UI","Value":["1.2.3"]},"00081155":{"vr":"UI","Value":["2.25.1"]}}]}}"#;
        let second = r#"{"00081199":{"vr":"SQ","Value":[
            {"00081150":{"vr":"UI","Value":["1.2.3"]},"00081155":{"vr":"UI","Value":["2.25.2"]}}]}}"#;

        assert!(
            parse_request(b"[]").is_err(),
            "an empty array names no dataset"
        );
        assert!(
            parse_request(format!("[{first},{second}]").as_bytes()).is_err(),
            "two datasets are ambiguous"
        );
        assert!(
            parse_request(br#"[{"00081150":{"vr":"UI","Value":["1.2.3"]}}]"#).is_err(),
            "one object with no sequence is still a refusal"
        );
    }

    #[test]
    fn the_study_form_parses_into_flat_references_carrying_the_study() {
        let body = br#"{"00081110":{"vr":"SQ","Value":[{"0020000D":{"vr":"UI","Value":["1.2.3.4"]},
            "00081115":{"vr":"SQ","Value":[{"00081112":{"vr":"SQ","Value":[
            {"00081150":{"vr":"UI","Value":["1.2.3"]},"0008114A":{"vr":"SQ","Value":[
            {"00081155":{"vr":"UI","Value":["2.25.1"]}}]}}]}}]}}]}}"#;
        let req = parse_request(body).expect("parse");
        assert!(matches!(req.form, RequestForm::Study));
        assert_eq!(req.flattened.len(), 1);
        assert_eq!(req.flattened[0].2.as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn both_sequence_forms_at_once_is_refused() {
        // 0008,1199 and 0008,1110 are each 1C: exactly one may be present.
        let body = br#"{"00081199":{"vr":"SQ","Value":[
            {"00081150":{"vr":"UI","Value":["1.2.3"]},"00081155":{"vr":"UI","Value":["2.25.1"]}}]},
            "00081110":{"vr":"SQ","Value":[]}}"#;
        assert!(parse_request(body).is_err());
    }

    #[test]
    fn a_payload_with_no_sequence_is_refused() {
        assert!(parse_request(br#"{"00081150":{"vr":"UI","Value":["1.2.3"]}}"#).is_err());
    }

    #[test]
    fn an_unknown_top_level_key_is_refused() {
        let body = br#"{"00081199":{"vr":"SQ","Value":[
            {"00081150":{"vr":"UI","Value":["1.2.3"]},"00081155":{"vr":"UI","Value":["2.25.1"]}}]},
            "00081195":{"vr":"UI","Value":["1.2.3"]}}"#;
        assert!(
            parse_request(body).is_err(),
            "0008,1195 is not in Annex J.1"
        );
    }

    #[test]
    fn a_duplicate_instance_is_refused() {
        let flat = vec![
            ("1.2.3".to_string(), "2.25.1".to_string(), None),
            ("1.2.3".to_string(), "2.25.1".to_string(), None),
        ];
        assert!(reject_duplicate_instances(&flat).is_err());
    }

    #[test]
    fn distinct_instances_are_accepted() {
        let flat = vec![
            ("1.2.3".to_string(), "2.25.1".to_string(), None),
            ("1.2.3".to_string(), "2.25.2".to_string(), None),
        ];
        assert!(reject_duplicate_instances(&flat).is_ok());
    }

    #[test]
    fn the_timestamp_has_the_shape_the_store_parses() {
        let ts = now_timestamp();
        assert_eq!(ts.len(), 20, "timestamp: {ts}");
        assert!(ts.ends_with('Z'), "timestamp: {ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }

    /// A body the XML writer refuses must not be answered as an empty 200.
    ///
    /// `application/dicom+xml` with zero bytes says "the document is empty",
    /// and an empty document is a well-formed answer to a well-formed request -
    /// so the client had no way to learn the render failed. The body used here
    /// is truncated JSON, which the PS3.19 converter parses before rendering.
    #[test]
    fn an_unrenderable_body_answers_500_rather_than_an_empty_200() {
        let truncated = b"[{\"00080016\":{\"Value\":[".to_vec();
        for kind in [
            AcceptKind::Xml,
            AcceptKind::MultipartXml,
            AcceptKind::MultipartJson,
        ] {
            let multipart_json = matches!(kind, AcceptKind::MultipartJson);
            let is_xml = matches!(kind, AcceptKind::Xml);
            let resp = finish(kind, "2.25.7", truncated.clone(), 0);
            if multipart_json {
                // The JSON part is never parsed, so it is returned verbatim
                // inside the multipart envelope.
                assert_eq!(resp.status, 200, "the JSON bytes need no XML pass");
                assert!(
                    resp.body
                        .windows(truncated.len())
                        .any(|w| w == truncated.as_slice())
                );
                continue;
            }
            assert_eq!(resp.status, 500, "the XML branch must not claim success");
            assert_eq!(resp.content_type, "application/dicom+json; charset=utf-8");
            assert!(
                resp.body.windows(7).any(|w| w == b"\"error\""),
                "the failure is named in the body (xml={is_xml})"
            );
        }
        // The happy path still renders, so the refusal is about the body.
        let ok = finish(AcceptKind::Xml, "2.25.7", b"[]".to_vec(), 0);
        assert_eq!(ok.status, 200);
        assert_eq!(ok.content_type, "application/dicom+xml");
    }
}
