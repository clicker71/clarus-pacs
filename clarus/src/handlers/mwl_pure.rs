// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! MWL-RS pure core (PS3.18 §14).
//!
//! Search is the ONLY transaction of the Modality Scheduled Procedure Step
//! Service: 14.3 states that no Create exists, in DIMSE or in DICOMweb, so
//! this module is the whole surface. Search is the C-FIND analog (14.4) and
//! the matching/return keys are PS3.4 Table K.6-1.
//!
//! Fully synchronous: no framework types, no `async`, no runtime handle. The
//! dispatch wrapper passes the already-parsed query string and Accept header,
//! and renders whatever this returns.

#![forbid(unsafe_code)]

use clarus_core::idx::ngram::fuzzy_normalize;
use clarus_core::query::bitap::bitap_match;
use clarus_core::workflow::{SpsRecord, WorkflowStore};

use crate::handlers::json_common::{err_msg_json, push_json_str};
use crate::handlers::workflow_response::{WorkflowErr, WorkflowResp, bad_request};
use crate::state::AppState;

/// The MWL search query: the seven REQUIRED matching keys of Table K.6-1 plus
/// the nine optional ones this server supports, and the two control parameters.
///
/// Every field is a matching key. The sequence-qualified spellings of the SPS
/// keys route here too (see [`parse_query`]): the standard types the SPS
/// attributes as one Item of (0040,0100) with Sequence Matching, so a caller
/// may name them either bare or through the sequence path and both state one
/// fact.
#[derive(Debug, Default)]
pub struct MwlQuery {
    /// (0040,0001) Scheduled Station AE Title - REQUIRED key.
    pub station_aet: Option<String>,
    /// (0040,0002) Scheduled Procedure Step Start Date - REQUIRED key.
    pub sps_start_date: Option<String>,
    /// (0040,0003) Scheduled Procedure Step Start Time - REQUIRED key.
    pub sps_start_time: Option<String>,
    /// (0008,0060) Modality - REQUIRED key.
    pub modality: Option<String>,
    /// (0040,0006) Scheduled Performing Physician's Name - REQUIRED key.
    pub performing_physician: Option<String>,
    /// (0010,0010) Patient's Name - REQUIRED key.
    pub patient_name: Option<String>,
    /// (0010,0020) Patient ID - REQUIRED key.
    pub patient_id: Option<String>,
    /// (0008,0050) Accession Number - optional key.
    pub accession_number: Option<String>,
    /// (0040,1001) Requested Procedure ID - optional key.
    pub requested_procedure_id: Option<String>,
    /// (0032,1060) Requested Procedure Description - optional key.
    pub requested_procedure_description: Option<String>,
    /// (0020,000D) Study Instance UID - optional key.
    pub study_uid: Option<String>,
    /// (0010,0030) Patient's Birth Date - optional key.
    pub birth_date: Option<String>,
    /// (0010,0040) Patient's Sex - optional key.
    pub sex: Option<String>,
    /// (0032,1032) Requesting Physician - optional key.
    pub requesting_physician: Option<String>,
    /// (0008,0090) Referring Physician's Name - optional key.
    pub referring_physician: Option<String>,
    /// (0040,0009) Scheduled Procedure Step ID - optional key.
    pub sps_id: Option<String>,
    /// Maximum results. `None` = the server default (`[workflow]
    /// mwl_default_limit`).
    pub limit: Option<u64>,
    /// Zero-based offset (PS3.18 Table 8.3.4-1 control parameter).
    pub offset: Option<u64>,
    /// (control) `fuzzymatching=true` applies Levenshtein-1 fuzzy matching to
    /// the Person Name keys - Patient's Name (0010,0010), Scheduled Performing
    /// Physician's Name (0040,0006), Referring Physician's Name (0008,0090) -
    /// per PS3.18 8.3.4.2. Default `false`: exact single-value matching.
    pub fuzzymatching: bool,
}

