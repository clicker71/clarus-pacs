// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DICOM XML (PS3.19) emission for UPS-RS workitems - the typed writer.
//!
//! Renders a [`WorkItem`] directly into a DICOM XML `<NativeDicomModel>`
//! document, byte-exact with the frozen `workitem.xml` golden. The previous
//! path went through `serde_json::Value` (`workitem_to_dicomweb_json` ->
//! `dicomweb_json_to_xml`); this typed walk reads the field values directly
//! (no serde, no `Value`), so the always-on XML producer stays alive after
//! removes the SQLite bridges.
//!
//! Attribute order follows the frozen golden: the legacy `serde_json::Map`
//! (a `BTreeMap`) serialized keys in ascending tag order, so `workitem_to_xml`
//! emits the tags in that same ascending order to stay byte-identical.

#![forbid(unsafe_code)]

use clarus_ports::models::WorkItem;
use std::fmt::Write;

use crate::handlers::DICOM_XML_CONTENT_TYPE;

/// The DICOM XML namespace for the NativeDICOM model (PS3.19).
const DICOM_XML_NS: &str = "http://dicom.nema.org/PS3.19/models/NativeDICOM";

/// Escape text content for XML element bodies (5 special chars).
///
/// Iterates over `char`s so multi-byte UTF-8 (e.g. Cyrillic) passes through
/// unchanged; only the 5 XML-special ASCII characters are replaced.
fn esc_into(out: &mut String, s: &str) {
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
}

/// Write a DICOM PersonName (VR=PN) as an XML `PersonName` element.
///
/// Decomposes the raw `^`-delimited value exactly like the legacy converter:
/// Alphabetic/Ideographic/Phonetic groups on `=`, each on `^` into up to 5
/// named components.
fn write_person_name(out: &mut String, raw: &str) {
    let groups: Vec<&str> = raw.split('=').collect();
    out.push_str("<PersonName>");

    fn write_name_group(out: &mut String, group_name: &str, value: &str) {
        let parts: Vec<&str> = value.split('^').collect();
        let _ = write!(out, "<{group_name}>");
        let write_part = |out: &mut String, elem: &str, v: &str| {
            let _ = write!(out, "<{elem}>");
            esc_into(out, v);
            let _ = write!(out, "</{elem}>");
        };
        if let Some(f) = parts.first()
            && !f.is_empty()
        {
            write_part(out, "FamilyName", f);
        }
        if let Some(g) = parts.get(1)
            && !g.is_empty()
        {
            write_part(out, "GivenName", g);
        }
        if let Some(m) = parts.get(2)
            && !m.is_empty()
        {
            write_part(out, "MiddleName", m);
        }
        if let Some(p) = parts.get(3)
            && !p.is_empty()
        {
            write_part(out, "Prefix", p);
        }
        if let Some(s) = parts.get(4)
            && !s.is_empty()
        {
            write_part(out, "Suffix", s);
        }
        let _ = write!(out, "</{group_name}>");
    }

    if let Some(alphabetic) = groups.first()
        && !alphabetic.is_empty()
    {
        write_name_group(out, "Alphabetic", alphabetic);
    }
    if let Some(ideographic) = groups.get(1)
        && !ideographic.is_empty()
    {
        write_name_group(out, "Ideographic", ideographic);
    }
    if let Some(phonetic) = groups.get(2)
        && !phonetic.is_empty()
    {
        write_name_group(out, "Phonetic", phonetic);
    }

    out.push_str("</PersonName>");
}

