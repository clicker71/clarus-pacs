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
    // PS3.19 A.1.5-2: `number` is REQUIRED on `PersonName` (the order within
    // the Value Field, monotonically from 1). One PN value, so `number="1"`.
    out.push_str("<PersonName number=\"1\">");

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
            // PS3.19 Table 10.2-1 (Person Name Components Macro) names this
            // component `NamePrefix`; a bare `Prefix` is not an element the
            // schema defines, so a schema-aware reader drops it.
            write_part(out, "NamePrefix", p);
        }
        if let Some(s) = parts.get(4)
            && !s.is_empty()
        {
            write_part(out, "NameSuffix", s);
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

/// The PS3.6 keyword for a tag this server renders.
///
/// PS3.19 A.1.5-2 makes `keyword` **Required unless the DICOM Data Element is
/// unknown to the host**, so an empty string is only correct for a tag we do not
/// recognize. Every attribute this server emits IS known, and the keyword is the
/// PS3.6 name in DICOM title case - the same name the JSON renderer's callers use
/// and the same one a schema-validating reader looks up.
///
/// An unknown tag keeps an empty keyword rather than inventing one: a wrong
/// keyword is worse than an absent one, because a reader that trusts it reads a
/// different attribute.
fn keyword_for(tag: &str) -> &'static str {
    match tag {
        "00080016" => "SOPClassUID",
        "00080018" => "SOPInstanceUID",
        "00080050" => "AccessionNumber",
        "00080060" => "Modality",
        "00080090" => "ReferringPhysicianName",
        // CodeSequenceMacro components (PS3.3 Table 8.8-1): a code item is
        // three attributes, and a reader binds each by its own keyword.
        "00080100" => "CodeValue",
        "00080102" => "CodingSchemeDesignator",
        "00080104" => "CodeMeaning",
        "00081150" => "ReferencedSOPClassUID",
        "00081155" => "ReferencedSOPInstanceUID",
        "00081199" => "ReferencedSOPSequence",
        "00100010" => "PatientName",
        "00100020" => "PatientID",
        "00100021" => "IssuerOfPatientID",
        "0020000D" => "StudyInstanceUID",
        "0020000E" => "SeriesInstanceUID",
        "00321032" => "RequestingPhysician",
        "00400002" => "ScheduledProcedureStepStartDate",
        "00400003" => "ScheduledProcedureStepStartTime",
        "00400009" => "ScheduledProcedureStepSequence",
        "00400010" => "ScheduledStationName",
        "00401001" => "RequestedProcedureID",
        "00401002" => "ReasonForTheRequestedProcedure",
        "00402400" => "CommentsOnThePerformedProcedureStep",
        "00404005" => "ScheduledProcedureStepStartDateTime",
        "00404018" => "ScheduledWorkitemCodeSequence",
        "00404019" => "PerformedWorkitemCodeSequence",
        "00404021" => "InputInformationSequence",
        "00404025" => "ScheduledStationNameCodeSequence",
        "00404028" => "PerformedStationNameCodeSequence",
        "00404033" => "OutputInformationSequence",
        "00404041" => "InputReadinessState",
        "00404050" => "PerformedProcedureStepStartDateTime",
        "00404051" => "PerformedProcedureStepEndDateTime",
        "00404052" => "ProcedureStepCancellationDateTime",
        "0040A370" => "ReferencedRequestSequence",
        "0040A375" => "CurrentRequestedProcedureEvidenceSequence",
        "0040E020" => "TypeOfInstances",
        "00741000" => "ProcedureStepState",
        // PS3.6 registers (0074,1002) as "Procedure Step Progress Information
        // Sequence" with THIS keyword. The shorter ProgressInformationSequence
        // is not a keyword at all, so a schema-aware reader binding on it finds
        // nothing (standard/ps3.6/ups-native-model-keywords.json).
        "00741002" => "ProcedureStepProgressInformationSequence",
        "00741200" => "ScheduledProcedureStepPriority",
        "00741202" => "WorklistLabel",
        "00741204" => "ProcedureStepLabel",
        "00741210" => "ScheduledProcessingParametersSequence",
        "00741216" => "UnifiedProcedureStepPerformedProcedureSequence",
        "00741238" => "ReasonForCancellation",
        _ => "",
    }
}

