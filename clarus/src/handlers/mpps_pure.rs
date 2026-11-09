// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! MPPS-RS pure core (PS3.18 §15).
//!
//! Three transactions on one resource: Create (15.4, N-CREATE), Update (15.5,
//! N-SET, distinguished by the mandatory `?update` parameter) and Retrieve
//! (15.6, N-GET, OPTIONAL for the origin server and implemented here). There
//! is no list/search transaction (15.3).
//!
//! The state machine is PS3.4 F.7: N-CREATE must set (0040,0252) to
//! `IN PROGRESS`; a final status is `COMPLETED` or `DISCONTINUED`; the
//! Performed Series Sequence (0040,0340) must not be empty at a final status;
//! and (0040,0281) is OPTIONAL (3/3) and, when used, is created zero-length at
//! N-CREATE because a top-level attribute cannot be deleted later (PS3.4
//! F.7.2.1.1 note 5).
//!
//! Fully synchronous: no framework types, no `async`, no runtime handle.

#![forbid(unsafe_code)]

use clarus_core::workflow::{MppsRecord, MppsReference, WorkflowStore};

use crate::handlers::json_common::{JsonVal, err_msg_json, push_json_str};
use crate::handlers::ups::json::extract_tag;
use crate::handlers::ups_pure::{now_stamp, single_dataset};
use crate::handlers::workflow_response::{
    WorkflowErr, WorkflowResp, bad_request, duplicate_instance, not_found,
};
use crate::state::AppState;

/// The status a modicum N-CREATE must send (PS3.4 F.7.2.1: the SCU "shall only
/// send the N-CREATE request with Performed Procedure Step Status (0040,0252)
/// set to IN PROGRESS").
const STATUS_IN_PROGRESS: &str = "IN PROGRESS";
/// The two final statuses (PS3.4 F.7 state attributes).
const STATUS_COMPLETED: &str = "COMPLETED";
const STATUS_DISCONTINUED: &str = "DISCONTINUED";

/// The workflow store rooted at the configured `[workflow] dir`.
fn workflow_store(state: &AppState) -> WorkflowStore {
    WorkflowStore::new(std::path::PathBuf::from(
        &state.config.workflow.resolved_dir(),
    ))
}

/// Extract the list of (0040,0340) Performed Series Sequence items.
///
/// Each item carries a (0008,1150)/(0008,1155) reference pair (PS3.3 C.12.1).
/// An empty sequence returns `Some(vec![])` - the distinction the final-state
/// check depends on.
fn extract_performed_series(obj: &JsonVal) -> Option<Vec<MppsReference>> {
    let seq = obj.obj_get("00400340")?;
    let items = seq.obj_get("Value").and_then(|v| v.as_array())?;
    let mut out = Vec::new();
    for item in items {
        let sop_class_uid = extract_tag(item, "00081150").unwrap_or_default();
        let sop_instance_uid = extract_tag(item, "00081155").unwrap_or_default();
        // A reference with an empty instance UID names nothing (the same rule
        // the UPS output list applies), so it is not stored.
        if sop_instance_uid.is_empty() {
            continue;
        }
        out.push(MppsReference {
            sop_class_uid,
            sop_instance_uid,
        });
    }
    Some(out)
}

/// Read (0040,0270) > (0020,000D) - the Study Instance UID lives INSIDE the
/// Scheduled Step Attributes Sequence, not at the top level (PS3.4 F.7).
fn extract_study_uid(obj: &JsonVal) -> Option<String> {
    let seq = obj.obj_get("00400270")?;
    let items = seq.obj_get("Value").and_then(|v| v.as_array())?;
    items.first().and_then(|item| extract_tag(item, "0020000D"))
}

/// Read the discontinuation reason out of (0040,0281) > (0040,0282).
///
/// (0040,0281) is a Code Sequence: the reason is the Code Value of its first
/// item. Reading the outer element with the scalar helper cannot work, because
/// its `Value` holds items, not strings.
fn extract_code_reason(obj: &JsonVal) -> Option<String> {
    let seq = obj.obj_get("00400281")?;
    let items = seq.obj_get("Value").and_then(|v| v.as_array())?;
    items.first().and_then(|item| extract_tag(item, "00400282"))
}