/// Parse the MWL search query string.
///
/// Each matching key is accepted in its PS3.6 keyword form and in its hex-tag
/// form, exactly as the UPS search does (PS3.18 8.3.4 defines the key by
/// keyword). An unknown or misspelled key is REFUSED rather than ignored: a
/// dropped key widens the result set to the whole worklist, which a caller
/// cannot tell from a genuinely broad match - and for a worklist that also
/// leaks every patient it holds. dcm4chee-arc refuses the same request, so the
/// loud answer is the interoperable one.
///
/// The SPS keys are also accepted in their sequence-qualified spelling
/// (`ScheduledProcedureStepSequence.…` / `00400100.…`), because PS3.4 K.6-1
/// retrieves them through (0040,0100) with Sequence Matching; the sequence
/// segment lives in the parameter NAME and reaches the same field.
fn parse_query(query_str: &str) -> Result<MwlQuery, String> {
    let mut q = MwlQuery::default();
    for pair in query_str.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let key = crate::handlers::json_common::form_decode_str(k);
        let val = crate::handlers::json_common::form_decode_str(v);
        match key.as_str() {
            // (0040,0001) Scheduled Station AE Title, bare and sequence-qualified.
            "00400001"
            | "ScheduledStationAETitle"
            | "00400100.00400001"
            | "ScheduledProcedureStepSequence.ScheduledStationAETitle" => q.station_aet = Some(val),
            // (0040,0002)/(0040,0003) start date/time, bare and qualified.
            "00400002"
            | "ScheduledProcedureStepStartDate"
            | "00400100.00400002"
            | "ScheduledProcedureStepSequence.ScheduledProcedureStepStartDate" => {
                q.sps_start_date = Some(val)
            }
            "00400003"
            | "ScheduledProcedureStepStartTime"
            | "00400100.00400003"
            | "ScheduledProcedureStepSequence.ScheduledProcedureStepStartTime" => {
                q.sps_start_time = Some(val)
            }
            "00080060"
            | "Modality"
            | "00400100.00080060"
            | "ScheduledProcedureStepSequence.Modality" => q.modality = Some(val),
            // (0040,0006) Scheduled Performing Physician's Name.
            "00400006"
            | "ScheduledPerformingPhysicianName"
            | "ScheduledPerformingPhysiciansName"
            | "00400100.00400006"
            | "ScheduledProcedureStepSequence.ScheduledPerformingPhysicianName" => {
                q.performing_physician = Some(val)
            }
            "00100010" | "PatientName" => q.patient_name = Some(val),
            "00100020" | "PatientID" => q.patient_id = Some(val),
            "00080050" | "AccessionNumber" => q.accession_number = Some(val),
            "00401001" | "RequestedProcedureID" => q.requested_procedure_id = Some(val),
            "00321060" | "RequestedProcedureDescription" => {
                q.requested_procedure_description = Some(val)
            }
            "0020000D" | "StudyInstanceUID" => q.study_uid = Some(val),
            "00100030" | "PatientBirthDate" => q.birth_date = Some(val),
            "00100040" | "PatientSex" => q.sex = Some(val),
            "00321032" | "RequestingPhysician" => q.requesting_physician = Some(val),
            "00080090" | "ReferringPhysicianName" | "ReferringPhysiciansName" => {
                q.referring_physician = Some(val)
            }
            "00400009"
            | "ScheduledProcedureStepID"
            | "00400100.00400009"
            | "ScheduledProcedureStepSequence.ScheduledProcedureStepID" => q.sps_id = Some(val),
            // PS3.18 8.3.4 Table 8.3.4-1: limit/offset are uint. A present
            // value that is not an unsigned integer is an invalid query
            // parameter (400, PS3.18 8.5), not a silently ignored one.
            "limit" => {
                q.limit =
                    Some(val.parse::<u64>().map_err(|_| {
                        format!("limit must be a non-negative integer (got \"{val}\")")
                    })?)
            }
            "offset" => {
                q.offset = Some(val.parse::<u64>().map_err(|_| {
                    format!("offset must be a non-negative integer (got \"{val}\")")
                })?)
            }
            // PS3.18 Table 8.3.4-1 control parameters. `fuzzymatching` is
            // honoured (8.3.4.2: fuzzy matching of Person Name attributes);
            // the rest are accepted and ignored only where ignoring them
            // cannot change the answer: `includefield` is satisfied because
            // the render returns every attribute the record holds, and the
            // two `*valuematching` parameters are the default literal-match
            // behaviour.
            "fuzzymatching" => q.fuzzymatching = val.eq_ignore_ascii_case("true"),
            "includefield" | "emptyvaluematching" | "multiplevaluematching" => {}
            other => {
                return Err(format!(
                    "Unknown or misspelled query parameter '{other}': matching keys are \
                     PS3.6 keywords or hex tags (PS3.18 8.3.4)"
                ));
            }
        }
    }
    Ok(q)
}

/// Levenshtein-1 Person Name match (PS3.18 8.3.4.2 fuzzy path).
///
/// Mirrors the QIDO `Predicate::Fuzzy` path: PN separators (`^`/`=`) fold to
/// a space so a hand-typed space query aligns with a stored `^`/`=` name, both
/// sides are lowercased (Bitap is ASCII-case-insensitive only), and
/// `bitap_match` then tests the one-edit window.
fn fuzzy_pn(pattern: &str, name: &str) -> bool {
    bitap_match(
        &fuzzy_normalize(pattern).to_lowercase(),
        &fuzzy_normalize(name).to_lowercase(),
    )
}