/// Append one scalar `<DicomAttribute>` element (a `<Value number="1">` body,
/// or `PersonName` decomposition for VR=PN).
fn write_scalar(out: &mut String, tag: &str, vr: &str, value: &str) {
    if vr == "PN" && value.is_empty() {
        // PS3.19 A.1.5-2: a zero-length PN is an attribute element with NO
        // `PersonName` child. `PersonName` is REQUIRED only when the element
        // has a non-zero value, and is not allowed otherwise - so emitting an
        // empty `<PersonName>` for a blank name claims a value where the
        // standard says there is none.
        let keyword = keyword_for(tag);
        let _ = write!(
            out,
            "<DicomAttribute tag=\"{tag}\" vr=\"{vr}\" keyword=\"{keyword}\"></DicomAttribute>"
        );
        return;
    }
    let keyword = keyword_for(tag);
    let _ = write!(
        out,
        "<DicomAttribute tag=\"{tag}\" vr=\"{vr}\" keyword=\"{keyword}\">"
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
    // (0032,1032) Requesting Physician is a CHILD of (0040,A370) Referenced
    // Request Sequence (PS3.4 Table CC.2.5-3), not a dataset-root attribute,
    // so it is written inside that sequence below. It used to be emitted here,
    // where a reader binding by the standard's path never looks.
    // (0020,000D) at DATASET ROOT - the study assigned to the OUTPUT instances.
    // PS3.4 Table CC.2.5-1 lists it as its own row, Type 2, beside the Type 1
    // nested study of the Referenced Request Sequence. Emitting it here is not a
    // duplicate: one says where the results go, the other which study was read.
    if let Some(v) = wi.output_study_uid.as_deref().filter(|u| !u.is_empty()) {
        write_scalar(out, "0020000D", "UI", v);
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
    // (0040,4021) InputInformationSequence - SQ, one item naming the input.
    //
    // When the full sequence was stored, it is re-parsed and re-emitted through
    // the same serde-free JSON->XML converter as (0074,1210) - the typed writer
    // cannot model an arbitrary subtree. The converter has no depth cap while
    // the request-body parser refuses at MAX_JSON_DEPTH, so a stored subtree
    // this server accepted CAN fail to convert.
    //
    // With nothing stored there is no item to write: the sequence is a
    // Referenced Instances and Access Macro item (PS3.3 Table 10-3b, cited by
    // CC.2.5-2c) whose instances are named by (0008,1150)/(0008,1155), and the
    // record holds neither a SOP Class nor a SOP Instance UID for its input. An
    // item carrying only (0020,000D) and `TypeOfInstances` = DICOM claims
    // instances without naming one, which no consumer can resolve.
    if let Some(stored) = wi
        .input_information
        .as_deref()
        .filter(|stored| !stored.is_empty())
        && let Ok(element) = crate::handlers::instance_xml::json_info_to_xml_element(
            "00404021",
            "InputInformationSequence",
            stored.as_bytes(),
        )
    {
        out.push_str(&element);
    }
    // (0040,4041) InputReadinessState - CS.
    if let Some(v) = wi.input_readiness.as_deref() {
        write_scalar(out, "00404041", "CS", v);
    }
    // (0040,4052) and (0074,1238) are NOT top-level: PS3.3 Table C.30.1-1
    // makes both children of (0074,1002) Progress Information Sequence, so they
    // are written there below. Emitting them at root states an attribute the
    // module does not put at root, and a reader binding by path finds nothing.
    // (0040,A370) ReferencedRequestSequence - SQ, the carriage of the study
    // and the accession. Ascending tag order puts it before (0074,1000), the
    // same position the JSON render gives it.
    write_referenced_request(
        out,
        wi.accession_number.as_deref(),
        wi.input_study_uid.as_deref(),
        wi.requested_procedure_id.as_deref(),
        wi.reason_for_requested_procedure.as_deref(),
        wi.requesting_physician.as_deref(),
    );
    // (0074,1000) ProcedureStepState - CS (always).
    write_scalar(out, "00741000", "CS", &wi.workflow_state);
    // (0074,1200) ScheduledProcedureStepPriority - CS.
    if let Some(v) = wi.sps_priority.as_deref() {
        write_scalar(out, "00741200", "CS", v);
    }
    // (0074,1202) Worklist Label - LO, Type 2/1 and a matching key (PS3.4
    // Table CC.2.5-3). Ascending tag order puts it before (0074,1204).
    if let Some(v) = wi.worklist_label.as_deref() {
        write_scalar(out, "00741202", "LO", v);
    }
    // (0074,1204) ProcedureStepLabel - LO (PS3.6), not SH: a label up to 64
    // characters is legal and an SH render would truncate it at 16.
    if let Some(v) = wi.step_label.as_deref() {
        write_scalar(out, "00741204", "LO", v);
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
        // A parse failure IS possible (the converter has no depth cap while
        // the request-body parser refuses at MAX_JSON_DEPTH). There is no
        // typed projection for this element, so on failure it is omitted;
        // that is the only fallback this opaque-only attribute has.
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
    //
    // When the full item was stored opaquely (it may carry children the typed
    // projection does not model), it is re-parsed and re-emitted verbatim.
    // The converter has no depth cap while the request-body parser refuses at
    // MAX_JSON_DEPTH, so a stored item this server accepted CAN fail to
    // convert; on that failure the typed projection is written instead of
    // dropping the element silently.
    if let Some(stored) = wi
        .performed_procedure_information
        .as_deref()
        .filter(|stored| !stored.is_empty())
    {
        match crate::handlers::instance_xml::json_info_to_xml_element(
            "00741216",
            "UnifiedProcedureStepPerformedProcedureSequence",
            stored.as_bytes(),
        ) {
            Ok(element) => out.push_str(&element),
            // A parse failure is possible (see above): fall back to the typed
            // projection rather than dropping the performer report.
            Err(_) => write_performed_procedure(out, wi),
        }
    } else {
        write_performed_procedure(out, wi);
    }
    // (0074,1002) ProgressInformationSequence - SQ, one item, carrying the
    // cancellation facts (PS3.3 Table C.30.1-1). Skipped entirely when neither
    // child has a value: the sequence is "Zero or one Item" and an item
    // holding no attribute states nothing.
    write_progress_information(
        out,
        wi.cancellation_datetime.as_deref(),
        wi.reason_for_cancellation.as_deref(),
    );
}

/// Write (0074,1002) Progress Information Sequence, one item.
///
/// Children in ascending tag order: (0040,4052) Procedure Step Cancellation
/// DateTime before (0074,1238) Reason For Cancellation.
fn write_progress_information(
    out: &mut String,
    cancellation_datetime: Option<&str>,
    reason_for_cancellation: Option<&str>,
) {
    if cancellation_datetime.is_none() && reason_for_cancellation.is_none() {
        // K1: (0074,1002) is Final State 2 (PS3.4 Table CC.2.5-1), so the
        // element is present even with no item. PS3.19 A.1: a zero-length
        // sequence is an attribute element with no Item child.
        out.push_str(
            "<DicomAttribute tag=\"00741002\" vr=\"SQ\" \
             keyword=\"ProcedureStepProgressInformationSequence\"></DicomAttribute>",
        );
        return;
    }
    out.push_str(
        "<DicomAttribute tag=\"00741002\" vr=\"SQ\" \
         keyword=\"ProcedureStepProgressInformationSequence\"><Item number=\"1\">",
    );
    if let Some(v) = cancellation_datetime {
        write_scalar(out, "00404052", "DT", v);
    }
    if let Some(v) = reason_for_cancellation {
        write_scalar(out, "00741238", "LT", v);
    }
    out.push_str("</Item></DicomAttribute>");
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
/// that. A record with no report at all still states the ELEMENT: PS3.4
/// Table CC.2.5-1 types (0074,1216) Final State 2, and PS3.19 A.1 renders an
/// empty sequence as an attribute with no Item child - an empty ITEM would
/// claim a performance that reported nothing.
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
        out.push_str(
            "<DicomAttribute tag=\"00741216\" vr=\"SQ\" \
             keyword=\"UnifiedProcedureStepPerformedProcedureSequence\"></DicomAttribute>",
        );
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
        if outputs.is_empty() {
            // PS3.4 Table CC.2.5-1 types (0040,4033) Final State 2, so the
            // element is stated even with no item; PS3.19 A.1 renders that as
            // an attribute with no Item child.
            out.push_str(
                "<DicomAttribute tag=\"00404033\" vr=\"SQ\" \
                 keyword=\"OutputInformationSequence\"></DicomAttribute>",
            );
        } else {
            out.push_str(
                "<DicomAttribute tag=\"00404033\" vr=\"SQ\" keyword=\"OutputInformationSequence\">",
            );
            for (i, output) in outputs.iter().enumerate() {
                let _ = write!(out, "<Item number=\"{}\">", i + 1);
                // The item is a Referenced Instances and Access Macro item
                // (PS3.3 Table 10-3b): the reference pair sits in a NESTED
                // (0008,1199) Referenced SOP Sequence, not flat on the item,
                // and the macro's own children follow at item level.
                out.push_str(
                    "<DicomAttribute tag=\"00081199\" vr=\"SQ\" keyword=\"ReferencedSOPSequence\">\
                     <Item number=\"1\">",
                );
                write_scalar(out, "00081150", "UI", &output.sop_class_uid);
                write_scalar(out, "00081155", "UI", &output.sop_instance_uid);
                out.push_str("</Item></DicomAttribute>");
                // (0020,000D) Study Instance UID - the macro's Type 1C study,
                // present when the output names one. It tells a consumer which
                // study to search for the referenced instance.
                if let Some(study) =
                    Some(output.study_instance_uid.as_str()).filter(|u| !u.is_empty())
                {
                    write_scalar(out, "0020000D", "UI", study);
                }
                // (0020,000E) Series Instance UID - Type 1C, required for an
                // object with a Series IE. Omitted when the performer named
                // none, which is the one case the macro leaves open.
                if !output.series_instance_uid.is_empty() {
                    write_scalar(out, "0020000E", "UI", &output.series_instance_uid);
                }
                // (0040,E020) Type of Instances - Type 1 in the macro: it says
                // WHAT KIND of instance the reference points at, and without it
                // the item does not describe an instance of any type.
                write_scalar(out, "0040E020", "CS", "DICOM");
                out.push_str("</Item>");
            }
            out.push_str("</DicomAttribute>");
        }
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
    requesting_physician: Option<&str>,
) {
    // E4: Study Instance UID is N-CREATE 1/1 in this sequence (PS3.4 Table
    // CC.2.5-3, `ups-cc-2.5-3-referenced-request-nested.json`), so an item
    // without one is not a valid item. An accession-only or procedure-ID-only
    // item names no study and a consumer cannot resolve it.
    let Some(study) = study_uid.filter(|u| !u.is_empty()) else {
        return;
    };
    out.push_str(
        "<DicomAttribute tag=\"0040A370\" vr=\"SQ\" keyword=\"ReferencedRequestSequence\">",
    );
    out.push_str("<Item number=\"1\">");
    if let Some(accession) = accession {
        write_scalar(out, "00080050", "SH", accession);
    }
    write_scalar(out, "0020000D", "UI", study);
    // (0032,1032) Requesting Physician is a CHILD of this sequence (PS3.4 Table
    // CC.2.5-3), not a dataset-root attribute. Ascending tag order puts it
    // before (0040,1001).
    if let Some(physician) = requesting_physician.filter(|p| !p.is_empty()) {
        write_scalar(out, "00321032", "PN", physician);
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
/// every later one. An empty answer still carries ONE part, not a bare
/// terminating delimiter: PS3.19 A.1.5-2 states that an empty result set is a
/// `NativeDicomModel` with zero `DicomAttribute` elements - a valid, well-formed
/// document - so a client that parses the body gets a model it can read and sees
/// zero workitems. A bare `--boundary--` is a multipart entity with no parts at
/// all, which a DICOMweb client that expects one model per part cannot
/// distinguish from a truncated response.
pub fn workitems_to_multipart(workitems: &[WorkItem], boundary: &str) -> Vec<u8> {
    let mut body = String::with_capacity((workitems.len() + 1) * 640);
    if workitems.is_empty() {
        let xml = empty_native_model();
        body.push_str(&format!("--{boundary}\r\n"));
        body.push_str(&format!(
            "Content-Type: {DICOM_XML_CONTENT_TYPE}\r\nContent-Length: {}\r\n\r\n",
            xml.len()
        ));
        body.push_str(&xml);
        body.push_str(&format!("\r\n--{boundary}--\r\n"));
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

/// A `NativeDicomModel` with no attributes - the standard's encoding of an
/// empty result set (PS3.19 A.1.5-2).
///
/// Distinct from "no model at all": the document is well-formed and parses to
/// zero DicomAttribute elements, which is the answer "this search matched
/// nothing". The XML declaration and both namespaces match `workitem_to_xml`, so
/// a reader uses one parser for both.
pub fn empty_native_model() -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    // The SAME namespace `workitem_to_xml` declares, so one parser reads both.
    // The plain `http://dicom.nema.org/standard.html` this used to declare is
    // not the Native DICOM Model namespace, and a reader that binds on
    // `{DICOM_XML_NS}NativeDicomModel` saw no root for an empty answer.
    let _ = writeln!(out, r#"<NativeDicomModel xmlns="{DICOM_XML_NS}">"#);
    out.push_str("</NativeDicomModel>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::{
        DICOM_XML_NS, empty_native_model, keyword_for, workitem_to_xml, workitems_to_multipart,
        write_scalar, xml_multipart_content_type,
    };
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
            worklist_label: None,
            output_study_uid: None,
            input_readiness: Some("READY".to_string()),
            sps_start_datetime: Some("20261011090000".to_string()),
            input_study_uid: None,
            input_information: None,
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
            performed_procedure_information: None,
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

    /// D5: an empty answer is a valid model, not a bare terminator.
    ///
    /// PS3.19 A.1.5-2: an empty result set is a `NativeDicomModel` with zero
    /// DicomAttribute elements. A bare `--b--` is a multipart entity with no
    /// parts, which a DICOMweb client expecting one model per part cannot tell
    /// from a truncated response.
    #[test]
    fn an_empty_answer_carries_a_model_with_no_attributes() {
        let body = workitems_to_multipart(&[], "b");
        let text = String::from_utf8_lossy(&body).into_owned();

        // Exactly one part, framed like every other part.
        assert_eq!(
            text.matches("Content-Type: application/dicom+xml").count(),
            1,
            "the empty answer carries one part: {text}"
        );
        assert!(
            text.starts_with("--b\r\n"),
            "the first delimiter has no leading CRLF: {text}"
        );
        assert!(
            text.ends_with("--b--\r\n"),
            "the terminating delimiter closes the body: {text}"
        );

        // The part body is a well-formed model with no attributes.
        assert!(
            text.contains("<NativeDicomModel"),
            "the part must carry a NativeDicomModel: {text}"
        );
        assert!(
            text.contains(&format!("xmlns=\"{DICOM_XML_NS}\"")),
            "the empty model declares the Native DICOM Model namespace, the same one \
             workitem_to_xml uses: {text}"
        );
        assert!(
            text.contains(&format!("<NativeDicomModel xmlns=\"{DICOM_XML_NS}\">")),
            "an element-name check alone cannot see a wrong namespace: {text}"
        );
        assert!(text.contains("</NativeDicomModel>"), "and close it: {text}");
        assert!(
            !text.contains("<DicomAttribute"),
            "zero workitems means zero DicomAttribute elements: {text}"
        );

        // The declared Content-Length matches the model actually sent.
        let model = empty_native_model();
        assert!(
            text.contains(&format!("Content-Length: {}\r\n\r\n", model.len())),
            "Content-Length must describe the model body: {text}"
        );
        assert!(text.contains(&model), "the model itself is in the part");
    }

    /// PS3.19 A.1: zero length is stated by OMITTING `<Value>`, not by writing
    /// an empty one. An empty `<Value number="1"></Value>` claims one value
    /// exists and that it is empty, which is a different assertion and one the
    /// standard has no encoding for.
    ///
    /// The keyword is the real PS3.6 name (D3): `keyword` is Required unless the
    /// tag is unknown to the host, and 0040,0009 is a tag this server emits.
    #[test]
    fn a_zero_length_value_is_written_without_a_value_element() {
        let mut out = String::new();
        write_scalar(&mut out, "00400009", "SH", "");
        assert_eq!(
            out,
            "<DicomAttribute tag=\"00400009\" vr=\"SH\" \
             keyword=\"ScheduledProcedureStepSequence\"></DicomAttribute>",
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
            "<DicomAttribute tag=\"00400009\" vr=\"SH\" \
             keyword=\"ScheduledProcedureStepSequence\">\
             <Value number=\"1\">ISSUER-1</Value></DicomAttribute>"
        );
    }

    /// D3: every scalar this server emits carries its PS3.6 keyword, never an
    /// empty one. PS3.19 A.1.5-2 makes `keyword` Required unless the tag is
    /// unknown to the host, and a schema-aware reader uses it to bind the
    /// element.
    #[test]
    fn every_known_tag_carries_its_ps36_keyword() {
        for (tag, keyword) in [
            ("00100010", "PatientName"),
            ("00100020", "PatientID"),
            ("00080018", "SOPInstanceUID"),
            ("00321032", "RequestingPhysician"),
            ("00404041", "InputReadinessState"),
            ("00741000", "ProcedureStepState"),
        ] {
            let mut out = String::new();
            write_scalar(&mut out, tag, "LO", "X");
            assert!(
                out.contains(&format!("keyword=\"{keyword}\"")),
                "tag {tag} must carry keyword {keyword}, got: {out}"
            );
            assert!(
                !out.contains("keyword=\"\""),
                "tag {tag} must not emit an empty keyword: {out}"
            );
        }
        // An unknown tag keeps the empty keyword rather than inventing one.
        assert_eq!(keyword_for("77777777"), "");
    }

    /// J3: every standard tag this writer emits carries its PS3.6 keyword, and
    /// (0074,1002) carries the registered one.
    ///
    /// PS3.19 A.1.5-2 makes `keyword` Required unless the Data Element is
    /// unknown to the host, so an empty keyword on a tag the registry knows is
    /// a defect a schema-aware reader trips on. The spellings are taken from
    /// `standard/ps3.6/ups-native-model-keywords.json`, which records them
    /// straight from the PS3.6 registry.
    #[test]
    fn every_emitted_tag_carries_its_ps36_keyword() {
        for (tag, keyword) in [
            ("00080016", "SOPClassUID"),
            ("00080018", "SOPInstanceUID"),
            ("00080050", "AccessionNumber"),
            ("00080060", "Modality"),
            ("00080100", "CodeValue"),
            ("00080102", "CodingSchemeDesignator"),
            ("00080104", "CodeMeaning"),
            ("00081150", "ReferencedSOPClassUID"),
            ("00081155", "ReferencedSOPInstanceUID"),
            ("00081199", "ReferencedSOPSequence"),
            ("00100010", "PatientName"),
            ("00100020", "PatientID"),
            ("00100021", "IssuerOfPatientID"),
            ("0020000D", "StudyInstanceUID"),
            ("0020000E", "SeriesInstanceUID"),
            ("00321032", "RequestingPhysician"),
            ("00400002", "ScheduledProcedureStepStartDate"),
            ("00400003", "ScheduledProcedureStepStartTime"),
            ("00400010", "ScheduledStationName"),
            ("00401001", "RequestedProcedureID"),
            ("00401002", "ReasonForTheRequestedProcedure"),
            ("00404005", "ScheduledProcedureStepStartDateTime"),
            ("00404018", "ScheduledWorkitemCodeSequence"),
            ("00404019", "PerformedWorkitemCodeSequence"),
            ("00404021", "InputInformationSequence"),
            ("00404025", "ScheduledStationNameCodeSequence"),
            ("00404028", "PerformedStationNameCodeSequence"),
            ("00404033", "OutputInformationSequence"),
            ("00404041", "InputReadinessState"),
            ("00404050", "PerformedProcedureStepStartDateTime"),
            ("00404051", "PerformedProcedureStepEndDateTime"),
            ("00404052", "ProcedureStepCancellationDateTime"),
            ("0040A370", "ReferencedRequestSequence"),
            ("0040E020", "TypeOfInstances"),
            ("00741000", "ProcedureStepState"),
            ("00741002", "ProcedureStepProgressInformationSequence"),
            ("00741200", "ScheduledProcedureStepPriority"),
            ("00741202", "WorklistLabel"),
            ("00741204", "ProcedureStepLabel"),
            ("00741210", "ScheduledProcessingParametersSequence"),
            ("00741216", "UnifiedProcedureStepPerformedProcedureSequence"),
            ("00741238", "ReasonForCancellation"),
        ] {
            assert_eq!(
                keyword_for(tag),
                keyword,
                "tag {tag} must carry its PS3.6 keyword"
            );
        }
        // The shorter spelling is not a PS3.6 keyword and must not appear.
        assert_ne!(
            keyword_for("00741002"),
            "ProgressInformationSequence",
            "(0074,1002) is Procedure Step Progress Information Sequence"
        );
    }

    /// D2: `PersonName` carries `number="1"` - REQUIRED per PS3.19 A.1.5-2.
    #[test]
    fn a_person_name_carries_its_number_attribute() {
        let mut out = String::new();
        write_scalar(&mut out, "00100010", "PN", "SMITH^JOHN");
        assert!(
            out.contains("<PersonName number=\"1\">"),
            "PersonName must carry number=\"1\": {out}"
        );
    }

    /// D1: a zero-length PN has NO `PersonName` child - the element is not
    /// allowed when the value is empty (PS3.19 A.1.5-2).
    #[test]
    fn a_zero_length_person_name_has_no_person_name_element() {
        let mut out = String::new();
        write_scalar(&mut out, "00100010", "PN", "");
        assert!(
            !out.contains("<PersonName"),
            "a zero-length PN must not emit PersonName: {out}"
        );
        assert!(
            out.contains("keyword=\"PatientName\"") && out.contains("></DicomAttribute>"),
            "it is still an attribute element with the keyword and no children: {out}"
        );
    }

    /// D4: the component elements are the macro's names, not `Prefix`/`Suffix`.
    #[test]
    fn name_components_use_the_macro_element_names() {
        let mut out = String::new();
        write_scalar(&mut out, "00100010", "PN", "FAM^GIV^MID^DR^JR");
        for elem in [
            "FamilyName",
            "GivenName",
            "MiddleName",
            "NamePrefix",
            "NameSuffix",
        ] {
            assert!(
                out.contains(&format!("<{elem}>")),
                "missing <{elem}>: {out}"
            );
        }
        assert!(!out.contains("<Prefix>"), "not <Prefix>: {out}");
        assert!(!out.contains("<Suffix>"), "not <Suffix>: {out}");
    }

    /// A stored (0040,4021) the converter refuses (malformed JSON) must not be
    /// replaced by an item the Referenced Instances and Access Macro cannot
    /// fill: the record holds no SOP Instance UID for its input, so an item
    /// claiming DICOM instances while naming none would be unresolvable. The
    /// element is omitted instead.
    #[test]
    fn a_refused_input_information_emits_no_unresolvable_item() {
        let mut wi = workitem("1.2.3.1");
        wi.input_information = Some("{not valid json".to_string());
        wi.input_study_uid = Some("1.2.3".to_string());
        let xml = workitem_to_xml(&wi);
        assert!(
            !xml.contains("tag=\"00404021\""),
            "no item the macro cannot fill: {xml}"
        );
        assert!(
            !xml.contains("tag=\"0040E020\""),
            "no anonymous instance claim: {xml}"
        );
    }

    /// A record that knows its input study but no instance emits no Input
    /// Information Sequence item at all (PS3.3 Table 10-3b): the macro names
    /// instances through (0008,1199), and a study UID alone cannot fill it.
    #[test]
    fn a_study_uid_alone_emits_no_input_information_item() {
        let mut wi = workitem("1.2.3.5");
        wi.input_study_uid = Some("1.2.3.4.5".to_string());
        let xml = workitem_to_xml(&wi);
        assert!(
            !xml.contains("tag=\"00404021\""),
            "study UID alone is not a reference: {xml}"
        );
    }

    /// K1: the two Final State 2 sequences are stated even when empty.
    ///
    /// PS3.4 Table CC.2.5-1 types (0074,1002) and (0074,1216) Final State 2, so
    /// a retrieval always carries the elements. PS3.19 A.1 renders an empty
    /// sequence as an attribute element with no Item child - an empty ITEM
    /// would claim a report that states nothing.
    #[test]
    fn the_required_empty_sequences_are_stated() {
        let mut wi = workitem("1.2.3.9");
        wi.cancellation_datetime = None;
        wi.reason_for_cancellation = None;
        wi.performed_procedure_information = None;
        wi.output_information = None;
        wi.performed_start_datetime = None;
        wi.performed_end_datetime = None;
        let xml = workitem_to_xml(&wi);
        assert!(
            xml.contains(
                "tag=\"00741002\" vr=\"SQ\" \
                 keyword=\"ProcedureStepProgressInformationSequence\"></DicomAttribute>"
            ),
            "the progress sequence is present and empty: {xml}"
        );
        assert!(
            xml.contains(
                "tag=\"00741216\" vr=\"SQ\" \
                 keyword=\"UnifiedProcedureStepPerformedProcedureSequence\"></DicomAttribute>"
            ),
            "the performed sequence is present and empty: {xml}"
        );
        assert!(
            !xml.contains("keyword=\"ProcedureStepProgressInformationSequence\"><Item"),
            "no empty Item inside the progress sequence: {xml}"
        );
    }

    /// K3/K4: the Worklist Label is emitted before the Procedure Step Label,
    /// and the dataset-root Study Instance UID appears at the root while the
    /// input study stays inside the Referenced Request Sequence.
    #[test]
    fn the_worklist_label_and_the_root_study_are_emitted() {
        let mut wi = workitem("1.2.3.8");
        wi.worklist_label = Some("WL-CAD".to_string());
        wi.output_study_uid = Some("1.9.9".to_string());
        wi.input_study_uid = Some("1.2.3.4.5".to_string());
        let xml = workitem_to_xml(&wi);
        assert!(
            xml.contains("tag=\"00741202\" vr=\"LO\" keyword=\"WorklistLabel\">"),
            "the Worklist Label is present: {xml}"
        );
        let label = xml.find("tag=\"00741202\"").expect("label");
        let step = xml.find("tag=\"00741204\"").expect("step label");
        assert!(label < step, "(0074,1202) sorts before (0074,1204): {xml}");
        assert!(
            xml.contains(
                "tag=\"0020000D\" vr=\"UI\" keyword=\"StudyInstanceUID\">\
                 <Value number=\"1\">1.9.9</Value>"
            ),
            "the root study is the OUTPUT study: {xml}"
        );
        // The input study is still inside the Referenced Request Sequence.
        assert!(
            xml.contains(
                "tag=\"0020000D\" vr=\"UI\" keyword=\"StudyInstanceUID\">\
                          <Value number=\"1\">1.2.3.4.5</Value>"
            ) || xml.contains("<Value number=\"1\">1.2.3.4.5</Value>"),
            "the input study stays nested: {xml}"
        );
    }

    /// K5: an output item carries the complete macro, not just the SOP pair.
    ///
    /// PS3.3 Table 10-3b: the item needs Type of Instances, and conditionally
    /// the study and series UIDs. A pair-only item is an incomplete reference.
    #[test]
    fn an_output_item_carries_the_complete_macro() {
        let mut wi = workitem("1.2.3.7");
        wi.performed_start_datetime = Some("20261011090000".to_string());
        wi.output_information = Some(vec![clarus_ports::models::OutputReference {
            sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".to_string(),
            study_instance_uid: "1.2.3".to_string(),
            series_instance_uid: "1.2.3.4".to_string(),
            sop_instance_uid: "1.2.3.4.5".to_string(),
        }]);
        let xml = workitem_to_xml(&wi);
        assert!(
            xml.contains("tag=\"0040E020\" vr=\"CS\" keyword=\"TypeOfInstances\">"),
            "Type of Instances is Type 1 in the macro: {xml}"
        );
        assert!(
            xml.contains(
                "tag=\"0020000D\" vr=\"UI\" keyword=\"StudyInstanceUID\">\
                          <Value number=\"1\">1.2.3</Value>"
            ) || xml.contains("<Value number=\"1\">1.2.3</Value>"),
            "the output study is named: {xml}"
        );
        assert!(
            xml.contains("tag=\"0020000E\" vr=\"UI\" keyword=\"SeriesInstanceUID\">"),
            "the series is named: {xml}"
        );
    }

    /// A stored (0074,1216) the converter refuses must fall back to the typed
    /// projection (the start datetime child is written only by
    /// `write_performed_procedure`).
    #[test]
    fn a_refused_performed_procedure_falls_back_to_the_typed_projection() {
        let mut wi = workitem("1.2.3.2");
        wi.performed_procedure_information = Some("not json".to_string());
        wi.performed_start_datetime = Some("20261011090000".to_string());
        let xml = workitem_to_xml(&wi);
        assert!(
            xml.contains("tag=\"00741216\""),
            "the performed sequence must not disappear: {xml}"
        );
        assert!(
            xml.contains("tag=\"00404050\""),
            "the legacy projection carries the start datetime: {xml}"
        );
    }

    /// (0032,1032) Requesting Physician is a CHILD of (0040,A370) in the UPS
    /// IOD (PS3.4 Table CC.2.5-3), so it must not appear at the dataset root.
    /// A consumer walking the root would otherwise see a matching key the
    /// standard does not define there.
    #[test]
    fn the_requesting_physician_rides_inside_the_referenced_request() {
        let mut wi = workitem("1.2.3.3");
        wi.input_study_uid = Some("1.2.3.4.5".to_string());
        wi.requesting_physician = Some("SMITH^JOHN".to_string());
        let xml = workitem_to_xml(&wi);
        let seq = xml
            .split("tag=\"0040A370\"")
            .nth(1)
            .expect("the referenced request sequence is present");
        assert!(
            seq.contains("tag=\"00321032\""),
            "the physician is a child of the sequence: {xml}"
        );
        assert!(
            !xml.split("tag=\"0040A370\"")
                .next()
                .unwrap()
                .contains("00321032"),
            "no physician at the dataset root: {xml}"
        );
    }

    /// An item of (0040,A370) without (0020,000D) cannot be resolved by a
    /// consumer, so a workitem that names only an accession emits no item at
    /// all (rather than an item missing its Type 1 study).
    #[test]
    fn no_referenced_request_item_without_a_study() {
        let mut wi = workitem("1.2.3.4");
        wi.accession_number = Some("ACC-1".to_string());
        wi.requested_procedure_id = Some("RP-1".to_string());
        wi.requesting_physician = Some("SMITH^JOHN".to_string());
        wi.input_study_uid = None;
        let xml = workitem_to_xml(&wi);
        assert!(
            !xml.contains("tag=\"0040A370\""),
            "no study -> no item: {xml}"
        );
        assert!(
            !xml.contains("00321032"),
            "the physician travels only with the item: {xml}"
        );
    }
}