/// `POST /dicomweb/modality-performed-procedure-steps/{mppsUID}` - N-CREATE.
pub fn create(
    state: &AppState,
    mpps_uid: &str,
    body: &[u8],
    accept: Option<&str>,
    keep_alive: bool,
) -> Result<WorkflowResp, WorkflowErr> {
    let _ = keep_alive;
    if !state.config.workflow.enabled {
        return Err(service_disabled());
    }
    let parsed = crate::handlers::json_common::parse_json(body)
        .ok_or_else(|| bad_request("Body must be a DICOMweb JSON dataset"))?;
    let obj = single_dataset(&parsed)
        .ok_or_else(|| bad_request("Body must be a DICOMweb JSON array of one dataset"))?;

    // (0040,0252) is Type 1 at N-CREATE and must be IN PROGRESS (F.7.2.1).
    let status = extract_tag(obj, "00400252")
        .ok_or_else(|| bad_request("Performed Procedure Step Status (0040,0252) is required"))?;
    if status != STATUS_IN_PROGRESS {
        return Err(bad_request(&format!(
            "Performed Procedure Step Status must be IN PROGRESS at N-CREATE (0040,0252), got \
             '{status}' (PS3.4 F.7.2.1)"
        )));
    }

    // A record that already exists is a duplicate SOP Instance (0111H).
    let store = workflow_store(state);
    if store.find_mpps(mpps_uid).map_err(store_err)?.is_some() {
        return Err(duplicate_instance(mpps_uid));
    }

    let rec = MppsRecord {
        mpps_uid: mpps_uid.to_string(),
        status: status.clone(),
        modality: extract_tag(obj, "00080060"),
        station_ae: extract_tag(obj, "00400241"),
        station_name: extract_tag(obj, "00400242"),
        start_date: extract_tag(obj, "00400244"),
        start_time: extract_tag(obj, "00400245"),
        end_date: extract_tag(obj, "00400250"),
        end_time: extract_tag(obj, "00400251"),
        study_uid: extract_study_uid(obj),
        performed_series: extract_performed_series(obj),
        // (0040,0281) is OPTIONAL; when the client sends it, the reason text
        // is item 1 > (0040,0282) Code Value. The renderer emits the sequence
        // shape, so the reader accepts that shape and the flattened string
        // some clients send for the same fact.
        discontinuation_reason: extract_code_reason(obj).or_else(|| extract_tag(obj, "00400281")),
        created_at: Some(now_stamp()),
        updated_at: None,
    };
    store.insert_mpps(&rec).map_err(store_err)?;
    crate::clog_event!(
        crate::clog::Level::Info,
        "MPPS_EVENT",
        "",
        "mpps_uid={} op={} previous={} new={}",
        rec.mpps_uid,
        "create",
        "created",
        rec.status
    );
    render(201, &rec, accept)
}