/// Whether a record satisfies every filter (AND semantics).
fn matches(rec: &SpsRecord, q: &MwlQuery) -> bool {
    macro_rules! eq_opt {
        ($q:expr, $field:expr) => {
            if let Some(v) = $q.as_deref()
                && $field.as_deref() != Some(v)
            {
                return false;
            }
        };
    }
    // The seven required keys are always present in a stored record. The
    // Person Name keys (PatientName, PerformingPhysicianName) match exactly
    // by default and within one Levenshtein edit when `fuzzymatching=true`
    // (8.3.4.2); the other keys compare exactly (wild-card and range matching
    // are a client-side request this origin declines).
    if let Some(v) = q.station_aet.as_deref()
        && rec.station_aet != v
    {
        return false;
    }
    if let Some(v) = q.sps_start_date.as_deref()
        && rec.sps_start_date != v
    {
        return false;
    }
    if let Some(v) = q.sps_start_time.as_deref()
        && rec.sps_start_time != v
    {
        return false;
    }
    // Modality is a CS value: its case is not significant.
    if let Some(v) = q.modality.as_deref()
        && !rec.modality.eq_ignore_ascii_case(v)
    {
        return false;
    }
    if let Some(v) = q.performing_physician.as_deref() {
        let hit = if q.fuzzymatching {
            fuzzy_pn(v, &rec.scheduled_performing_physician)
        } else {
            rec.scheduled_performing_physician == v
        };
        if !hit {
            return false;
        }
    }
    if let Some(v) = q.patient_name.as_deref() {
        let hit = if q.fuzzymatching {
            fuzzy_pn(v, &rec.patient_name)
        } else {
            rec.patient_name == v
        };
        if !hit {
            return false;
        }
    }
    if let Some(v) = q.patient_id.as_deref()
        && rec.patient_id != v
    {
        return false;
    }
    eq_opt!(q.accession_number, rec.accession_number);
    eq_opt!(q.requested_procedure_id, rec.requested_procedure_id);
    eq_opt!(
        q.requested_procedure_description,
        rec.requested_procedure_description
    );
    eq_opt!(q.study_uid, rec.study_uid);
    eq_opt!(q.birth_date, rec.birth_date);
    eq_opt!(q.sex, rec.sex);
    eq_opt!(q.requesting_physician, rec.requesting_physician);
    if let Some(v) = q.referring_physician.as_deref() {
        let hit = match rec.referring_physician.as_deref() {
            Some(have) => {
                if q.fuzzymatching {
                    fuzzy_pn(v, have)
                } else {
                    have == v
                }
            }
            None => false,
        };
        if !hit {
            return false;
        }
    }
    eq_opt!(q.sps_id, rec.sps_id);
    true
}

/// The workflow store rooted at the configured `[workflow] dir`.
pub(crate) fn workflow_store(state: &AppState) -> WorkflowStore {
    WorkflowStore::new(std::path::PathBuf::from(
        &state.config.workflow.resolved_dir(),
    ))
}