/// Append one scalar `<DicomAttribute>` element (a `<Value number="1">` body,
/// or `PersonName` decomposition for VR=PN).
fn write_scalar(out: &mut String, tag: &str, vr: &str, value: &str) {
    let _ = write!(
        out,
        "<DicomAttribute tag=\"{tag}\" vr=\"{vr}\" keyword=\"\">"
    );
    if vr == "PN" {
        write_person_name(out, value);
    } else if value.is_empty() {
        // PS3.19 A.1: a zero-length value is an attribute element with NO
        // <Value> child. The empty string and "not present" are different
        // facts - e.g. an issuer assigned but left blank - and an empty
        // <Value number="1"></Value> states the second while pretending to
        // state the first. A reader that counts <Value> elements then sees one
        // value where the standard says none.
    } else {
        out.push_str("<Value number=\"1\">");
        esc_into(out, value);
        out.push_str("</Value>");
    }
    out.push_str("</DicomAttribute>");
}

/// Append a workitem's `<DicomAttribute>` elements to `out` (no root).
///
/// Emits the UPS attributes in ascending tag order (matching the frozen
/// `workitem.xml` golden) and follows the same presence rules as the JSON
/// renderer in `ups::json`: the identity pair (0008,0016)/(0008,0018) and the
/// state (0074,1000) are always present, the rest only when set.
///
/// There is no (0008,0060) Modality in the UPS IOD (PS3.4 Table CC.2.5-3
/// defines none), but the element is emitted when the workitem carries one:
/// it is a documented vendor extension, because the DIMSE MWL service
/// requires Modality as a Return key Type 1. Absent stays absent.
fn write_workitem_attrs(out: &mut String, wi: &WorkItem) {
    // (0008,0016) SOPClassUID - UI (always): the UPS Push SOP Class.
    write_scalar(out, "00080016", "UI", super::json::UPS_PUSH_SOP_CLASS_UID);
    // (0008,0018) SOPInstanceUID - UI (always): the workitem's own identity.
    write_scalar(out, "00080018", "UI", &wi.workitem_uid);
    // (0008,0050) and (0020,000D) are NOT emitted bare: the standard carriage
    // is the Referenced Request Sequence below, and duplicating the two facts
    // at the top level would state them twice. See `write_referenced_request`.
    // (0008,0060) Modality - CS. Vendor extension, see the docstring.
    if let Some(v) = wi.modality.as_deref().filter(|m| !m.is_empty()) {
        write_scalar(out, "00080060", "CS", v);
    }
    // (0010,0010) PatientName - PN.
    if let Some(v) = wi.patient_name.as_deref() {
        write_scalar(out, "00100010", "PN", v);
    }
    // (0010,0020) PatientID - LO.
    if let Some(v) = wi.patient_id.as_deref() {
        write_scalar(out, "00100020", "LO", v);
    }
    // (0010,0021) IssuerOfPatientID - LO, type 1C: REQUIRED whenever a Patient
    // ID is present, and a zero-length value is the standard's own way of
    // saying "present, and this site declares no issuer". Same rule as the JSON
    // renderer, so the two renderings state the same facts.
    if wi.patient_id.is_some() {
        write_scalar(
            out,
            "00100021",
            "LO",
            wi.patient_id_issuer.as_deref().unwrap_or(""),
        );
    }
    // (0032,1032) RequestingPhysician - PN.
    if let Some(v) = wi.requesting_physician.as_deref() {
        write_scalar(out, "00321032", "PN", v);
    }
    // (0040,0002) ScheduledProcedureStepStartDate - DA.
    if let Some(v) = wi.procedure_step_start_date.as_deref() {
        write_scalar(out, "00400002", "DA", v);
    }
    // (0040,0003) ScheduledProcedureStepStartTime - TM.
    if let Some(v) = wi.procedure_step_start_time.as_deref() {
        write_scalar(out, "00400003", "TM", v);
    }
    // (0040,0010) ScheduledStationName - SH.
    if let Some(v) = wi.station_name.as_deref() {
        write_scalar(out, "00400010", "SH", v);
    }
    // (0040,4005) ScheduledProcedureStepStartDateTime - DT.
    if let Some(v) = wi.sps_start_datetime.as_deref() {
        write_scalar(out, "00404005", "DT", v);
    }
    // (0040,4025) ScheduledStationNameCodeSequence - SQ, one item whose Code
    // Value is the station (PS3.4 Table CC.2.5-3; (0040,0001) has no row at
    // all). Ascending tag order puts it after the whole (0040,00xx) block and
    // after (0040,4005), which is where the JSON renderer writes it too.
    if let Some(v) = wi.station_aet.as_deref() {
        write_scheduled_station_name_code(out, v);
    }
    // (0040,4018) ScheduledWorkitemCodeSequence - SQ. AIW-I 4.84.4.1.2.1 has
    // the performer echo this code back, and AIW-I 50.4.1.2 leaves the values
    // to deployment agreement, which is why the requester sends one at all. A
    // code with no Code Value is not a code, so it is not emitted.
    if let Some(code) = wi
        .scheduled_workitem_code
        .as_ref()
        .filter(|c| !c.value.is_empty())
    {
        write_workitem_code_sequence(out, "00404018", "ScheduledWorkitemCodeSequence", code);
    }
    // (0040,4021) InputInformationSequence - SQ, one item naming the study.
    if let Some(v) = wi.input_study_uid.as_deref() {
        write_input_information(out, v);
    }
    // (0040,4041) InputReadinessState - CS.
    if let Some(v) = wi.input_readiness.as_deref() {
        write_scalar(out, "00404041", "CS", v);
    }
    // (0040,4052) ProcedureStepCancellationDateTime - DT.
    if let Some(v) = wi.cancellation_datetime.as_deref() {
        write_scalar(out, "00404052", "DT", v);
    }
    // (0040,A370) ReferencedRequestSequence - SQ, the carriage of the study
    // and the accession. Ascending tag order puts it after (0040,4052) and
    // before (0074,1000), the same position the JSON render gives it.
    write_referenced_request(
        out,
        wi.accession_number.as_deref(),
        wi.input_study_uid.as_deref(),
        wi.requested_procedure_id.as_deref(),
        wi.reason_for_requested_procedure.as_deref(),
    );
    // (0074,1000) ProcedureStepState - CS (always).
    write_scalar(out, "00741000", "CS", &wi.workflow_state);
    // (0074,1200) ScheduledProcedureStepPriority - CS.
    if let Some(v) = wi.sps_priority.as_deref() {
        write_scalar(out, "00741200", "CS", v);
    }
    // (0074,1204) ProcedureStepLabel - SH.
    if let Some(v) = wi.step_label.as_deref() {
        write_scalar(out, "00741204", "SH", v);
    }
    // (0074,1210) ScheduledProcessingParametersSequence - SQ. The element is
    // stored as an opaque JSON info object (the server does not interpret the
    // Content Items, PS3.3 Table C.30.2-1), so it is re-parsed and re-emitted
    // through the serde-free JSON->XML converter that already serves
    // `/metadata` - the typed writer cannot model an arbitrary subtree, and
    // dropping the element would silently lose a read-path attribute the JSON
    // renderer returns.
    if let Some(stored) = wi
        .scheduled_processing_parameters
        .as_deref()
        .filter(|stored| !stored.is_empty())
    {
        // The bytes were serialized by this server's own writer, so a parse
        // failure is a stored-corruption case, not a client shape: skipping
        // the element beats emitting a half-written sequence.
        if let Ok(element) = crate::handlers::instance_xml::json_info_to_xml_element(
            "00741210",
            "ScheduledProcessingParametersSequence",
            stored.as_bytes(),
        ) {
            out.push_str(&element);
        }
    }
    // (0074,1216) UnifiedProcedureStepPerformedProcedureSequence - SQ. This is
    // the ONLY place the UPS IOD carries the performer report (PS3.3 Table
    // C.30.3-1): the start/end datetimes, the performed station and workitem
    // codes and the outputs are all CHILDREN of this item, so a conformant
    // reader looks for them here and nowhere else.
    write_performed_procedure(out, wi);
    // (0074,1238) ReasonForCancellation - LT.
    if let Some(v) = wi.reason_for_cancellation.as_deref() {
        write_scalar(out, "00741238", "LT", v);
    }
}