/// `POST .../modality-performed-procedure-steps/{mppsUID}?update` - N-SET.
pub fn update(
    state: &AppState,
    mpps_uid: &str,
    body: &[u8],
    accept: Option<&str>,
    keep_alive: bool,
) -> Result<WorkflowResp, WorkflowErr> {
    let _ = keep_alive;
    if !state.config.workflow.enabled {
        return Err(service_disabled());
    }
    let parsed = crate::handlers::json_common::parse_json(body)
        .ok_or_else(|| bad_request("Body must be a DICOMweb JSON dataset"))?;
    let obj = single_dataset(&parsed)
        .ok_or_else(|| bad_request("Body must be a DICOMweb JSON array of one dataset"))?;

    let store = workflow_store(state);
    // N-SET on an unknown UID is refused: DICOMweb is not the DIMSE
    // vendor-tolerance path, so there is no implicit create.
    let existing = store
        .find_mpps(mpps_uid)
        .map_err(store_err)?
        .ok_or_else(|| not_found(&format!("MPPS '{mpps_uid}' not found")))?;

    // N-SET after a final status is refused (F.7.2.2: the state machine has no
    // transition out of COMPLETED or DISCONTINUED).
    if MppsRecord::is_final_status(&existing.status) {
        return Err(bad_request(&format!(
            "MPPS '{mpps_uid}' is already in the final state '{}'; no further N-SET is allowed",
            existing.status
        )));
    }

    // The N-SET body MAY carry a new status; absent keeps the current one.
    let new_status = extract_tag(obj, "00400252").unwrap_or_else(|| existing.status.clone());
    // Only IN PROGRESS -> COMPLETED/DISCONTINUED (or staying IN PROGRESS, a
    // legal no-op update) is permitted.
    if new_status != STATUS_IN_PROGRESS
        && new_status != STATUS_COMPLETED
        && new_status != STATUS_DISCONTINUED
    {
        return Err(bad_request(&format!(
            "Invalid Performed Procedure Step Status '{new_status}' (0040,0252)"
        )));
    }

    // Merge the N-SET body onto the existing record (attributes not sent are
    // left as they are, which is how N-SET works).
    let mut rec = existing;
    // Captured before the merge overwrites it, so the event line can report
    // the state the record was in when this N-SET arrived.
    let initial_status = rec.status.clone();
    rec.status = new_status.clone();
    if let Some(v) = extract_tag(obj, "00080060") {
        rec.modality = Some(v);
    }
    if let Some(v) = extract_tag(obj, "00400241") {
        rec.station_ae = Some(v);
    }
    if let Some(v) = extract_tag(obj, "00400242") {
        rec.station_name = Some(v);
    }
    if let Some(v) = extract_tag(obj, "00400244") {
        rec.start_date = Some(v);
    }
    if let Some(v) = extract_tag(obj, "00400245") {
        rec.start_time = Some(v);
    }
    if let Some(v) = extract_tag(obj, "00400250") {
        rec.end_date = Some(v);
    }
    if let Some(v) = extract_tag(obj, "00400251") {
        rec.end_time = Some(v);
    }
    if let Some(v) = extract_study_uid(obj) {
        rec.study_uid = Some(v);
    }
    if let Some(series) = extract_performed_series(obj) {
        rec.performed_series = Some(series);
    }
    if let Some(v) = extract_code_reason(obj).or_else(|| extract_tag(obj, "00400281")) {
        rec.discontinuation_reason = Some(v);
    }

    // At a final status the Performed Series Sequence must not be empty (F.7:
    // "a Series must exist for every Performed Procedure Step").
    if MppsRecord::is_final_status(&rec.status) {
        let non_empty = rec.performed_series.as_ref().is_some_and(|s| !s.is_empty());
        if !non_empty {
            return Err(bad_request(
                "Performed Series Sequence (0040,0340) must not be empty when the status becomes \
                 COMPLETED or DISCONTINUED (PS3.4 F.7)",
            ));
        }
    }

    rec.updated_at = Some(now_stamp());
    // The previous state is the one on disk before the merge, so the line
    // reports a real transition even when the body left the status unchanged.
    let previous = initial_status;
    store.put_mpps(&rec).map_err(store_err)?;
    crate::clog_event!(
        crate::clog::Level::Info,
        "MPPS_EVENT",
        "",
        "mpps_uid={} op={} previous={} new={}",
        rec.mpps_uid,
        "update",
        previous,
        rec.status
    );
    render(200, &rec, accept)
}

/// `GET .../modality-performed-procedure-steps/{mppsUID}` - N-GET.
pub fn get(
    state: &AppState,
    mpps_uid: &str,
    accept: Option<&str>,
    keep_alive: bool,
) -> Result<WorkflowResp, WorkflowErr> {
    let _ = keep_alive;
    if !state.config.workflow.enabled {
        return Err(service_disabled());
    }
    let store = workflow_store(state);
    let rec = store
        .find_mpps(mpps_uid)
        .map_err(store_err)?
        .ok_or_else(|| not_found(&format!("MPPS '{mpps_uid}' not found")))?;
    render(200, &rec, accept)
}

/// The one response for create/update/get: JSON (default) or XML.
fn render(
    status: u16,
    rec: &MppsRecord,
    accept: Option<&str>,
) -> Result<WorkflowResp, WorkflowErr> {
    let content_type = crate::handlers::qido_pure::negotiate(accept);
    if content_type == crate::handlers::DICOM_XML_CONTENT_TYPE {
        return Ok(WorkflowResp {
            status,
            content_type: crate::handlers::DICOM_XML_CONTENT_TYPE,
            headers: Vec::new(),
            body: mpps_to_xml(rec).into_bytes(),
        });
    }
    let mut body = Vec::new();
    write_mpps_json(&mut body, rec);
    Ok(WorkflowResp {
        status,
        content_type: crate::handlers::DICOM_JSON_CONTENT_TYPE,
        headers: Vec::new(),
        body,
    })
}

