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
//
// Every tag below is quoted from `standard/ps3.4/storage-commitment-module-tags.json`
// (PS3.4 Annex J). They are string literals because they are DICOM JSON KEYS,
// not attribute tags: the module writes its own payload and never consults the
// parser's tag table, so a numeric constant would have to be re-rendered on
// every use. The test `the_wire_tags_match_the_standard_file` keeps each one
// bound to the carded fact.

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
    // PS3.18 8.7.8 / RFC 9110 12.5.1: the header is a comma-separated list of
    // media ranges, each optionally carrying `q=`. Searching the whole string
    // with `contains` treated an EXCLUDED type (`;q=0`) as a request for it, so
    // `application/dicom+json, application/dicom+xml;q=0` answered XML.
    //
    // Each range is scored, and the highest acceptable one wins. A range with
    // `q=0` is not acceptable at all; an absent `q` is 1.
    let mut best: Option<(u32, AcceptKind)> = None;
    for range in a.split(',') {
        let mut parts = range.split(';');
        let Some(media) = parts.next() else { continue };
        let media = media.trim().to_ascii_lowercase();
        if media.is_empty() {
            continue;
        }
        let mut quality = 1000u32;
        for param in parts {
            let param = param.trim();
            if let Some(q) = param
                .strip_prefix("q=")
                .or_else(|| param.strip_prefix("Q="))
            {
                quality = parse_quality(q).unwrap_or(0);
            }
        }
        if quality == 0 {
            continue;
        }
        let kind = match media.as_str() {
            "multipart/related" => AcceptKind::MultipartJson,
            "application/dicom+xml" => AcceptKind::Xml,
            // `*/*` and `application/*` mean "anything", which is the default.
            "application/dicom+json" => AcceptKind::Json,
            _ => continue,
        };
        // A multipart range that also NAMES a part type selects that part type.
        // The `type=` parameter is what carries it; it is read from the same
        // range so the exclusion above still applies.
        let kind = if matches!(kind, AcceptKind::MultipartJson)
            && range.to_ascii_lowercase().contains("dicom+xml")
        {
            AcceptKind::MultipartXml
        } else {
            kind
        };
        if best.as_ref().is_none_or(|(q, _)| quality > *q) {
            best = Some((quality, kind));
        }
    }
    best.map(|(_, kind)| kind).unwrap_or(AcceptKind::Json)
}

/// Parse an RFC 9110 `q` weight into thousandths.
///
/// `q=0.001` is the smallest acceptable weight and `q=0` means "not
/// acceptable". Anything outside `0..=1` is malformed and treated as
/// unacceptable rather than as full weight.
fn parse_quality(raw: &str) -> Option<u32> {
    let raw = raw.trim();
    let value: f64 = raw.parse().ok()?;
    if !(0.0..=1.0).contains(&value) {
        return None;
    }
    Some((value * 1000.0).round() as u32)
}

/// A boundary that is unique per response.
///
/// The value only has to be absent from the body; the Transaction UID plus a
/// process-unique counter gives that for the small DICOM JSON payloads here
/// without pulling in a random source.
fn boundary_for(transaction_uid: &str, seq: u64) -> String {
    // RFC 2046 5.1.1: a boundary is at most 70 characters. A conformant UID may
    // be 64 characters, and `clarus-commitment-` plus 64 plus a counter is 84 -
    // so the UID part is TRUNCATED rather than trusted. The counter is what
    // makes the value unique, so clipping the readable part loses nothing.
    const MAX_BOUNDARY: usize = 70;
    let prefix = "clarus-commitment-";
    let suffix = format!("-{seq}");
    let budget = MAX_BOUNDARY.saturating_sub(prefix.len() + suffix.len());
    let sanitized: String = transaction_uid
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(budget)
        .collect();
    format!("{prefix}{sanitized}{suffix}")
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
    /// `(sop_class_uid, sop_instance_uid, study_uid, series_uid)` in request
    /// order. A `None` slot is what the request did NOT name: the instance form
    /// (Annex J.1) carries no study or series reference, so neither is
    /// invented.
    ///
    /// The series travels because a reference names a SLOT in the archive, not
    /// just an object: an instance stored under series A of the study must not
    /// satisfy a reference made under series B.
    flattened: Vec<(String, String, Option<String>, Option<String>)>,
}