/// `GET /dicomweb/modality-scheduled-procedure-steps` - MWL Search.
pub fn search(
    state: &AppState,
    query_str: Option<&str>,
    accept: Option<&str>,
    keep_alive: bool,
    service: &str,
) -> Result<WorkflowResp, WorkflowErr> {
    let _ = keep_alive;
    if !state.config.workflow.enabled {
        return Err(WorkflowErr {
            status: 404,
            body: err_msg_json(
                "Not Found",
                "The Modality Scheduled Procedure Step service is disabled",
            ),
        });
    }
    let mut query = match query_str {
        Some(s) if !s.is_empty() => parse_query(s).map_err(|msg| bad_request(&msg))?,
        _ => MwlQuery::default(),
    };

    // The response bound: a client `limit` is CLAMPED to `mwl_max_responses`
    // (the hard ceiling), and an absent `limit` takes `mwl_default_limit`
    // (0 = unlimited), which the ceiling then clamps as well.
    //
    // `limit=0` asks for no results, not for no bound. Reading it as
    // "unlimited" inverted the request: a client that wanted an empty answer
    // got the LARGEST answer the server would ever send. The ceiling is the
    // zero the request already asked for, so it is also the clamp.
    let ceiling = state.config.workflow.mwl_max_responses;
    let effective_limit = match query.limit {
        Some(l) => Some(l.min(ceiling)),
        None => match state.config.workflow.mwl_default_limit {
            0 => None,
            d => Some(d.min(ceiling)),
        },
    };
    query.limit = effective_limit;

    let store = workflow_store(state);
    let records = store.all_sps().map_err(|e| WorkflowErr {
        status: 500,
        body: err_msg_json("Internal Server Error", &e.to_string()),
    })?;

    let mut matched: Vec<SpsRecord> = records.into_iter().filter(|r| matches(r, &query)).collect();
    // Deterministic order: start datetime, then UID - a worklist is read in
    // schedule order, and the UID breaks ties so two reads agree.
    matched.sort_by(|a, b| {
        (
            a.sps_start_date.as_str(),
            a.sps_start_time.as_str(),
            &a.sps_uid,
        )
            .cmp(&(
                b.sps_start_date.as_str(),
                b.sps_start_time.as_str(),
                &b.sps_uid,
            ))
    });

    // offset then limit (the search control parameters, Table 8.3.4-1).
    // The pre-pagination match count is what PS3.18 8.3.4.4.1 needs for the
    // truncation Warning, so it is captured before the page is cut.
    let total_matches = matched.len();
    let offset = query.offset.unwrap_or(0) as usize;
    if offset < matched.len() {
        matched.drain(..offset);
    } else {
        matched.clear();
    }
    if let Some(l) = query.limit {
        matched.truncate(l as usize);
    }
    let remaining = (total_matches as u64)
        .saturating_sub(query.offset.unwrap_or(0))
        .saturating_sub(matched.len() as u64);

    let mut headers = Vec::new();
    if remaining > 0 {
        headers.push((
            "Warning",
            format!(
                "299 {service}: There are {remaining} additional results that can be requested"
            ),
        ));
    }

    // One line per search, emitted HERE - at the single place the answer is
    // known - so a line describes every search exactly once whatever the
    // accepted media type. The station is capped to 16 chars (AE, PS3.5
    // Table 6.2-1) so a caller cannot grow the line with a long value.
    let station: String = query
        .station_aet
        .as_deref()
        .unwrap_or("")
        .chars()
        .take(16)
        .collect();
    crate::clog_event!(
        crate::clog::Level::Info,
        "MWL_SEARCH",
        "",
        "station={} n_matched={}",
        station,
        matched.len()
    );

    let content_type = crate::handlers::qido_pure::negotiate(accept);
    if content_type == crate::handlers::DICOM_XML_CONTENT_TYPE {
        // PS3.18 14.1.3 Table 14.1.3-1 makes the XML form of this response
        // multipart/related; type="application/dicom+xml": a MWL Search
        // returns N scheduled procedure steps, and each one is its own DICOM
        // XML document. A single model root listing every record was the only
        // shape one document could take, and it has no boundary between
        // records that a parser can act on - the records are then
        // indistinguishable from one another's attributes.
        //
        // One record therefore means one part, and the single-record case
        // takes the same path: the part framing is what the standard asks for
        // regardless of N, and a client that reads multipart for N>1 must not
        // be handed a bare document for N==1.
        let boundary = clarus_adapters::boundary_id();
        // `WorkflowResp::content_type` is a `&'static str`; the multipart media
        // type carries a per-response boundary, so it travels as a header
        // instead and the static field stays empty (the renderer skips it).
        let mut multipart_headers = headers;
        multipart_headers.push(("Content-Type", multipart_content_type(&boundary)));
        let body = multipart_body(&matched, &boundary).into_bytes();
        return Ok(WorkflowResp {
            status: 200,
            content_type: "",
            headers: multipart_headers,
            body,
        });
    }
    let mut body = Vec::new();
    body.push(b'[');
    for (i, rec) in matched.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        write_sps_json(&mut body, rec);
    }
    body.push(b']');
    Ok(WorkflowResp {
        status: 200,
        content_type: crate::handlers::DICOM_JSON_CONTENT_TYPE,
        headers,
        body,
    })
}