/// A disabled surface answers 404, not 405: the resource is not there.
fn service_disabled() -> WorkflowErr {
    WorkflowErr {
        status: 404,
        body: err_msg_json(
            "Not Found",
            "The Modality Performed Procedure Step service is disabled",
        ),
    }
}

/// Map a store error onto a 500 with the underlying message.
fn store_err(e: clarus_core::workflow::WorkflowStoreError) -> WorkflowErr {
    WorkflowErr {
        status: 500,
        body: err_msg_json("Internal Server Error", &e.to_string()),
    }
}

/// Render one MPPS record as a DICOMweb JSON object (PS3.18 Appendix F).
fn write_mpps_json(out: &mut Vec<u8>, rec: &MppsRecord) {
    out.push(b'{');
    let mut wrote = false;
    macro_rules! emit {
        ($tag:expr, $vr:expr, $val:expr) => {
            if let Some(v) = $val {
                if wrote {
                    out.push(b',');
                }
                wrote = true;
                out.push(b'"');
                out.extend_from_slice($tag.as_bytes());
                out.extend_from_slice(b"\":{\"Value\":[");
                push_json_str(out, v);
                out.extend_from_slice(b"],\"vr\":\"");
                out.extend_from_slice($vr.as_bytes());
                out.extend_from_slice(b"\"}");
            }
        };
    }
    // Alphabetical tag order.
    emit!(
        "00080016",
        "UI",
        Some("1.2.840.10008.3.1.2.3.3") // MPPS SOP Class UID
    );
    emit!("00080018", "UI", Some(rec.mpps_uid.as_str()));
    emit!("00080060", "CS", rec.modality.as_deref());
    emit!("00400241", "AE", rec.station_ae.as_deref());
    emit!("00400242", "SH", rec.station_name.as_deref());
    emit!("00400244", "DA", rec.start_date.as_deref());
    emit!("00400245", "TM", rec.start_time.as_deref());
    emit!("00400250", "DA", rec.end_date.as_deref());
    emit!("00400251", "TM", rec.end_time.as_deref());
    // (0040,0252) is always present (it is the state of the record).
    emit!("00400252", "CS", Some(rec.status.as_str()));
    // (0040,0270) Scheduled Step Attributes Sequence > (0020,000D). The study
    // lives inside the sequence, never as a bare top-level key.
    if let Some(study_uid) = &rec.study_uid {
        if wrote {
            out.push(b',');
        }
        wrote = true;
        out.extend_from_slice(b"\"00400270\":{\"Value\":[{\"0020000D\":{\"Value\":[");
        push_json_str(out, study_uid);
        out.extend_from_slice(b"],\"vr\":\"UI\"}}],\"vr\":\"SQ\"}");
    }
    // (0040,0281) is a SEQUENCE whose first item carries the reason text in
    // (0040,0282) Code Value. It used to be rendered as a bare string under
    // vr SQ, which is not a legal DICOM JSON sequence at all: PS3.18 F.2.5
    // makes a sequence's Value an array of ITEMS, and a JSON string is not an
    // item. A reader that validates the payload rejects the whole instance,
    // so the shape has to match the VR the tag declares.
    if let Some(reason) = &rec.discontinuation_reason {
        if wrote {
            out.push(b',');
        }
        wrote = true;
        out.extend_from_slice(b"\"00400281\":{\"Value\":[{\"00400282\":{\"Value\":[");
        push_json_str(out, reason);
        out.extend_from_slice(b"],\"vr\":\"LO\"}}],\"vr\":\"SQ\"}");
    }
    // (0040,0340) the performed series references.
    if let Some(series) = &rec.performed_series {
        if wrote {
            out.push(b',');
        }
        wrote = true;
        out.extend_from_slice(b"\"00400340\":{\"Value\":[");
        for (i, r) in series.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.extend_from_slice(b"{\"00081150\":{\"Value\":[");
            push_json_str(out, &r.sop_class_uid);
            out.extend_from_slice(b"],\"vr\":\"UI\"},\"00081155\":{\"Value\":[");
            push_json_str(out, &r.sop_instance_uid);
            out.extend_from_slice(b"],\"vr\":\"UI\"}}");
        }
        out.extend_from_slice(b"],\"vr\":\"SQ\"}");
    }
    // The last write sets `wrote` without a reader; naming the read keeps the
    // shared macro (and every emit above it) free of a special case.
    let _ = wrote;
    out.push(b'}');
}