/// Append (0040,4025) ScheduledStationNameCodeSequence: one item whose Code
/// Value is the station AE Title.
///
/// PS3.4 Table CC.2.5-3 carries the station here and gives (0040,0001)
/// Scheduled Station AE Title no row, so this sequence is the whole statement.
/// AIW-I 4.81.4.1.2.1 Table 4.81.4.1.2.1-3 makes the Code Value the AE Title a
/// performer claims, which is also the key a station-oriented search sends, so
/// the fact is stated once and in the place both ends read it. The JSON
/// renderer mints the same item (see `ups::json`), and the two have to agree.
fn write_scheduled_station_name_code(out: &mut String, station: &str) {
    out.push_str(
        "<DicomAttribute tag=\"00404025\" vr=\"SQ\" keyword=\"ScheduledStationNameCodeSequence\">",
    );
    out.push_str("<Item number=\"1\">");
    // Ascending tag order within the item: (0008,0100), (0008,0102), (0008,0104).
    write_scalar(out, "00080100", "SH", station);
    write_scalar(out, "00080102", "SH", super::json::STATION_CODING_SCHEME);
    write_scalar(out, "00080104", "LO", super::json::STATION_CODE_MEANING);
    out.push_str("</Item>");
    out.push_str("</DicomAttribute>");
}