/// Render one SPS record as a DICOMweb JSON object (PS3.18 Appendix F).
///
/// The response is SPS-level - the standard returns "a list of matching
/// scheduled procedure steps" (14.4) - so the SPS attributes are emitted at
/// the top level of the item, NOT wrapped in a Study/Series/Instance
/// hierarchy the way QIDO does.
fn write_sps_json(out: &mut Vec<u8>, rec: &SpsRecord) {
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
    // Alphabetical tag order, the DICOMweb JSON convention this tree follows.
    emit!("00080050", "SH", rec.accession_number.as_deref());
    emit!("00080060", "CS", Some(rec.modality.as_str()));
    emit!("00080090", "PN", rec.referring_physician.as_deref());
    emit!("00100010", "PN", Some(rec.patient_name.as_str()));
    emit!("00100020", "LO", Some(rec.patient_id.as_str()));
    emit!("00100030", "DA", rec.birth_date.as_deref());
    emit!("00100040", "CS", rec.sex.as_deref());
    emit!("0020000D", "UI", rec.study_uid.as_deref());
    emit!("00321032", "PN", rec.requesting_physician.as_deref());
    emit!(
        "00321060",
        "LO",
        rec.requested_procedure_description.as_deref()
    );
    emit!("00400001", "AE", Some(rec.station_aet.as_str()));
    emit!("00400002", "DA", Some(rec.sps_start_date.as_str()));
    emit!("00400003", "TM", Some(rec.sps_start_time.as_str()));
    emit!(
        "00400006",
        "PN",
        Some(rec.scheduled_performing_physician.as_str())
    );
    emit!("00400009", "SH", rec.sps_id.as_deref());
    emit!("00401001", "SH", rec.requested_procedure_id.as_deref());
    // The last emit sets `wrote` without a reader; naming the read keeps the
    // shared macro (and every emit above it) free of a special case.
    let _ = wrote;
    out.push(b'}');
}

/// Render ONE SPS record as a DICOM XML `NativeDicomModel` document.
///
/// PS3.18 14.1.3 makes `application/dicom+xml` Required for the service, so
/// the render is not optional. The shape is the one PS3.18 Annex F.3 fixes: a
/// `NativeDicomModel` whose `DicomAttribute`s carry the SPS tags at the model
/// root, because the response is SPS-level.
///
/// ONE record per document, and the documents are framed as parts by the
/// caller (see `search`). An earlier revision concatenated every record into a
/// single root separated by a tagless `<DicomAttribute/>`: PS3.19 A.1 defines
/// no such element, so it was a private marker that no conformant reader could
/// be expected to honour, and without it the records' attributes were
/// indistinguishable.
fn sps_to_xml(records: &[SpsRecord]) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
    // PS3.19 A.1.1 puts every NativeDicomModel element in this namespace; a
    // model without it is unqualified XML that a schema-aware reader is free
    // to reject as "not DICOM" rather than silently accept.
    out.push_str(&format!(
        "<NativeDicomModel xmlns=\"{}\">",
        NATIVE_DICOM_MODEL_NS
    ));
    for rec in records {
        xml_attr(&mut out, "00080050", "SH", rec.accession_number.as_deref());
        xml_attr(&mut out, "00080060", "CS", Some(rec.modality.as_str()));
        xml_attr(
            &mut out,
            "00080090",
            "PN",
            rec.referring_physician.as_deref(),
        );
        xml_attr(&mut out, "00100010", "PN", Some(rec.patient_name.as_str()));
        xml_attr(&mut out, "00100020", "LO", Some(rec.patient_id.as_str()));
        xml_attr(&mut out, "00100030", "DA", rec.birth_date.as_deref());
        xml_attr(&mut out, "00100040", "CS", rec.sex.as_deref());
        xml_attr(&mut out, "0020000D", "UI", rec.study_uid.as_deref());
        xml_attr(
            &mut out,
            "00321032",
            "PN",
            rec.requesting_physician.as_deref(),
        );
        xml_attr(
            &mut out,
            "00321060",
            "LO",
            rec.requested_procedure_description.as_deref(),
        );
        xml_attr(&mut out, "00400001", "AE", Some(rec.station_aet.as_str()));
        xml_attr(
            &mut out,
            "00400002",
            "DA",
            Some(rec.sps_start_date.as_str()),
        );
        xml_attr(
            &mut out,
            "00400003",
            "TM",
            Some(rec.sps_start_time.as_str()),
        );
        xml_attr(
            &mut out,
            "00400006",
            "PN",
            Some(rec.scheduled_performing_physician.as_str()),
        );
        xml_attr(&mut out, "00400009", "SH", rec.sps_id.as_deref());
        xml_attr(
            &mut out,
            "00401001",
            "SH",
            rec.requested_procedure_id.as_deref(),
        );
    }
    out.push_str("</NativeDicomModel>");
    out
}

/// The response media type PS3.18 14.1.3 names for the XML form.
fn multipart_content_type(boundary: &str) -> String {
    format!(
        "multipart/related; type=\"{}\"; boundary=\"{boundary}\"",
        crate::handlers::DICOM_XML_CONTENT_TYPE
    )
}