/// Read a DICOM JSON `Value` array of one tag as a list of objects.
fn tag_sequence_items<'a>(obj: &'a JsonVal, tag: &str) -> Option<&'a Vec<JsonVal>> {
    obj.obj_get(tag)?.obj_get("Value")?.as_array()
}

/// Whether the request STATES `tag`, whether or not it carries a `Value`.
///
/// The 1C rule of Annex J.1 is about which module the request DECLARED, not
/// about whether the declaration turned out to hold anything. Treating a
/// present-but-empty `0008,1110` as absent let a payload that declared BOTH
/// forms through the "one of the two" gate: `0008,1199` was honoured and the
/// empty study sequence silently dropped.
fn states_tag(obj: &JsonVal, tag: &str) -> bool {
    obj.obj_get(tag).is_some()
}

/// Extract the single string value of one tag from a DICOM JSON item.
///
/// Every tag this module reads is single-valued (a UID, or a reference), so an
/// item carrying several values is a malformed reference, not a reference to
/// several things: taking the first would silently commit one of them and drop
/// the rest, leaving no way for the requester to learn that. The error is the
/// caller's to raise.
fn tag_single_string_strict(item: &JsonVal, tag: &str) -> Result<Option<String>, CommitmentErr> {
    let Some(v) = item.obj_get(tag) else {
        return Ok(None);
    };
    let Some(arr) = v.obj_get("Value").and_then(JsonVal::as_array) else {
        // A present attribute with no Value array is an empty reference, which
        // is not the same as an absent one: the client stated the tag and gave
        // nothing.
        return Err(bad_request(
            "a reference attribute is present but carries no Value array",
        ));
    };
    match arr.len() {
        0 => Err(bad_request(
            "a reference attribute carries an empty Value array",
        )),
        1 => arr[0]
            .as_str()
            .map(|s| Some(s.to_string()))
            .ok_or_else(|| bad_request("a reference attribute is not a string")),
        _ => Err(bad_request(
            "a single-valued reference attribute carries more than one value",
        )),
    }
}

/// Extract the single string value of one tag from a DICOM JSON item.
fn tag_single_string(item: &JsonVal, tag: &str) -> Option<String> {
    tag_single_string_strict(item, tag).ok().flatten()
}