/// Append one coded-entry sequence: a single item holding (0008,0100) Code
/// Value, (0008,0102) Coding Scheme Designator and (0008,0104) Code Meaning, in
/// ascending tag order.
fn write_coded_item(
    out: &mut String,
    tag: &str,
    keyword: &str,
    code: &clarus_ports::models::WorkItemCode,
) {
    let _ = write!(
        out,
        "<DicomAttribute tag=\"{tag}\" vr=\"SQ\" keyword=\"{keyword}\">"
    );
    out.push_str("<Item number=\"1\">");
    write_scalar(out, "00080100", "SH", &code.value);
    write_scalar(out, "00080102", "SH", &code.scheme);
    write_scalar(out, "00080104", "LO", &code.meaning);
    out.push_str("</Item>");
    out.push_str("</DicomAttribute>");
}

/// Append (0040,4018) ScheduledWorkitemCodeSequence.
fn write_workitem_code_sequence(
    out: &mut String,
    tag: &str,
    keyword: &str,
    code: &clarus_ports::models::WorkItemCode,
) {
    write_coded_item(out, tag, keyword, code);
}

/// Append (0074,1216) UnifiedProcedureStepPerformedProcedureSequence.
///
/// The item carries exactly the children the record holds, in ascending tag
/// order (0040,4019) < (0040,4028) < (0040,4033) < (0040,4050) < (0040,4051),
/// so a report that named its outputs but no end time round-trips as exactly
/// that. A record with no report at all emits nothing: an item with no
/// children would say "a performance was reported and it states nothing".
fn write_performed_procedure(out: &mut String, wi: &WorkItem) {
    let code_present = |code: Option<&clarus_ports::models::WorkItemCode>| {
        code.is_some_and(|c| !c.value.is_empty())
    };
    let any = wi.performed_start_datetime.is_some()
        || wi.performed_end_datetime.is_some()
        || code_present(wi.performed_workitem_code.as_ref())
        || code_present(wi.performed_station_name_code.as_ref())
        || wi.output_information.is_some();
    if !any {
        return;
    }
    out.push_str(
        "<DicomAttribute tag=\"00741216\" vr=\"SQ\" \
         keyword=\"UnifiedProcedureStepPerformedProcedureSequence\">",
    );
    out.push_str("<Item number=\"1\">");
    if let Some(code) = wi
        .performed_workitem_code
        .as_ref()
        .filter(|c| !c.value.is_empty())
    {
        write_coded_item(out, "00404019", "PerformedWorkitemCodeSequence", code);
    }
    if let Some(code) = wi
        .performed_station_name_code
        .as_ref()
        .filter(|c| !c.value.is_empty())
    {
        write_coded_item(out, "00404028", "PerformedStationNameCodeSequence", code);
    }
    if let Some(outputs) = wi.output_information.as_deref() {
        out.push_str(
            "<DicomAttribute tag=\"00404033\" vr=\"SQ\" keyword=\"OutputInformationSequence\">",
        );
        for (i, output) in outputs.iter().enumerate() {
            let _ = write!(out, "<Item number=\"{}\">", i + 1);
            // Ascending tag order within the item: (0008,1150), (0008,1155),
            // (0020,000E). The series is omitted when the performer named none,
            // which mirrors the JSON renderer and keeps the wire shape
            // unchanged for a reference that has no series.
            write_scalar(out, "00081150", "UI", &output.sop_class_uid);
            write_scalar(out, "00081155", "UI", &output.sop_instance_uid);
            if !output.series_instance_uid.is_empty() {
                write_scalar(out, "0020000E", "UI", &output.series_instance_uid);
            }
            out.push_str("</Item>");
        }
        out.push_str("</DicomAttribute>");
    }
    if let Some(v) = wi.performed_start_datetime.as_deref() {
        write_scalar(out, "00404050", "DT", v);
    }
    if let Some(v) = wi.performed_end_datetime.as_deref() {
        write_scalar(out, "00404051", "DT", v);
    }
    out.push_str("</Item>");
    out.push_str("</DicomAttribute>");
}