/// Render one MPPS record as a DICOM XML `NativeDicomModel`.
fn mpps_to_xml(rec: &MppsRecord) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
    // PS3.19 A.1 puts the DICOM XML elements in a named namespace, and a
    // namespace-aware reader matches on it: a bare `<NativeDicomModel>` is a
    // DIFFERENT element in no namespace, so the document was well-formed and
    // still unreadable to such a client. `instance_xml` already emits the
    // attribute; this path did not.
    out.push_str("<NativeDicomModel xmlns=\"http://dicom.nema.org/PS3.19/models/NativeDICOM\">");
    xml_value(&mut out, "00080016", "UI", Some("1.2.840.10008.3.1.2.3.3"));
    xml_value(&mut out, "00080018", "UI", Some(rec.mpps_uid.as_str()));
    xml_value(&mut out, "00080060", "CS", rec.modality.as_deref());
    xml_value(&mut out, "00400241", "AE", rec.station_ae.as_deref());
    xml_value(&mut out, "00400242", "SH", rec.station_name.as_deref());
    xml_value(&mut out, "00400244", "DA", rec.start_date.as_deref());
    xml_value(&mut out, "00400245", "TM", rec.start_time.as_deref());
    xml_value(&mut out, "00400250", "DA", rec.end_date.as_deref());
    xml_value(&mut out, "00400251", "TM", rec.end_time.as_deref());
    xml_value(&mut out, "00400252", "CS", Some(rec.status.as_str()));
    if let Some(study_uid) = &rec.study_uid {
        out.push_str("<DicomAttribute tag=\"00400270\" vr=\"SQ\" keyword=\"ScheduledStepAttributesSequence\">");
        out.push_str("<Item number=\"1\">");
        xml_value(&mut out, "0020000D", "UI", Some(study_uid));
        out.push_str("</Item></DicomAttribute>");
    }
    // Same sequence shape as the JSON renderer: vr="SQ" means Items, and the
    // reason text lives in (0040,0282) inside the first one (PS3.18 F.2.5
    // makes the DICOM-XML and JSON Models agree item for item).
    if let Some(reason) = &rec.discontinuation_reason {
        out.push_str(
            "<DicomAttribute tag=\"00400281\" vr=\"SQ\" \
             keyword=\"PerformedProcedureStepDiscontinuationReasonCodeSequence\">",
        );
        out.push_str("<Item number=\"1\">");
        xml_value(&mut out, "00400282", "LO", Some(reason));
        out.push_str("</Item></DicomAttribute>");
    }
    if let Some(series) = &rec.performed_series {
        out.push_str(
            "<DicomAttribute tag=\"00400340\" vr=\"SQ\" keyword=\"PerformedSeriesSequence\">",
        );
        for (i, r) in series.iter().enumerate() {
            out.push_str(&format!("<Item number=\"{}\">", i + 1));
            xml_value(&mut out, "00081150", "UI", Some(&r.sop_class_uid));
            xml_value(&mut out, "00081155", "UI", Some(&r.sop_instance_uid));
            out.push_str("</Item>");
        }
        out.push_str("</DicomAttribute>");
    }
    out.push_str("</NativeDicomModel>");
    out
}

/// Append a `DicomAttribute` with a single value child, escaped.
fn xml_value(out: &mut String, tag: &str, vr: &str, value: Option<&str>) {
    let Some(v) = value else {
        return;
    };
    out.push_str(&format!(
        "<DicomAttribute tag=\"{tag}\" vr=\"{vr}\" keyword=\"\">"
    ));
    out.push_str(&format!("<Value number=\"1\">{}</Value>", xml_escape(v)));
    out.push_str("</DicomAttribute>");
}