/// Collect an instance reference (`0008,1150` + `0008,1155`) from one item.
fn read_instance_ref(
    item: &JsonVal,
    study_uid: Option<&str>,
    series_uid: Option<&str>,
) -> Result<(String, String, Option<String>, Option<String>), CommitmentErr> {
    // STRICT: a multi-valued or empty reference is malformed, not a reference
    // to the first of several. Committing one and dropping the rest would leave
    // the requester unable to learn which of the SOPs it named were answered.
    let sop_class = tag_single_string_strict(item, T_REFERENCED_SOP_CLASS_UID)?
        .ok_or_else(|| bad_request("Referenced SOP Class UID (0008,1150) is missing"))?;
    let sop_instance = tag_single_string_strict(item, T_REFERENCED_SOP_INSTANCE_UID)?
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
    Ok((
        sop_class,
        sop_instance,
        study_uid.map(str::to_string),
        series_uid.map(str::to_string),
    ))
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

    // Presence is decided by the TAG, not by a successful `Value` read: a
    // sequence the request declared with no `Value` array still declares that
    // form, and dropping it here would let a payload carrying both forms past
    // the mutual-exclusion rule.
    if states_tag(root, T_REFERENCED_SOP_SEQUENCE) && states_tag(root, T_REFERENCED_STUDY_SEQUENCE)
    {
        return Err(bad_request(
            "Referenced SOP Sequence (0008,1199) and Referenced Study Sequence (0008,1110) are mutually exclusive (1C)",
        ));
    }
    if states_tag(root, T_REFERENCED_SOP_SEQUENCE) && sop_seq.is_none() {
        return Err(bad_request(
            "Referenced SOP Sequence (0008,1199) is present but carries no Value array",
        ));
    }
    if states_tag(root, T_REFERENCED_STUDY_SEQUENCE) && study_seq.is_none() {
        return Err(bad_request(
            "Referenced Study Sequence (0008,1110) is present but carries no Value array",
        ));
    }

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
                flattened.push(read_instance_ref(item, None, None)?);
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
                    let series_uid = tag_single_string(s, "0020000E").ok_or_else(|| {
                        bad_request(
                            "SeriesInstanceUID is missing from the Referenced Series Sequence item",
                        )
                    })?;
                    if !is_valid_uid(&series_uid) {
                        return Err(bad_request(
                            "SeriesInstanceUID in the Referenced Series Sequence is not a UID",
                        ));
                    }
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
                        let sop_class =
                            tag_single_string_strict(class_item, T_REFERENCED_SOP_CLASS_UID)?
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
                            let sop_instance =
                                tag_single_string_strict(inst, T_REFERENCED_SOP_INSTANCE_UID)?
                                    .ok_or_else(|| {
                                        bad_request(
                                            "Referenced SOP Instance UID (0008,1155) is missing",
                                        )
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
                                Some(series_uid.clone()),
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
    flattened: &[(String, String, Option<String>, Option<String>)],
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
    /// (0008,0016) SOP Class UID the manifest records for the instance.
    ///
    /// A reference names BOTH the class and the instance, and the answer has
    /// to be about the pair: an instance stored as CT referenced as MR is not
    /// the object the requester committed, and echoing the requested class
    /// back would confirm a commitment to something the archive does not hold.
    sop_class_uid: Option<String>,
    /// (0020,000D) of the manifest that vouched for it, when one did.
    study_uid: Option<String>,
    /// (0020,000E) of the series holding it, when a manifest vouched for it.
    ///
    /// Annex J.2 answers a study-shaped request with a Study Sequence whose
    /// items carry a Referenced Series Sequence; without the series the
    /// response cannot state the hierarchy the standard defines.
    series_uid: Option<String>,
}

/// (0008,0016) SOP Class UID the manifest records for `sop_instance_uid`.
///
/// A reference names a CLASS and an INSTANCE, so the answer has to be about
/// the pair. The manifest holds the class on the instance itself, which is the
/// only place a DICOMweb archive can settle the question without opening the
/// object.
fn sop_class_of_instance(
    m: &clarus_core::manifest::Manifest,
    sop_instance_uid: &str,
) -> Option<String> {
    let inst = m.find_instance(sop_instance_uid)?;
    inst.attrs.iter().find_map(|(t, v)| {
        if *t == clarus_core::query::tag::SOP_CLASS_UID
            && let clarus_core::manifest::AttrValue::Text(s) = v
        {
            return Some(s.clone());
        }
        None
    })
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
        state: &AppState,
        study_uid: &str,
    ) -> Option<&clarus_core::manifest::Manifest> {
        self.by_study
            .entry(study_uid.to_string())
            .or_insert_with(|| state.read_server_manifest(study_uid).and_then(|r| r.ok()))
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
    uids: &[(&str, Option<&str>, Option<&str>)],
) -> Result<Vec<InstancePresence>, CommitmentErr> {
    let mut out: Vec<InstancePresence> = Vec::with_capacity(uids.len());
    // UIDs the index (or the study hint) could not place, with their input
    // positions so the answers land back in order.
    let mut pending: Vec<(usize, &str)> = Vec::new();
    let mut cache = ManifestCache::default();

    for (index, (sop, hint, _series)) in uids.iter().enumerate() {
        let resolved = match hint {
            // A study-shaped request names the study: one probe, no scan.
            Some(study_uid) => cache
                .get(state, study_uid)
                .filter(|m| m.find_instance(sop).is_some())
                .map(|m| {
                    let study = m.study_instance_uid().unwrap_or(study_uid).to_string();
                    let series = m.series_of_instance(sop).map(str::to_string);
                    let class = sop_class_of_instance(m, sop);
                    (study, series, class)
                }),
            // The index names the owning study in the common case.
            None => state.shadow.sop_owner_study(sop).and_then(|owner| {
                cache
                    .get(state, &owner)
                    .filter(|m| m.find_instance(sop).is_some())
                    .map(|m| {
                        let study = m.study_instance_uid().unwrap_or(&owner).to_string();
                        let series = m.series_of_instance(sop).map(str::to_string);
                        let class = sop_class_of_instance(m, sop);
                        (study, series, class)
                    })
            }),
        };
        match resolved {
            Some((study_uid, series_uid, sop_class)) => out.push(InstancePresence {
                present: true,
                sop_class_uid: sop_class,
                study_uid: Some(study_uid),
                series_uid,
            }),
            None => {
                out.push(InstancePresence {
                    present: false,
                    sop_class_uid: None,
                    study_uid: None,
                    series_uid: None,
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
        let Some(Ok(m)) = state.read_server_manifest(stem) else {
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
                    sop_class_uid: sop_class_of_instance(&m, sop),
                    study_uid: Some(owner.clone()),
                    series_uid: m.series_of_instance(sop).map(str::to_string),
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
fn promote_committed_studies(state: &AppState, record: &CommitmentRecord) {
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
        let Some(Ok(manifest)) = state.read_server_manifest(study_uid) else {
            continue;
        };
        let stored = manifest.sop_instance_uids();
        if stored.is_empty() {
            continue;
        }
        let covered = stored.iter().all(|uid| instance_uids.contains(uid));
        if covered {
            // A commit that covers the whole study IS the quiet period ending:
            // the sender has said "that is everything". The token is built
            // from the CURRENT generation because this path did not go through
            // `take_due` - a STOW landing from here on bumps the generation
            // and `promote` then stands down, which is the same protection the
            // sweep gets.
            let token = super::readiness::DueStudy {
                study_instance_uid: study_uid.to_string(),
                generation: state.readiness.generation(study_uid),
                deadline: std::time::Instant::now(),
            };
            super::readiness::promote(state, &token);
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
        // PS3.18 F.2.3: a numeric VR's JSON value is a NUMBER, so 0112H is
        // `[274]`. Writing the hex digits as a string declared a US value the
        // payload's own type does not agree with.
        out.extend_from_slice(reason.code_number().to_string().as_bytes());
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
    // PS3.18 Annex J.2 / Table J.2-1 gives BOTH outer sequences the same
    // hierarchy: Study -> Referenced Series Sequence (0008,1115) -> Series ->
    // Referenced SOP Sequence (0008,1199) -> the (class, instance) pair. The
    // study is the outermost item in both, so a study holding one committed and
    // one failed instance appears in BOTH sequences, each listing only the
    // instances of that outcome. A flat Failed SOP Sequence on the study item
    // was a shape the table does not define, and it made a mixed study look
    // entirely failed.
    let groups = group_by_study_series(results);

    out.push(b'{');
    let mut need_comma = false;
    for (tag, want) in [
        (T_REFERENCED_STUDY_SEQUENCE, CommitmentOutcome::Committed),
        (T_FAILED_STUDY_SEQUENCE, CommitmentOutcome::Failed),
    ] {
        let studies: Vec<&StudyGroups> = groups
            .iter()
            .filter(|s| {
                s.series
                    .iter()
                    .any(|se| se.instances.iter().any(|r| r.outcome == want))
            })
            .collect();
        if studies.is_empty() {
            continue;
        }
        if need_comma {
            out.push(b',');
        }
        need_comma = true;
        write_tag_key(out, tag, "SQ");
        out.push(b'[');
        for (n, study) in studies.iter().enumerate() {
            if n > 0 {
                out.push(b',');
            }
            write_study_item(out, study, want);
        }
        // The sequence value list, then the attribute object opened by
        // `write_tag_key` (`"TAG":{"vr":"SQ","Value":`). The study ITEM is
        // already closed by `write_study_item`.
        out.extend_from_slice(b"]}");
    }
    out.push(b'}');
}

/// One study item of a J.2 outer sequence, carrying only `want` instances.
fn write_study_item(out: &mut Vec<u8>, study: &StudyGroups, want: CommitmentOutcome) {
    out.push(b'{');
    out.extend_from_slice(b"\"0020000D\":{\"vr\":\"UI\",\"Value\":[");
    push_json_str(out, &study.study_uid);
    out.extend_from_slice(b"]},");
    write_tag_key(out, T_REFERENCED_SERIES_SEQUENCE, "SQ");
    out.push(b'[');
    let mut wrote_series = false;
    for series in &study.series {
        let items: Vec<&InstanceResult> = series
            .instances
            .iter()
            .filter(|r| r.outcome == want)
            .collect();
        if items.is_empty() {
            // A series with nothing of this outcome is not part of THIS
            // sequence's statement; it appears in the other one.
            continue;
        }
        if wrote_series {
            out.push(b',');
        }
        wrote_series = true;
        out.push(b'{');
        out.extend_from_slice(b"\"0020000E\":{\"vr\":\"UI\",\"Value\":[");
        push_json_str(out, &series.series_uid);
        out.extend_from_slice(b"]},");
        write_tag_key(out, T_REFERENCED_SOP_SEQUENCE, "SQ");
        out.push(b'[');
        for (k, r) in items.iter().enumerate() {
            if k > 0 {
                out.push(b',');
            }
            write_instance_ref(out, &r.sop_class_uid, &r.sop_instance_uid, r.failure_reason);
        }
        // Close the Referenced SOP Sequence value list, its attribute object,
        // and the SERIES item object.
        out.extend_from_slice(b"]}}");
    }
    // Close the Referenced Series Sequence value list, its attribute object,
    // and the study item object.
    out.extend_from_slice(b"]}}");
}

/// One study's instances grouped by series, in request order.
struct StudyGroups {
    study_uid: String,
    series: Vec<SeriesGroup>,
}

struct SeriesGroup {
    series_uid: String,
    instances: Vec<InstanceResult>,
}

fn group_by_study_series(results: &[InstanceResult]) -> Vec<StudyGroups> {
    let mut out: Vec<StudyGroups> = Vec::new();
    for r in results {
        let study_uid = r.study_uid.clone().unwrap_or_default();
        let series_uid = r.series_uid.clone().unwrap_or_default();
        if !out.iter().any(|s| s.study_uid == study_uid) {
            out.push(StudyGroups {
                study_uid: study_uid.clone(),
                series: Vec::new(),
            });
        }
        let study = out
            .iter_mut()
            .find(|s| s.study_uid == study_uid)
            .expect("pushed above");
        if !study.series.iter().any(|s| s.series_uid == series_uid) {
            study.series.push(SeriesGroup {
                series_uid: series_uid.clone(),
                instances: Vec::new(),
            });
        }
        study
            .series
            .iter_mut()
            .find(|s| s.series_uid == series_uid)
            .expect("pushed above")
            .instances
            .push(r.clone());
    }
    out
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
        // The router validates the Transaction UID before any store call, so
        // this only fires if a future path skips that. A UID that cannot name
        // a file is the client's mistake, not the service's, and PS3.18 13.4.1.2
        // makes an unusable identifier a 400.
        StoreError::InvalidUid(why) => bad_request(&format!(
            "The Transaction UID in the resource path is not a UID: {why}"
        )),
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
    let wanted: Vec<(&str, Option<&str>, Option<&str>)> = request
        .flattened
        .iter()
        .map(|(_, sop, study, series)| (sop.as_str(), study.as_deref(), series.as_deref()))
        .collect();
    let presences = evaluate_instances(state, &dir, &wanted)?;

    let mut instances = Vec::with_capacity(request.flattened.len());
    for ((sop_class, sop_instance, study_uid, series_uid), presence) in
        request.flattened.iter().zip(presences)
    {
        // The commitment is to a (class, instance) PAIR. An instance stored as
        // CT but referenced as MR is not the object the requester committed, so
        // the class the manifest records must equal the one requested. A
        // manifest that records no class cannot confirm the pair either.
        let class_agrees = presence
            .sop_class_uid
            .as_deref()
            .is_some_and(|stored| stored == sop_class.as_str());
        // A reference names a slot, not just an object. An instance stored
        // under series A must not satisfy a reference made under series B of
        // the same study: the requester asked for THAT series' content, and
        // answering with a sibling's object would commit the wrong thing.
        // A request that names no series (the instance form) has nothing to
        // check.
        let series_agrees = match series_uid {
            None => true,
            Some(requested) => presence
                .series_uid
                .as_deref()
                .is_some_and(|stored| stored == requested.as_str()),
        };
        let (outcome, failure_reason) = if presence.present && class_agrees && series_agrees {
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
            // The requested series wins when one was named: the group must
            // describe the slot the requester pointed at, which is what the
            // `series_agrees` check just confirmed.
            series_uid: series_uid.clone().or(presence.series_uid),
        });
    }

    let record = CommitmentRecord {
        transaction_uid: transaction_uid.to_string(),
        requested_at: now_timestamp()?,
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
    promote_committed_studies(state, &record);

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
///
/// FALLIBLE: `requested_at` drives retention, so an unconvertible clock must
/// not be written as 1970 - that would place a fresh commitment at the start
/// of the retention window and expire it on the next sweep.
fn now_timestamp() -> Result<String, CommitmentErr> {
    let now = clarus_core::time::local_now()
        .map_err(|_| internal_err("the system clock cannot be converted to a timestamp"))?;
    Ok(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year, now.month, now.day, now.hour, now.minute, now.second
    ))
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
            series_uid: None,
        }
    }

    /// Read `obj[TAG].Value[0]` as an unsigned number - the DICOM JSON shape of
    /// a numeric VR such as US (PS3.18 F.2.3).
    ///
    /// A failure reason is `[274]`, not `["0112"]`, so a string helper would
    /// silently answer `None` for a correct payload.
    fn attr_number(obj: &JsonVal, tag: &str) -> Option<u64> {
        match obj.obj_get(tag)?.obj_get("Value")?.as_array()?.first()? {
            JsonVal::U64(n) => Some(*n),
            _ => None,
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
        // US is a numeric VR, so the value is the NUMBER 274 (0x0112).
        assert_eq!(
            attr_number(&failed[0], "00081197"),
            Some(274),
            "Failure Reason is an unsigned number, not the hex digits as a string"
        );
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

        // The study holds BOTH outcomes, so it appears in BOTH outer
        // sequences - each listing only its own instances (Annex J.2).
        let committed = seq_items(&parsed, "00081110").expect("referenced study sequence");
        assert_eq!(committed.len(), 1);
        assert_eq!(attr(&committed[0], "0020000D").as_deref(), Some("1.2.3.4"));
        let series = seq_items(&committed[0], "00081115").expect("referenced series sequence");
        let ok = seq_items(&series[0], "00081199").expect("referenced sop sequence");
        assert_eq!(ok.len(), 1);
        assert_eq!(attr(&ok[0], "00081155").as_deref(), Some("2.25.1"));

        let studies = seq_items(&parsed, "0008119B").expect("failed study sequence");
        assert_eq!(studies.len(), 1);
        assert_eq!(attr(&studies[0], "0020000D").as_deref(), Some("1.2.3.4"));
        let series = seq_items(&studies[0], "00081115").expect("referenced series sequence");
        let failed = seq_items(&series[0], "00081199").expect("referenced sop sequence");
        assert_eq!(failed.len(), 1);
        assert_eq!(attr(&failed[0], "00081155").as_deref(), Some("2.25.2"));
        assert_eq!(attr_number(&failed[0], "00081197"), Some(274));
        // The flat failed sequence the old renderer emitted is gone.
        assert!(
            studies[0].obj_get("00081198").is_none(),
            "the failure is nested under series -> SOP, not flat on the study"
        );
    }

    /// RFC 2046 5.1.1: a boundary is at most 70 characters. A conformant UID
    /// may be 64, so a fixed prefix plus the whole UID plus a counter exceeded
    /// the limit and a strict MIME reader rejected the multipart body.
    #[test]
    fn boundary_stays_within_the_rfc_2046_limit() {
        let long_uid = "1".repeat(64);
        let boundary = boundary_for(&long_uid, u64::MAX);
        assert!(
            boundary.len() <= 70,
            "boundary is {} chars: {boundary}",
            boundary.len()
        );
        // Still unique per response: the counter is what carries uniqueness.
        assert_ne!(boundary_for(&long_uid, 1), boundary_for(&long_uid, 2));
        // And it appears verbatim in the header and the body delimiters.
        let body = wrap_multipart(&boundary, "application/dicom+json", b"{}");
        let text = String::from_utf8(body).expect("utf-8");
        assert_eq!(
            text.matches(&boundary).count(),
            2,
            "opening and closing delimiter use the same value: {text}"
        );
    }

    /// The manifest records the class of every instance it lists, so the
    /// presence answer must carry it - the commit loop compares against it.
    #[test]
    fn the_presence_record_carries_the_stored_sop_class() {
        use clarus_core::manifest::{
            AttrValue, InstanceRecord, Manifest, NOT_REJECTED_REASON, SeriesRecord, StudyQido,
            SysHeader, TIER_HOT,
        };
        let m = Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 64,
                arrived_at: 0,
            },
            study: StudyQido {
                attrs: vec![(0x0020_000D, AttrValue::Text("1.2.3".to_string()))],
                series_count: 1,
                instance_count: 1,
                modalities_in_study: Some("CT".to_string()),
            },
            series: vec![SeriesRecord {
                attrs: vec![(0x0020_000E, AttrValue::Text("1.2.3.4".to_string()))],
                instances: vec![InstanceRecord {
                    attrs: vec![
                        (0x0008_0018, AttrValue::Text("1.2.3.4.5".to_string())),
                        (
                            0x0008_0016,
                            AttrValue::Text("1.2.840.10008.5.1.4.1.1.2".to_string()),
                        ),
                    ],
                    transfer_syntax: "1.2.840.10008.1.2.1".to_string(),
                    storage_tier: TIER_HOT,
                    rejection_reason: NOT_REJECTED_REASON,
                    blake3: [0u8; 32],
                    file_size: 64,
                    frames: Vec::new(),
                }],
            }],
        };
        assert_eq!(
            sop_class_of_instance(&m, "1.2.3.4.5").as_deref(),
            Some("1.2.840.10008.5.1.4.1.1.2")
        );
        // An instance the manifest does not list has no class to compare.
        assert_eq!(sop_class_of_instance(&m, "9.9.9"), None);
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
        // J.2 nests the instances: study -> series -> SOP.
        let series = seq_items(&studies[0], "00081115").expect("referenced series sequence");
        let sop = seq_items(&series[0], "00081199").expect("referenced sop sequence");
        assert_eq!(attr(&sop[0], "00081155").as_deref(), Some("2.25.1"));
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
            "00081115":{"vr":"SQ","Value":[{"0020000E":{"vr":"UI","Value":["1.2.3.5"]},
            "00081112":{"vr":"SQ","Value":[
            {"00081150":{"vr":"UI","Value":["1.2.3"]},"0008114A":{"vr":"SQ","Value":[
            {"00081155":{"vr":"UI","Value":["2.25.1"]}}]}}]}}]}}]}}"#;
        let req = parse_request(body).expect("parse");
        assert!(matches!(req.form, RequestForm::Study));
        assert_eq!(req.flattened.len(), 1);
        assert_eq!(req.flattened[0].2.as_deref(), Some("1.2.3.4"));
        assert_eq!(
            req.flattened[0].3.as_deref(),
            Some("1.2.3.5"),
            "the referenced series travels with the reference"
        );
    }

    /// A study-form reference with no SeriesInstanceUID cannot name a slot, so
    /// it is refused rather than silently accepted as "any series".
    #[test]
    fn a_study_form_reference_without_a_series_uid_is_refused() {
        let body = br#"{"00081110":{"vr":"SQ","Value":[{"0020000D":{"vr":"UI","Value":["1.2.3.4"]},
            "00081115":{"vr":"SQ","Value":[{"00081112":{"vr":"SQ","Value":[
            {"00081150":{"vr":"UI","Value":["1.2.3"]},"0008114A":{"vr":"SQ","Value":[
            {"00081155":{"vr":"UI","Value":["2.25.1"]}}]}}]}}]}}]}}"#;
        assert!(
            parse_request(body).is_err(),
            "a reference that names no series is not a complete reference"
        );
    }

    /// A single-valued reference carrying TWO values is malformed.
    ///
    /// Taking the first would commit one SOP and drop the other, and the
    /// requester would have no way to learn which of the two it named was
    /// answered.
    #[test]
    fn a_multi_valued_reference_is_refused() {
        let body = br#"{"00081199":{"vr":"SQ","Value":[
            {"00081150":{"vr":"UI","Value":["1.2.3"]},
             "00081155":{"vr":"UI","Value":["2.25.1","2.25.2"]}}]}}"#;
        assert!(
            parse_request(body).is_err(),
            "a reference names one instance, not a list"
        );
    }

    /// A declared sequence with no `Value` array is not "absent": it declared a
    /// form. Both spellings must be refused rather than silently dropped.
    #[test]
    fn a_declared_sequence_without_a_value_array_is_refused() {
        // Declares BOTH forms: the empty study sequence must still trip the 1C
        // mutual-exclusion rule.
        let both = br#"{"00081199":{"vr":"SQ","Value":[
            {"00081150":{"vr":"UI","Value":["1.2.3"]},
             "00081155":{"vr":"UI","Value":["2.25.1"]}}]},
            "00081110":{"vr":"SQ"}}"#;
        assert!(
            parse_request(both).is_err(),
            "an empty study sequence still DECLARES the study form"
        );

        // A lone empty form is an empty request, not a missing one.
        let lone = br#"{"00081199":{"vr":"SQ"}}"#;
        assert!(
            parse_request(lone).is_err(),
            "a present sequence with no Value array is refused by name"
        );
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
            ("1.2.3".to_string(), "2.25.1".to_string(), None, None),
            ("1.2.3".to_string(), "2.25.1".to_string(), None, None),
        ];
        assert!(reject_duplicate_instances(&flat).is_err());
    }

    #[test]
    fn distinct_instances_are_accepted() {
        let flat = vec![
            ("1.2.3".to_string(), "2.25.1".to_string(), None, None),
            ("1.2.3".to_string(), "2.25.2".to_string(), None, None),
        ];
        assert!(reject_duplicate_instances(&flat).is_ok());
    }

    #[test]
    fn the_timestamp_has_the_shape_the_store_parses() {
        let ts = now_timestamp().expect("the clock is after the epoch on a sane host");
        assert_eq!(ts.len(), 20, "timestamp: {ts}");
        assert!(ts.ends_with('Z'), "timestamp: {ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }

    /// Every wire tag this module writes is in the carded Annex J fact.
    ///
    /// The constants are literals because they are DICOM JSON KEYS, so nothing
    /// in the type system stops one from drifting (a transposed digit writes a
    /// sequence under a tag no client reads, and the answer still parses). This
    /// test is the binding: a tag that is not in the standard file fails here.
    #[test]
    fn the_wire_tags_match_the_standard_file() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("standard")
            .join("ps3.4")
            .join("storage-commitment-module-tags.json");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("standard reference {}: {e}", path.display()));
        for (constant, tag) in [
            ("Referenced SOP Sequence", T_REFERENCED_SOP_SEQUENCE),
            ("Failed SOP Sequence", T_FAILED_SOP_SEQUENCE),
            ("Failure Reason", T_FAILURE_REASON),
            ("Referenced Study Sequence", T_REFERENCED_STUDY_SEQUENCE),
            ("Failed Study Sequence", T_FAILED_STUDY_SEQUENCE),
            ("Referenced Series Sequence", T_REFERENCED_SERIES_SEQUENCE),
            ("Referenced SOP Class UID", T_REFERENCED_SOP_CLASS_UID),
            ("Referenced SOP Instance UID", T_REFERENCED_SOP_INSTANCE_UID),
        ] {
            let needle = format!("\"{tag}\": \"{constant}\"");
            assert!(
                text.contains(&needle),
                "{tag} is not carded as {constant} in {}",
                path.display()
            );
        }
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
        // A top-level ARRAY is not a metadata document: PS3.18 Table 10.4-1
        // makes the RESPONSE an array of datasets, but each element of it - the
        // thing this renderer converts - is a JSON object. Accepting `[]` and
        // emitting an empty model made a producer error indistinguishable from
        // genuinely empty metadata, so it is refused like any other unrenderable
        // body.
        let empty = finish(AcceptKind::Xml, "2.25.7", b"[]".to_vec(), 0);
        assert_eq!(empty.status, 500, "a non-object document is not renderable");

        // The happy path still renders, so the refusal is about the body.
        let ok = finish(AcceptKind::Xml, "2.25.7", b"{}".to_vec(), 0);
        assert_eq!(ok.status, 200);
        assert_eq!(ok.content_type, "application/dicom+xml");
    }
}