/// Append (0040,4021) InputInformationSequence: one item naming the input
/// study, in the PS3.19 A.1 sequence shape.
fn write_input_information(out: &mut String, study_uid: &str) {
    out.push_str(
        "<DicomAttribute tag=\"00404021\" vr=\"SQ\" keyword=\"InputInformationSequence\">",
    );
    out.push_str("<Item number=\"1\">");
    write_scalar(out, "0040E020", "CS", "DICOM");
    write_scalar(out, "0020000D", "UI", study_uid);
    out.push_str("</Item>");
    out.push_str("</DicomAttribute>");
}

/// Append (0040,A370) ReferencedRequestSequence: one item carrying the
/// Accession Number and the Study Instance UID.
///
/// PS3.4 Table CC.2.5-3 types the sequence 2/2 at N-CREATE with a nested
/// Accession Number (0008,0050) Required and a nested Study Instance UID
/// (0020,000D) Optional. THIS is the carriage the task requester reads and
/// writes, so the two facts are stated here and NOT as bare top-level
/// elements - the XML renderer and the JSON one have to say the same thing.
///
/// Emitted when EITHER value is known: a sequence with one nested element is
/// the honest shape for a workitem that knows the study but no accession (or
/// the reverse), and omitting the whole sequence would drop a fact the record
/// holds. Both absent -> nothing.
///
/// Within the item the elements are in ascending tag order (0008,0050 before
/// 0020,000D before 0040,1001 before 0040,1002), the order the JSON emitter
/// uses.
fn write_referenced_request(
    out: &mut String,
    accession: Option<&str>,
    study_uid: Option<&str>,
    requested_procedure_id: Option<&str>,
    reason_for_requested_procedure: Option<&str>,
) {
    if accession.is_none()
        && study_uid.is_none()
        && requested_procedure_id.is_none()
        && reason_for_requested_procedure.is_none()
    {
        return;
    }
    out.push_str(
        "<DicomAttribute tag=\"0040A370\" vr=\"SQ\" keyword=\"ReferencedRequestSequence\">",
    );
    out.push_str("<Item number=\"1\">");
    if let Some(accession) = accession {
        write_scalar(out, "00080050", "SH", accession);
    }
    if let Some(study) = study_uid {
        write_scalar(out, "0020000D", "UI", study);
    }
    // (0040,1001) Requested Procedure ID - a REQUIRED matching key of
    // CC.2.5-3 and the requester's own order number for the step.
    if let Some(procedure) = requested_procedure_id {
        write_scalar(out, "00401001", "SH", procedure);
    }
    // (0040,1002) Reason for the Requested Procedure - the one nested
    // attribute of this sequence an N-SET may change (Table CC.2.5-3).
    if let Some(reason) = reason_for_requested_procedure {
        write_scalar(out, "00401002", "LO", reason);
    }
    out.push_str("</Item>");
    out.push_str("</DicomAttribute>");
}