/// Escape the five XML predefined entities.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::json_common::parse_json;

    fn obj(json: &str) -> JsonVal {
        parse_json(json.as_bytes()).expect("valid json")
    }

    #[test]
    fn extract_performed_series_keeps_pairs_and_drops_empty_refs() {
        let v = obj(r#"{"00400340":{"vr":"SQ","Value":[
                 {"00081150":{"vr":"UI","Value":["1.2.3"]},"00081155":{"vr":"UI","Value":["4.5.6"]}},
                 {"00081150":{"vr":"UI","Value":["1.2.3"]},"00081155":{"vr":"UI","Value":[""]}}]}}"#);
        let series = extract_performed_series(&v).expect("Some");
        assert_eq!(series.len(), 1, "an empty instance UID names nothing");
        assert_eq!(series[0].sop_instance_uid, "4.5.6");
    }

    #[test]
    fn extract_performed_series_distinguishes_empty_from_absent() {
        assert!(extract_performed_series(&obj(r#"{}"#)).is_none());
        let empty = obj(r#"{"00400340":{"vr":"SQ","Value":[]}}"#);
        assert_eq!(extract_performed_series(&empty), Some(Vec::new()));
    }

    #[test]
    fn extract_study_uid_reads_inside_the_scheduled_step_sequence() {
        let v =
            obj(r#"{"00400270":{"vr":"SQ","Value":[{"0020000D":{"vr":"UI","Value":["1.2.9"]}}]}}"#);
        assert_eq!(extract_study_uid(&v).as_deref(), Some("1.2.9"));
        assert!(extract_study_uid(&obj(r#"{"0020000D":{"vr":"UI","Value":["1.2.9"]}}"#)).is_none());
    }

    fn record_with_reason(reason: &str) -> MppsRecord {
        MppsRecord {
            mpps_uid: "1.2.3".to_string(),
            status: "DISCONTINUED".to_string(),
            modality: None,
            station_ae: None,
            station_name: None,
            start_date: None,
            start_time: None,
            end_date: None,
            end_time: None,
            study_uid: None,
            performed_series: None,
            discontinuation_reason: Some(reason.to_string()),
            created_at: None,
            updated_at: None,
        }
    }

    /// (0040,0281) is a sequence, so its rendered `Value` must be items. The
    /// previous renderer put a JSON STRING under `"vr":"SQ"`, which no
    /// DICOM-JSON reader accepts as a sequence - the payload was well-formed
    /// JSON and still invalid as DICOM.
    #[test]
    fn the_discontinuation_reason_renders_as_a_sequence_item() {
        let rec = record_with_reason("Equipment failure");
        let mut body = Vec::new();
        write_mpps_json(&mut body, &rec);
        let text = String::from_utf8(body).expect("utf-8");
        assert!(
            text.contains(
                r#""00400281":{"Value":[{"00400282":{"Value":["Equipment failure"],"vr":"LO"}}],"vr":"SQ"}"#
            ),
            "sequence must carry an item, got {text}"
        );
        // The XML model must agree with the JSON model, item for item.
        let xml = mpps_to_xml(&rec);
        assert!(
            xml.contains("<DicomAttribute tag=\"00400281\" vr=\"SQ\""),
            "{xml}"
        );
        assert!(
            xml.contains("<Value number=\"1\">Equipment failure</Value>"),
            "the reason is the item's Code Value, got {xml}"
        );
    }

    /// The reader accepts the shape the writer emits, and the flattened string
    /// older clients send for the same fact; a reason the client never sent
    /// stays absent rather than becoming an empty sequence.
    #[test]
    fn the_discontinuation_reason_round_trips_through_both_shapes() {
        let sequenced = obj(
            r#"{"00400281":{"vr":"SQ","Value":[{"00400282":{"vr":"LO","Value":["Broken"]}}]}}"#,
        );
        assert_eq!(extract_code_reason(&sequenced).as_deref(), Some("Broken"));
        let flattened = obj(r#"{"00400281":{"vr":"LO","Value":["Broken"]}}"#);
        assert!(extract_code_reason(&flattened).is_none());
        assert_eq!(
            extract_tag(&flattened, "00400281").as_deref(),
            Some("Broken")
        );
        assert!(extract_code_reason(&obj(r#"{}"#)).is_none());
    }
}