/// Assemble the multipart/related body: one `application/dicom+xml` part per
/// scheduled procedure step.
///
/// RFC 2046 5.1.1 puts no CRLF before the FIRST delimiter and a CRLF before
/// every later one, which is why the separator depends on whether a part has
/// already been written. An archival body with zero steps still carries the
/// terminating delimiter: a multipart entity with no parts is legal, and it
/// parses as "no records" instead of failing on the media type the client
/// asked for.
fn multipart_body(records: &[SpsRecord], boundary: &str) -> String {
    if records.is_empty() {
        return format!("--{boundary}--\r\n");
    }
    let mut body = String::new();
    for rec in records {
        let xml = sps_to_xml(std::slice::from_ref(rec));
        if body.is_empty() {
            body.push_str(&format!("--{boundary}\r\n"));
        } else {
            body.push_str(&format!("\r\n--{boundary}\r\n"));
        }
        body.push_str("Content-Type: application/dicom+xml\r\n");
        body.push_str(&format!("Content-Length: {}\r\n\r\n", xml.len()));
        body.push_str(&xml);
    }
    body.push_str(&format!("\r\n--{boundary}--\r\n"));
    body
}

/// Append one `DicomAttribute` with a single `PersonName` or `Value` child.
fn xml_attr(out: &mut String, tag: &str, vr: &str, value: Option<&str>) {
    let Some(v) = value else {
        return;
    };
    let escaped = xml_escape(v);
    out.push_str(&format!(
        "<DicomAttribute tag=\"{tag}\" vr=\"{vr}\" keyword=\"\">"
    ));
    if vr == "PN" {
        out.push_str("<PersonName number=\"1\">");
        write_person_name_components(v, out);
        out.push_str("</PersonName>");
    } else {
        out.push_str(&format!("<Value number=\"1\">{escaped}</Value>"));
    }
    out.push_str("</DicomAttribute>");
}

/// The XML namespace PS3.19 A.1.1 assigns to the model. Pinned here so the
/// renderer has one place to name it.
const NATIVE_DICOM_MODEL_NS: &str = "http://dicom.nema.org/PS3.19/models/NativeDICOM";

/// Write a `PN` value as its PS3.19 A.1.2 component groups.
///
/// A PN is `family^given^middle^prefix^suffix` per group (PS3.5 6.2), and the
/// groups are separated by `=`. Rendering the whole raw string inside
/// `<Alphabetic>` loses the boundary: a consumer reading `FamilyName` gets
/// nothing, and an Ideographic or Phonetic group disappears entirely - which
/// is the common case for CJK names, not an edge one. Empty components are
/// omitted; PS3.19 defines no element for them.
fn write_person_name_components(raw: &str, out: &mut String) {
    for (group_name, group) in [
        ("Alphabetic", raw.split('=').next()),
        ("Ideographic", raw.split('=').nth(1)),
        ("Phonetic", raw.split('=').nth(2)),
    ] {
        let Some(group) = group.filter(|g| !g.is_empty()) else {
            continue;
        };
        out.push('<');
        out.push_str(group_name);
        out.push('>');
        let mut parts = group.split('^');
        for name in ["FamilyName", "GivenName", "MiddleName", "Prefix", "Suffix"] {
            let Some(part) = parts.next().filter(|p| !p.is_empty()) else {
                continue;
            };
            out.push('<');
            out.push_str(name);
            out.push('>');
            out.push_str(&xml_escape(part));
            out.push_str("</");
            out.push_str(name);
            out.push('>');
        }
        out.push_str("</");
        out.push_str(group_name);
        out.push('>');
    }
}