/// Render a single [`WorkItem`] as a complete DICOM XML document.
pub fn workitem_to_xml(wi: &WorkItem) -> String {
    let mut out = String::with_capacity(1024);
    let _ = writeln!(out, r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    let _ = writeln!(out, r#"<NativeDicomModel xmlns="{DICOM_XML_NS}">"#);
    write_workitem_attrs(&mut out, wi);
    let _ = writeln!(out, "</NativeDicomModel>");
    out
}

/// The media type PS3.18 8.3.4.4.1 names for a DICOM XML response body.
///
/// A search answer is a LIST of workitems, and PS3.18 8.7.2 carries a list of
/// models as `multipart/related` with one `application/dicom+xml` part per
/// model - the same shape the MWL and QIDO search endpoints return. A single
/// model root holding every workitem's attributes cannot be read back: the
/// attributes of one workitem are indistinguishable from another's, and a
/// consumer looking for one item's Scheduled Procedure Step ID finds all of
/// them in one flat list.
pub fn xml_multipart_content_type(boundary: &str) -> String {
    format!("multipart/related; type=\"{DICOM_XML_CONTENT_TYPE}\"; boundary=\"{boundary}\"")
}

/// Assemble the multipart/related body: one `application/dicom+xml` document
/// per workitem.
///
/// RFC 2046 5.1.1 puts no CRLF before the FIRST delimiter and a CRLF before
/// every later one. An empty answer still carries the terminating delimiter: a
/// multipart entity with no parts is legal and parses as "no matches", which is
/// the honest answer where a bare model root would claim one nameless item.
pub fn workitems_to_multipart(workitems: &[WorkItem], boundary: &str) -> Vec<u8> {
    let mut body = String::with_capacity(workitems.len() * 640);
    if workitems.is_empty() {
        body.push_str(&format!("--{boundary}--\r\n"));
        return body.into_bytes();
    }
    for (i, wi) in workitems.iter().enumerate() {
        if i == 0 {
            body.push_str(&format!("--{boundary}\r\n"));
        } else {
            body.push_str(&format!("\r\n--{boundary}\r\n"));
        }
        let xml = workitem_to_xml(wi);
        body.push_str(&format!(
            "Content-Type: {DICOM_XML_CONTENT_TYPE}\r\nContent-Length: {}\r\n\r\n",
            xml.len()
        ));
        body.push_str(&xml);
    }
    body.push_str(&format!("\r\n--{boundary}--\r\n"));
    body.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::{workitems_to_multipart, write_scalar, xml_multipart_content_type};
    use clarus_ports::models::WorkItem;

    /// The multipart framing does not depend on which fields are populated: a
    /// part is written per workitem regardless of content. Only the identity
    /// fields are set so the rendered document is non-empty.
    fn workitem(uid: &str) -> WorkItem {
        WorkItem {
            id: None,
            workitem_uid: uid.to_string(),
            workflow_state: "SCHEDULED".to_string(),
            station_aet: Some("CT01".to_string()),
            station_name: None,
            procedure_step_start_date: Some("20261011".to_string()),
            procedure_step_start_time: Some("090000".to_string()),
            patient_name: Some("IVANOV^IVAN".to_string()),
            patient_id: Some("P1".to_string()),
            accession_number: None,
            requested_procedure_id: None,
            requesting_physician: None,
            modality: Some("CT".to_string()),
            sps_priority: None,
            transaction_uid: None,
            created_at: None,
            updated_at: None,
            step_label: Some("STEP".to_string()),
            input_readiness: Some("READY".to_string()),
            sps_start_datetime: Some("20261011090000".to_string()),
            input_study_uid: None,
            reason_for_cancellation: None,
            cancellation_datetime: None,
            patient_id_issuer: None,
            scheduled_workitem_code: None,
            output_information: None,
            performed_workitem_code: None,
            performed_station_name_code: None,
            performed_start_datetime: None,
            performed_end_datetime: None,
            scheduled_processing_parameters: None,
            reason_for_requested_procedure: None,
        }
    }

    /// PS3.18 8.7.2: a search answer is a LIST of models, so each workitem
    /// travels as its own `application/dicom+xml` document inside
    /// `multipart/related`. One model root holding every workitem's attributes
    /// cannot be read back - a consumer asking for one item's Scheduled
    /// Procedure Step ID would match all of them.
    #[test]
    fn two_workitems_become_two_xml_parts() {
        let items = vec![workitem("1.2.3.1"), workitem("1.2.3.2")];
        let boundary = "test-boundary";
        let body = String::from_utf8(workitems_to_multipart(&items, boundary)).expect("utf-8");

        assert_eq!(
            body.matches("Content-Type: application/dicom+xml").count(),
            2,
            "one part per workitem: {body}"
        );
        assert_eq!(
            body.matches("<NativeDicomModel xmlns=").count(),
            2,
            "each part is a complete model: {body}"
        );
        assert!(
            !body.contains("\r\n--test-boundary\r\n--test-boundary"),
            "parts are separated, not glued: {body}"
        );
        assert!(
            body.ends_with(&format!("--{boundary}--\r\n")),
            "the terminating delimiter closes the body: {body}"
        );
        let media = xml_multipart_content_type(boundary);
        assert!(
            media.starts_with("multipart/related; type=\"application/dicom+xml\"; boundary="),
            "PS3.18 8.7.2 names the media type: {media}"
        );
    }

    /// An empty answer still parses as "no matches".
    #[test]
    fn an_empty_answer_is_a_terminated_multipart() {
        let body = workitems_to_multipart(&[], "b");
        assert_eq!(
            body,
            b"--b--\r\n".to_vec(),
            "a legal empty multipart entity"
        );
    }

    /// PS3.19 A.1: zero length is stated by OMITTING `<Value>`, not by writing
    /// an empty one. An empty `<Value number="1"></Value>` claims one value
    /// exists and that it is empty, which is a different assertion and one the
    /// standard has no encoding for.
    #[test]
    fn a_zero_length_value_is_written_without_a_value_element() {
        let mut out = String::new();
        write_scalar(&mut out, "00400009", "SH", "");
        assert_eq!(
            out, "<DicomAttribute tag=\"00400009\" vr=\"SH\" keyword=\"\"></DicomAttribute>",
            "an empty issuer is a zero-length attribute, not an empty value"
        );
        assert!(
            !out.contains("<Value"),
            "no Value element may appear for a zero-length value"
        );
    }

    /// The non-empty case still writes exactly one numbered `<Value>`.
    #[test]
    fn a_non_empty_value_is_written_as_one_numbered_value_element() {
        let mut out = String::new();
        write_scalar(&mut out, "00400009", "SH", "ISSUER-1");
        assert_eq!(
            out,
            "<DicomAttribute tag=\"00400009\" vr=\"SH\" keyword=\"\">\
             <Value number=\"1\">ISSUER-1</Value></DicomAttribute>"
        );
    }
}