/// Escape the five XML predefined entities, so a name with `&` stays parseable.
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

    #[test]
    fn parse_query_accepts_keywords_and_hex() {
        let q =
            parse_query("ScheduledStationAETitle=CT01&00400002=20261011&Modality=CT&PatientID=P1")
                .expect("all keys are standard");
        assert_eq!(q.station_aet.as_deref(), Some("CT01"));
        assert_eq!(q.sps_start_date.as_deref(), Some("20261011"));
        assert_eq!(q.modality.as_deref(), Some("CT"));
        assert_eq!(q.patient_id.as_deref(), Some("P1"));
    }

    #[test]
    fn parse_query_accepts_the_sequence_qualified_sps_keys() {
        // PS3.4 K.6-1 retrieves the SPS attributes through (0040,0100), so the
        // sequence-qualified spelling must reach the SAME field - in both the
        // hex path form and the keyword path form.
        let q = parse_query("00400100.00400001=CT01&ScheduledProcedureStepSequence.Modality=CT")
            .expect("qualified keys");
        assert_eq!(q.station_aet.as_deref(), Some("CT01"));
        assert_eq!(q.modality.as_deref(), Some("CT"));

        let hex = parse_query("00400100.00400002=20261011").expect("qualified hex");
        assert_eq!(hex.sps_start_date.as_deref(), Some("20261011"));

        // The keyword-qualified form of a required SPS key reaches the same
        // field the bare keyword does.
        let keyword =
            parse_query("ScheduledProcedureStepSequence.ScheduledStationAETitle=CT02").unwrap();
        assert_eq!(keyword.station_aet.as_deref(), Some("CT02"));
    }

    #[test]
    fn parse_query_refuses_an_unknown_key() {
        let err = parse_query("patientid=x").unwrap_err();
        assert!(
            err.contains("patientid"),
            "the message names the key: {err}"
        );
    }

    #[test]
    fn parse_query_accepts_the_control_parameters() {
        // Table 8.3.4-1 marks every control parameter M for the origin server.
        parse_query("fuzzymatching=true&includefield=all&emptyvaluematching=false&limit=10")
            .expect("control parameters are not matching keys");
    }

    #[test]
    fn matches_compares_the_required_keys() {
        let rec = SpsRecord {
            sps_uid: "1.2.3".into(),
            station_aet: "CT01".into(),
            sps_start_date: "20261011".into(),
            sps_start_time: "090000".into(),
            modality: "CT".into(),
            scheduled_performing_physician: "SMITH^JOHN".into(),
            patient_name: "IVANOV^IVAN".into(),
            patient_id: "P1".into(),
            ..Default::default()
        };
        let mut q = MwlQuery::default();
        assert!(matches(&rec, &q), "an empty query matches everything");
        q.modality = Some("ct".into());
        assert!(matches(&rec, &q), "a modality is a CS value: case free");
        q.modality = Some("MR".into());
        assert!(!matches(&rec, &q));
        q.modality = None;
        q.patient_id = Some("P2".into());
        assert!(!matches(&rec, &q));
    }

    #[test]
    fn matches_rejects_a_record_with_no_optional_key() {
        let rec = SpsRecord {
            sps_uid: "1.2.3".into(),
            modality: "CT".into(),
            ..Default::default()
        };
        let q = MwlQuery {
            accession_number: Some("ACC-1".into()),
            ..MwlQuery::default()
        };
        assert!(!matches(&rec, &q), "None never equals a stated value");
    }

    #[test]
    fn parse_query_reads_the_fuzzymatching_flag() {
        assert!(parse_query("fuzzymatching=true").unwrap().fuzzymatching);
        assert!(
            parse_query("fuzzymatching=TRUE").unwrap().fuzzymatching,
            "the ABNF literal is case-insensitive"
        );
        assert!(!parse_query("fuzzymatching=false").unwrap().fuzzymatching);
        assert!(
            !parse_query("").unwrap().fuzzymatching,
            "absent means false (8.3.4.2)"
        );
    }

    #[test]
    fn matches_fuzzy_person_names_when_requested() {
        let rec = SpsRecord {
            sps_uid: "1.2.3".into(),
            station_aet: "CT01".into(),
            sps_start_date: "20261011".into(),
            sps_start_time: "090000".into(),
            modality: "CT".into(),
            scheduled_performing_physician: "SMITH^JOHN".into(),
            patient_name: "IVANOV^IVAN".into(),
            patient_id: "P1".into(),
            ..Default::default()
        };
        // Default (false) is exact single-value matching.
        let exact = MwlQuery {
            patient_name: Some("IVANOV^IVAN".into()),
            ..Default::default()
        };
        assert!(matches(&rec, &exact));
        let exact_miss = MwlQuery {
            patient_name: Some("IVANOV^IVN".into()),
            ..Default::default()
        };
        assert!(
            !matches(&rec, &exact_miss),
            "one edit is not an exact match"
        );

        // One edit matches only when fuzzy is on.
        let fuzzy = MwlQuery {
            patient_name: Some("IVANOV^IVN".into()),
            fuzzymatching: true,
            ..Default::default()
        };
        assert!(matches(&rec, &fuzzy), "one substitution is a fuzzy hit");

        // The PN separator folds to a space, so a typed query aligns with ^.
        let spaced = MwlQuery {
            patient_name: Some("IVANOV IVAN".into()),
            fuzzymatching: true,
            ..Default::default()
        };
        assert!(matches(&rec, &spaced));

        // Fuzzy applies to the physician PN key too.
        let phys = MwlQuery {
            performing_physician: Some("SMITH JON".into()),
            fuzzymatching: true,
            ..Default::default()
        };
        assert!(matches(&rec, &phys));

        // Two edits stay out.
        let far = MwlQuery {
            patient_name: Some("PETROV^PETR".into()),
            fuzzymatching: true,
            ..Default::default()
        };
        assert!(!matches(&rec, &far));

        // Non-PN keys never go fuzzy: Patient ID is LO.
        let pid = MwlQuery {
            patient_id: Some("P2".into()),
            fuzzymatching: true,
            ..Default::default()
        };
        assert!(!matches(&rec, &pid));
    }

    fn record(station: &str, patient_id: &str) -> SpsRecord {
        SpsRecord {
            sps_uid: "1.2.3".into(),
            station_aet: station.into(),
            sps_start_date: "20261011".into(),
            sps_start_time: "090000".into(),
            modality: "CT".into(),
            scheduled_performing_physician: "SMITH^JOHN".into(),
            patient_name: "IVANOV^IVAN".into(),
            patient_id: patient_id.into(),
            ..Default::default()
        }
    }

    /// PS3.19 A.1 knows no tag-less `DicomAttribute`: each part of the
    /// multipart answer is one complete document, and the boundary between
    /// records is carried by the MIME framing rather than by a private element
    /// no reader could be expected to skip.
    #[test]
    fn each_record_renders_as_its_own_document() {
        let rec = record("CT01", "P1");
        let xml = sps_to_xml(std::slice::from_ref(&rec));

        assert_eq!(
            xml.matches("<?xml").count(),
            1,
            "exactly one declaration per document: {xml}"
        );
        assert!(
            xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"),
            "the declaration is the first thing in the body: {xml}"
        );
        assert_eq!(
            xml.matches("<NativeDicomModel xmlns=").count(),
            1,
            "the root opens once and carries the PS3.19 namespace: {xml}"
        );
        assert!(
            xml.ends_with("</NativeDicomModel>"),
            "the root closes last: {xml}"
        );
        assert!(
            !xml.contains("<DicomAttribute/>"),
            "no tag-less attribute is emitted: {xml}"
        );
        assert!(
            xml.contains("<Value number=\"1\">P1</Value>"),
            "the record's attributes are at the model root: {xml}"
        );
    }

    /// The framing itself: two records become two parts, each a complete
    /// `application/dicom+xml` document, and the media type names the type.
    #[test]
    fn two_records_become_two_dicom_xml_parts() {
        let many = vec![record("CT01", "P1"), record("MR02", "P2")];
        let boundary = "test-boundary";
        let body = multipart_body(&many, boundary);

        let ct = multipart_content_type(boundary);
        assert!(
            ct.starts_with("multipart/related; type=\"application/dicom+xml\"; boundary="),
            "PS3.18 14.1.3 names the media type: {ct}"
        );

        assert_eq!(
            body.matches("Content-Type: application/dicom+xml").count(),
            2,
            "one part per record: {body}"
        );
        assert_eq!(
            body.matches("<?xml").count(),
            2,
            "one document per part: {body}"
        );
        assert!(
            body.ends_with(&format!("--{boundary}--\r\n")),
            "the terminating delimiter closes the body: {body}"
        );
        for pid in ["P1", "P2"] {
            assert!(body.contains(pid), "record {pid} has its own part: {body}");
        }
        assert!(
            !body.contains("<DicomAttribute/>"),
            "no private separator survives: {body}"
        );
    }

    /// The empty answer is still a well-formed document.
    #[test]
    fn an_empty_answer_is_an_empty_model() {
        assert_eq!(
            sps_to_xml(&[]),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><NativeDicomModel xmlns=\"http://dicom.nema.org/PS3.19/models/NativeDICOM\"></NativeDicomModel>"
        );
    }

    /// A PN reaches the model as its component groups, not as one opaque
    /// string: PS3.19 A.1.2 defines `FamilyName`/`GivenName` per group, and a
    /// reader that asks for a family name must get one.
    #[test]
    fn a_person_name_is_split_into_component_groups() {
        let mut rec = record("CT01", "P1");
        rec.patient_name = "IVANOV^IVAN^PETROVICH=ИВАНОВ^ИВАН".to_string();
        let xml = sps_to_xml(&[rec]);
        assert!(
            xml.contains("<PersonName number=\"1\"><Alphabetic><FamilyName>IVANOV</FamilyName><GivenName>IVAN</GivenName><MiddleName>PETROVICH</MiddleName></Alphabetic>"),
            "the alphabetic group carries its components: {xml}"
        );
        assert!(
            xml.contains("<Ideographic><FamilyName>ИВАНОВ</FamilyName>"),
            "the ideographic group is not dropped: {xml}"
        );
        assert!(
            !xml.contains("<Alphabetic>IVANOV^IVAN"),
            "the caret must not survive into the value: {xml}"
        );
    }
}
