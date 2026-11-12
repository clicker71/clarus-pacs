// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DICOMweb JSON serialization for UPS-RS workitems.
//!
//! Provides [`write_workitem_json`] (native, byte-exact with the legacy serde
//! render) and tag extraction helpers [`extract_tag`] and [`extract_pn`]
//! shared by create/update handlers.

#![forbid(unsafe_code)]

use clarus_ports::models::WorkItem;

use crate::handlers::json_common::{JsonVal, push_json_str};

/// The UPS Push SOP Class UID (PS3.4 Table CC.2.5-3, SOP Class UID row: the
/// workitem's own SOP Class, returned by N-GET and by retrieve).
pub const UPS_PUSH_SOP_CLASS_UID: &str = "1.2.840.10008.5.1.4.34.6.1";

/// Coding Scheme Designator of the codes this server mints itself.
///
/// PS3.3 Table 8.8-1 requires (0008,0102) on every code item and leaves the
/// scheme to the site; a locally assigned designator is the standard's own
/// answer for a code that belongs to no registered coding scheme. The value
/// is the DICOM convention for "locally assigned" (PS3.3 C.12.1.1), with the
/// site name appended the way such designators are written.
pub const STATION_CODING_SCHEME: &str = "99CLARUS";

/// Code Meaning of the station code (0008,0104).
///
/// The Code Value is an AE Title, which the standard defines as an
/// identifying string rather than a human-readable name, so the meaning has
/// to say what the value IS.
pub const STATION_CODE_MEANING: &str = "AI station";

/// Render a [`WorkItem`] as a DICOMweb JSON object into `out` (PS3.18
/// Appendix F), byte-exact with the legacy `serde_json::to_string` render.
///
/// The outer keys appear in alphabetical order; each present tag emits
/// `"TAG":{"Value":[<v>],"vr":"VR"}` (the `Value` key before `vr`). PN values
/// use the decomposed object form `{"Alphabetic":..}` (non-empty groups only,
/// in that key order). No serde on this path.
///
/// Identity (PS3.4 CC.2.5-3, the C-FIND unique return key): the workitem UID
/// is emitted in (0008,0018) SOP Instance UID, and the input study is named
/// through (0040,A370) Referenced Request Sequence and (0040,4021) Input
/// Information Sequence - never as a bare top-level (0020,000D).
///
/// Tags emitted: (0008,0016) SOP Class UID UI (always), (0008,0018) SOP
/// Instance UID UI (always), (0008,0060) Modality CS (vendor extension,
/// omitted when absent), (0010,0010) PatientName PN, (0010,0020) PatientID
/// LO, (0010,0021) IssuerOfPatientID LO (zero-length when the site declares
/// none), (0032,1032) RequestingPhysician PN, (0040,0002)/(0040,0003) start
/// date/time, (0040,0010) ScheduledStationName SH, (0040,4005)
/// ScheduledProcedureStepStartDateTime DT, (0040,4018) ScheduledWorkitemCode
/// Sequence SQ, (0040,4021) InputInformationSequence SQ, (0040,4025)
/// ScheduledStationNameCode Sequence SQ, (0040,4041) InputReadinessState CS,
/// (0040,4052) ProcedureStepCancellationDateTime DT, (0040,A370)
/// ReferencedRequestSequence SQ, (0074,1000) ProcedureStepState CS (always),
/// (0074,1200) ScheduledProcedureStepPriority CS, (0074,1204)
/// ProcedureStepLabel SH, (0074,1216) UnifiedProcedureStepPerformedProcedure
/// Sequence SQ - one item carrying (0040,4019) PerformedWorkitemCode
/// Sequence, (0040,4028) PerformedStationNameCode Sequence, (0040,4033)
/// OutputInformationSequence, (0040,4050)/(0040,4051) the performed start and
/// end datetimes - and (0074,1238) ReasonForCancellation LT.
///
/// The performer report is emitted ONLY inside (0074,1216): PS3.3 Table
/// C.30.3-1 nests every one of those attributes, so a bare top-level
/// (0040,4019)/(0040,4028)/(0040,4033) would be a shape no conformant reader
/// looks at. The item appears when at least one child has a value.
///
/// The Study Instance UID and the Accession Number are emitted ONLY inside
/// (0040,A370): the standard carries both there (CC.2.5-3), the requester
/// reads them there, and a bare top-level duplicate would state one fact
/// twice. A workitem that knows neither emits no sequence at all.
///
/// There is no (0008,0060) Modality in the UPS IOD: PS3.4 Table CC.2.5-3
/// defines no such attribute. The render carries it anyway as a documented
/// vendor extension, because the DIMSE MWL service requires Modality as a
/// Return key Type 1 (PS3.4 Table K.6-1) and the bridge maps this workitem
/// onto MWL. The value is the input study's modality, recorded at creation;
/// it is OMITTED when absent, so an externally created workitem renders
/// exactly as it did before this extension existed.
pub fn write_workitem_json(out: &mut Vec<u8>, wi: &WorkItem) {
    out.push(b'{');
    let mut wrote = false;
    // Emit a single non-PN attribute in alphabetical key order.
    //
    // PS3.18 F.2.5: a zero-length value is stated by OMITTING `Value`, not by
    // writing one empty string. `{"Value":[""],"vr":"LO"}` asserts that one
    // value exists and that it is the empty string - a different fact, and the
    // one the XML writer deliberately does not state (it emits an empty
    // element). The two renderers describe the same workitem.
    macro_rules! emit_str {
        ($key:expr, $vr:expr, $val:expr) => {
            if let Some(v) = $val {
                if wrote {
                    out.push(b',');
                }
                wrote = true;
                out.push(b'"');
                out.extend_from_slice($key.as_bytes());
                out.extend_from_slice(b"\":");
                if v.is_empty() {
                    out.extend_from_slice(b"{\"vr\":\"");
                    out.extend_from_slice($vr.as_bytes());
                    out.extend_from_slice(b"\"}");
                } else {
                    out.extend_from_slice(b"{\"Value\":[");
                    push_json_str(out, v);
                    out.extend_from_slice(b"],\"vr\":\"");
                    out.extend_from_slice($vr.as_bytes());
                    out.extend_from_slice(b"\"}");
                }
            }
        };
    }
    // Emit a single PN attribute (object form) in alphabetical key order.
    //
    // PS3.18 F.2.5 applies to PN like every other VR: a zero-length value is
    // stated by OMITTING `Value`. `{"Value":[{"Alphabetic":""}]}` instead
    // asserts one value exists whose alphabetic group is empty - a different
    // fact, and the one the XML writer deliberately does not state (it emits
    // no `PersonName` child for the same workitem).
    macro_rules! emit_pn {
        ($key:expr, $val:expr) => {
            if let Some(v) = $val {
                if wrote {
                    out.push(b',');
                }
                wrote = true;
                out.push(b'"');
                out.extend_from_slice($key.as_bytes());
                if v.is_empty() {
                    out.extend_from_slice(b"\":{\"vr\":\"PN\"}");
                } else {
                    out.extend_from_slice(b"\":{\"Value\":");
                    push_pn_object(out, v);
                    out.extend_from_slice(b",\"vr\":\"PN\"}");
                }
            }
        };
    }

    // Alphabetical order of the emitted keys.
    emit_str!("00080016", "UI", Some(UPS_PUSH_SOP_CLASS_UID));
    emit_str!("00080018", "UI", Some(wi.workitem_uid.as_str()));
    // (0008,0050) and (0020,000D) are NOT emitted bare: the standard carriage
    // is the Referenced Request Sequence below, and duplicating the two facts
    // at the top level would state them twice. See `emit_referenced_request`.
    // (0008,0060) Modality - vendor extension, see the docstring. Empty and
    // absent are the same statement here, so the filter collapses them into
    // the one the macro already understands.
    emit_str!(
        "00080060",
        "CS",
        wi.modality.as_deref().filter(|m| !m.is_empty())
    );
    emit_pn!("00100010", wi.patient_name.as_deref());
    emit_str!("00100020", "LO", wi.patient_id.as_deref()); // (0010,0021) is type 1C in the UPS request: it is REQUIRED whenever a
    // Patient ID is present, and a zero-length value is the standard's own
    // way of saying "present, and this site declares no issuer". Emitting
    // it only when a patient ID is present keeps a workitem with no
    // patient from carrying an issuer for a patient it does not name.
    if wi.patient_id.is_some() {
        // PS3.18 F.2.5: zero length is expressed by OMITTING `Value`, not by
        // writing one empty string. `{"Value":[""],"vr":"LO"}` asserts that
        // one value exists and that it is the empty string - a different
        // statement, and the one the XML writer deliberately does not make.
        // The two renderers describe the same workitem, so they must not
        // disagree about which fact is being stated.
        let issuer = wi.patient_id_issuer.as_deref().unwrap_or("");
        if issuer.is_empty() {
            if wrote {
                out.push(b',');
            }
            wrote = true;
            out.extend_from_slice(b"\"00100021\":{\"vr\":\"LO\"}");
        } else {
            emit_str!("00100021", "LO", Some(issuer));
        }
    }
    // (0032,1032) Requesting Physician is a CHILD of (0040,A370) Referenced
    // Request Sequence (PS3.4 Table CC.2.5-3), not a dataset-root attribute, so
    // it is written inside that sequence below. It used to be emitted at root,
    // where a reader binding by the standard's path never looks.
    // (0020,000D) at DATASET ROOT - the study assigned to the OUTPUT instances.
    // PS3.4 Table CC.2.5-1 lists it as its own row, Type 2, beside the Type 1
    // nested study inside the Referenced Request Sequence: they are different
    // facts (where the results go vs which study was read), so the root copy is
    // not a duplicate. Absent when the record names none.
    emit_str!("0020000D", "UI", wi.output_study_uid.as_deref());
    emit_str!("00400002", "DA", wi.procedure_step_start_date.as_deref());
    emit_str!("00400003", "TM", wi.procedure_step_start_time.as_deref());
    emit_str!("00400010", "SH", wi.station_name.as_deref());
    emit_str!("00404005", "DT", wi.sps_start_datetime.as_deref());
    // (0040,4018) < (0040,4021) < (0040,4025) in ascending tag order, so the
    // workitem code and the input study are emitted before the station
    // sequence, not after it.
    emit_workitem_code(out, wi.scheduled_workitem_code.as_ref(), &mut wrote);
    // (0040,4021) then (0040,4041)/(0040,4052): ascending tag order within the
    // (0040,40xx) block, which is where the input study and the state sit.
    //
    // When the full sequence was stored, it is emitted VERBATIM - re-projecting
    // it through the typed study UID would drop what the requester sent.
    // `push_json_str` would escape every quote; the stored bytes are already the
    // canonical JSON info object, so they are appended as-is (the same
    // discipline as (0074,1210) below, and allocation-free like the ap101 gate
    // requires).
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
    {
        if wrote {
            out.push(b',');
        }
        wrote = true;
        out.extend_from_slice(b"\"00404021\":");
        out.extend_from_slice(stored.as_bytes());
    }
    // (0040,4025) Scheduled Station Name Code Sequence - after (0040,4021) and
    // before (0040,4041), its ascending tag position.
    emit_scheduled_station_name_code(out, wi.station_aet.as_deref(), &mut wrote);
    emit_str!("00404041", "CS", wi.input_readiness.as_deref());
    // (0040,4052) and (0074,1238) are NOT top-level: PS3.3 Table C.30.1-1 makes
    // both children of (0074,1002) Progress Information Sequence, so they are
    // emitted there below. Emitting them at root states an attribute the
    // module does not put at root, and a reader binding by path finds nothing.
    emit_referenced_request(
        out,
        wi.accession_number.as_deref(),
        wi.input_study_uid.as_deref(),
        wi.requested_procedure_id.as_deref(),
        wi.reason_for_requested_procedure.as_deref(),
        wi.requesting_physician.as_deref(),
        &mut wrote,
    );
    emit_str!("00741000", "CS", Some(wi.workflow_state.as_str()));
    // (0074,1002) Progress Information Sequence - one item carrying the
    // cancellation facts (PS3.3 Table C.30.1-1). Emitted only when one of its
    // children has a value: an empty item states nothing and the sequence
    // itself is "Zero or one Item".
    emit_progress_information(
        out,
        wi.cancellation_datetime.as_deref(),
        wi.reason_for_cancellation.as_deref(),
        &mut wrote,
    );
    emit_str!("00741200", "CS", wi.sps_priority.as_deref());
    // (0074,1202) Worklist Label - Type 2/1 and a matching key (PS3.4 Table
    // CC.2.5-3). Ascending tag order puts it before (0074,1204).
    emit_str!("00741202", "LO", wi.worklist_label.as_deref());
    // (0074,1204) Procedure Step Label is VR LO (PS3.6), not SH: a label up to
    // 64 characters is legal, and an SH render would truncate it at 16.
    emit_str!("00741204", "LO", wi.step_label.as_deref());
    // (0074,1210) sorts after (0074,1204) and before (0074,1216) in ascending
    // tag order, which is the position the file's own convention puts it in.
    // It is stored ALREADY SERIALIZED, so it is appended verbatim.
    // `push_json_str` would escape every quote in it and hand the caller a
    // string where the standard puts a sequence; re-parsing and re-emitting
    // would allocate a second copy of the subtree on a read path that the
    // ap101 gate requires to be allocation-free.
    if let Some(stored) = wi
        .scheduled_processing_parameters
        .as_deref()
        .filter(|stored| !stored.is_empty())
    {
        if wrote {
            out.push(b',');
        }
        wrote = true;
        out.extend_from_slice(b"\"00741210\":");
        out.extend_from_slice(stored.as_bytes());
    }
    // When the full (0074,1216) was stored, it is emitted VERBATIM (the same
    // discipline as (0074,1210) above): the performer report may carry
    // children the typed projection does not model, and re-projecting would
    // drop them. Otherwise the typed projection is built below.
    if let Some(stored) = wi
        .performed_procedure_information
        .as_deref()
        .filter(|stored| !stored.is_empty())
    {
        if wrote {
            out.push(b',');
        }
        wrote = true;
        out.extend_from_slice(b"\"00741216\":");
        out.extend_from_slice(stored.as_bytes());
    } else {
        emit_performed_procedure_sequence(
            out,
            PerformedReport {
                start_datetime: wi.performed_start_datetime.as_deref(),
                end_datetime: wi.performed_end_datetime.as_deref(),
                station_name_code: wi.performed_station_name_code.as_ref(),
                workitem_code: wi.performed_workitem_code.as_ref(),
                output_information: wi.output_information.as_deref(),
            },
            &mut wrote,
        );
    }
    // The `wrote` flag is fully consumed by the emit pattern above; a final
    // read keeps the last assignment live.
    let _ = wrote;
    out.push(b'}');
}

/// Emit (0040,4018) Scheduled Workitem Code Sequence: one item, in tag order.
///
/// AIW-I 4.84.4.1.2.1 has the performer echo this code as (0040,4019) "if
/// present", and AIW-I 50.4.1.2 leaves the code values to deployment
/// agreement - which is why the requester has to send one at all. A code
/// with no Code Value is not a code and is not emitted.
fn emit_workitem_code(
    out: &mut Vec<u8>,
    code: Option<&clarus_ports::models::WorkItemCode>,
    wrote: &mut bool,
) {
    let Some(code) = code else {
        return;
    };
    if code.value.is_empty() {
        return;
    }
    if *wrote {
        out.push(b',');
    }
    *wrote = true;
    out.extend_from_slice(b"\"00404018\":{\"Value\":[{\"00080100\":{\"Value\":[");
    push_json_str(out, &code.value);
    out.extend_from_slice(b"],\"vr\":\"SH\"},\"00080102\":{\"Value\":[");
    push_json_str(out, &code.scheme);
    out.extend_from_slice(b"],\"vr\":\"SH\"},\"00080104\":{\"Value\":[");
    push_json_str(out, &code.meaning);
    out.extend_from_slice(b"],\"vr\":\"LO\"}}],\"vr\":\"SQ\"}");
}

/// The performer report as it appears on the wire: the attributes PS3.3 Table
/// C.30.3-1 nests inside (0074,1216).
///
/// Borrowed, so building it allocates nothing - the renderer runs on every UPS
/// read and the ap101 gate pins it at zero.
struct PerformedReport<'a> {
    start_datetime: Option<&'a str>,
    end_datetime: Option<&'a str>,
    station_name_code: Option<&'a clarus_ports::models::WorkItemCode>,
    workitem_code: Option<&'a clarus_ports::models::WorkItemCode>,
    output_information: Option<&'a [clarus_ports::models::OutputReference]>,
}

impl PerformedReport<'_> {
    /// Whether any child would be written. An item with no children renders as
    /// an empty `Value` array, which says "a performance was reported and it
    /// states nothing" - a statement the workitem does not make, since a record
    /// with no performed report keeps all five fields absent.
    fn is_empty(&self) -> bool {
        let code_present = |code: Option<&clarus_ports::models::WorkItemCode>| {
            code.is_some_and(|c| !c.value.is_empty())
        };
        self.start_datetime.is_none()
            && self.end_datetime.is_none()
            && !code_present(self.station_name_code)
            && !code_present(self.workitem_code)
            && self.output_information.is_none()
    }
}

/// Emit (0074,1216) Unified Procedure Step Performed Procedure Sequence.
///
/// PS3.3 Table C.30.3-1: this sequence is the ONLY place the UPS IOD carries
/// the performer report. (0040,4050)/(0040,4051) start and end datetimes,
/// (0040,4028) Performed Station Name Code, (0040,4019) Performed Workitem
/// Code and (0040,4033) Output Information Sequence are all CHILDREN of this
/// one item - a downstream consumer reads them here and nowhere else, so
/// emitting them bare would state the same facts in a shape a conformant reader
/// never looks at.
///
/// The item carries exactly the children the record holds, in ascending tag
/// order, so an update that reported only some of them round-trips as exactly
/// that: a performer that named its outputs but no end time is not made to
/// claim one on the way out.
///
/// This writes the three sub-elements of each code directly rather than
/// building owned `WorkItemCode` values, because this renderer runs on every
/// UPS read and the ap101 gate pins it at zero allocations; the scheme and
/// meaning are borrowed from the model, so nothing is copied.
fn emit_performed_procedure_sequence(
    out: &mut Vec<u8>,
    report: PerformedReport<'_>,
    wrote: &mut bool,
) {
    if report.is_empty() {
        // K1: PS3.4 Table CC.2.5-1 types (0074,1216) Final State 2, so a
        // retrieval always states the element. PS3.18 F.2.5 makes the empty
        // form `{"vr":"SQ"}` with no Value and no empty Item - an empty ITEM
        // would claim a performance that reported nothing.
        if *wrote {
            out.push(b',');
        }
        *wrote = true;
        out.extend_from_slice(b"\"00741216\":{\"vr\":\"SQ\"}");
        return;
    }
    if *wrote {
        out.push(b',');
    }
    *wrote = true;
    out.extend_from_slice(b"\"00741216\":{\"Value\":[{");
    // Children in ascending tag order: (0040,4019) < (0040,4028) <
    // (0040,4033) < (0040,4050) < (0040,4051).
    let mut item_wrote = false;
    if let Some(code) = report.workitem_code.filter(|c| !c.value.is_empty()) {
        emit_code_item(out, "00404019", code);
        item_wrote = true;
    }
    if let Some(code) = report.station_name_code.filter(|c| !c.value.is_empty()) {
        if item_wrote {
            out.push(b',');
        }
        emit_code_item(out, "00404028", code);
        item_wrote = true;
    }
    if let Some(outputs) = report.output_information {
        if item_wrote {
            out.push(b',');
        }
        emit_output_information(out, outputs);
        item_wrote = true;
    }
    if let Some(start) = report.start_datetime {
        if item_wrote {
            out.push(b',');
        }
        out.extend_from_slice(b"\"00404050\":{\"Value\":[");
        push_json_str(out, start);
        out.extend_from_slice(b"],\"vr\":\"DT\"}");
        item_wrote = true;
    }
    if let Some(end) = report.end_datetime {
        if item_wrote {
            out.push(b',');
        }
        out.extend_from_slice(b"\"00404051\":{\"Value\":[");
        push_json_str(out, end);
        out.extend_from_slice(b"],\"vr\":\"DT\"}");
    }
    out.extend_from_slice(b"}],\"vr\":\"SQ\"}");
}

/// Write one `CodeSequenceMacro` child as `"TAG":{...three sub-elements...}`,
/// with no leading comma - the caller decides where the separator goes.
///
/// PS3.3 Table 8.8-1: (0008,0100) Code Value, (0008,0102) Coding Scheme
/// Designator, (0008,0104) Code Meaning. A code with no Code Value is not a
/// code and is never emitted, which is the rule the create handler applies on
/// the way in.
fn emit_code_item(out: &mut Vec<u8>, tag: &str, code: &clarus_ports::models::WorkItemCode) {
    out.push(b'"');
    out.extend_from_slice(tag.as_bytes());
    out.extend_from_slice(b"\":{\"Value\":[{\"00080100\":{\"Value\":[");
    push_json_str(out, &code.value);
    out.extend_from_slice(b"],\"vr\":\"SH\"},\"00080102\":{\"Value\":[");
    push_json_str(out, &code.scheme);
    out.extend_from_slice(b"],\"vr\":\"SH\"},\"00080104\":{\"Value\":[");
    push_json_str(out, &code.meaning);
    out.extend_from_slice(b"],\"vr\":\"LO\"}}],\"vr\":\"SQ\"}");
}

/// Emit the (0040,4033) Output Information Sequence element itself: one item
/// per produced object, each a (0008,1150)/(0008,1155) reference pair - plus
/// (0020,000E) Series Instance UID when the performer named one (PS3.3
/// Table 10-3b types it 1C) - with no leading comma and no enclosing item
/// braces - the caller owns both.
///
/// This is how a COMPLETED workitem states its results (PS3.4 Table CC.2.5-3),
/// nested inside (0074,1216) per Table C.30.3-1. An empty list still emits the
/// element, because "the performer wrote a list and it is empty" is a fact the
/// record keeps apart from "no list was ever written"; PS3.18 F.2.5 makes the
/// empty form `{"vr":"SQ"}` with no `Value`, and both renderers agree.
fn emit_output_information(out: &mut Vec<u8>, outputs: &[clarus_ports::models::OutputReference]) {
    out.extend_from_slice(b"\"00404033\":");
    if outputs.is_empty() {
        // PS3.18 F.2.5: an attribute that is present but empty carries no
        // `Value`. `"Value":[]` asserts a zero-item value list, which is a
        // different statement from "the sequence is here and empty" - and the
        // XML renderer states the second by emitting an empty element.
        out.extend_from_slice(b"{\"vr\":\"SQ\"}");
        return;
    }
    out.extend_from_slice(b"{\"Value\":[");
    for (index, reference) in outputs.iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        // Each item is a Referenced Instances and Access Macro item (PS3.3
        // Table 10-3b): the references sit in a NESTED (0008,1199) Referenced
        // SOP Sequence, not flat on the item. Emitting the pair flat states a
        // shape the macro does not define for a sequence of outputs, and a
        // reader that walks (0008,1199) finds nothing to walk.
        out.extend_from_slice(b"{\"00081199\":{\"Value\":[{\"00081150\":{\"Value\":[");
        push_json_str(out, &reference.sop_class_uid);
        out.extend_from_slice(b"],\"vr\":\"UI\"},\"00081155\":{\"Value\":[");
        push_json_str(out, &reference.sop_instance_uid);
        out.extend_from_slice(b"],\"vr\":\"UI\"}}],\"vr\":\"SQ\"}");
        // (0020,000D) Study Instance UID - the macro's 1C study, present when
        // the output names one. It is what lets a consumer find the referenced
        // instance without a second query.
        if !reference.study_instance_uid.is_empty() {
            out.extend_from_slice(b",\"0020000D\":{\"Value\":[");
            push_json_str(out, &reference.study_instance_uid);
            out.extend_from_slice(b"],\"vr\":\"UI\"}");
        }
        // Omitted when the performer named no series, which keeps the wire
        // shape unchanged for a reference that has none.
        if !reference.series_instance_uid.is_empty() {
            out.extend_from_slice(b",\"0020000E\":{\"Value\":[");
            push_json_str(out, &reference.series_instance_uid);
            out.extend_from_slice(b"],\"vr\":\"UI\"}");
        }
        // (0040,E020) Type of Instances - Type 1 in the macro: without it the
        // item does not say what kind of instance it points at.
        out.extend_from_slice(b",\"0040E020\":{\"Value\":[\"DICOM\"],\"vr\":\"CS\"}");
        out.push(b'}');
    }
    out.extend_from_slice(b"],\"vr\":\"SQ\"}");
}
///
/// PS3.4 Table CC.2.5-3 carries the station as this sequence and gives
/// (0040,0001) Scheduled Station AE Title no row at all, so the code item is
/// the entire statement. AIW-I 4.81.4.1.2.1 Table 4.81.4.1.2.1-3 makes the
/// Code Value the AE Title a performer claims - which is also the key a
/// station-oriented search sends - so the station fact is stated once and in
/// the one place both ends read it.
///
/// This writes the three sub-elements directly rather than going through
/// `emit_code_attribute`, which takes an owned `WorkItemCode`: building that
/// value allocates, and this renderer runs on every UPS read (the ap101 gate
/// pins it at zero). The Coding Scheme Designator and the Code Meaning are
/// constants, so they are copied as bytes.
///
/// A station with no AE Title is not a station and emits no sequence; this
/// mirrors the create rule.
fn emit_scheduled_station_name_code(
    out: &mut Vec<u8>,
    station_aet: Option<&str>,
    wrote: &mut bool,
) {
    let Some(value) = station_aet else {
        return;
    };
    if *wrote {
        out.push(b',');
    }
    *wrote = true;
    out.extend_from_slice(b"\"00404025\":{\"Value\":[{\"00080100\":{\"Value\":[");
    push_json_str(out, value);
    out.extend_from_slice(b"],\"vr\":\"SH\"},\"00080102\":{\"Value\":[\"");
    out.extend_from_slice(STATION_CODING_SCHEME.as_bytes());
    out.extend_from_slice(b"\"],\"vr\":\"SH\"},\"00080104\":{\"Value\":[\"");
    out.extend_from_slice(STATION_CODE_MEANING.as_bytes());
    out.extend_from_slice(b"\"],\"vr\":\"LO\"}}],\"vr\":\"SQ\"}");
}

/// Emit (0040,A370) Referenced Request Sequence: one item carrying the
/// Accession Number and the Study Instance UID.
///
/// PS3.4 Table CC.2.5-3 types the sequence 2/2 at N-CREATE with a nested
/// Accession Number (0008,0050) Required and a nested Study Instance UID
/// (0020,000D) Optional. This is the carriage the task requester reads and
/// writes, so it - not the bare top-level tags - is how this server states the
/// two facts.
///
/// Emitted when EITHER value is known: a sequence with one nested element is
/// the honest shape for a workitem that knows the study but no accession (or
/// the reverse), and omitting the whole sequence would drop a fact the record
/// holds. Both absent -> nothing, the same honest skip the other emitters use.
///
/// The item's nested keys are in ascending tag order (0008,0050 before
/// 0020,000D), matching the alphabetical key order of the outer frame.
/// Emit (0074,1002) Progress Information Sequence, one item.
///
/// PS3.3 Table C.30.1-1 puts Procedure Step Cancellation DateTime (0040,4052)
/// and Reason For Cancellation (0074,1238) INSIDE this sequence, not at dataset
/// root. The sequence itself is "Zero or one Item", so nothing is emitted when
/// neither child has a value - an item holding no attribute states nothing.
fn emit_progress_information(
    out: &mut Vec<u8>,
    cancellation_datetime: Option<&str>,
    reason_for_cancellation: Option<&str>,
    wrote: &mut bool,
) {
    if cancellation_datetime.is_none() && reason_for_cancellation.is_none() {
        // K1: (0074,1002) is Final State 2 (PS3.4 Table CC.2.5-1), so the
        // element is present even when it holds no item. PS3.18 F.2.5: the
        // empty form carries no Value.
        if *wrote {
            out.push(b',');
        }
        *wrote = true;
        out.extend_from_slice(b"\"00741002\":{\"vr\":\"SQ\"}");
        return;
    }
    if *wrote {
        out.push(b',');
    }
    *wrote = true;
    out.extend_from_slice(b"\"00741002\":{\"vr\":\"SQ\",\"Value\":[{");
    // Children in ascending tag order: 0040,4052 before 0074,1238.
    let mut inner = false;
    if let Some(v) = cancellation_datetime {
        out.extend_from_slice(b"\"00404052\":{\"Value\":[");
        push_json_str(out, v);
        out.extend_from_slice(b"],\"vr\":\"DT\"}");
        inner = true;
    }
    if let Some(v) = reason_for_cancellation {
        if inner {
            out.push(b',');
        }
        out.extend_from_slice(b"\"00741238\":{\"Value\":[");
        push_json_str(out, v);
        out.extend_from_slice(b"],\"vr\":\"LT\"}");
    }
    out.extend_from_slice(b"}]}");
}

fn emit_referenced_request(
    out: &mut Vec<u8>,
    accession: Option<&str>,
    study_uid: Option<&str>,
    requested_procedure_id: Option<&str>,
    reason_for_requested_procedure: Option<&str>,
    requesting_physician: Option<&str>,
    wrote: &mut bool,
) {
    // E4: Study Instance UID is N-CREATE 1/1 in this sequence (PS3.4 Table
    // CC.2.5-3, `ups-cc-2.5-3-referenced-request-nested.json`), so an item
    // without one is not a valid item at all. Emitting an accession-only or
    // procedure-ID-only item states a reference to no study, which a consumer
    // cannot resolve and which the standard does not admit.
    let Some(study) = study_uid.filter(|u| !u.is_empty()) else {
        return;
    };
    if accession.is_none()
        && requested_procedure_id.is_none()
        && reason_for_requested_procedure.is_none()
        && requesting_physician.is_none()
    {
        // The study alone is still a complete reference (it is the required
        // child), so an item is emitted below - this arm only documents that
        // the other four are optional.
    }
    if *wrote {
        out.push(b',');
    }
    *wrote = true;
    out.extend_from_slice(b"\"0040A370\":{\"Value\":[{");
    let mut item_wrote = false;
    if let Some(accession) = accession.filter(|a| !a.is_empty()) {
        out.extend_from_slice(b"\"00080050\":{\"Value\":[");
        push_json_str(out, accession);
        out.extend_from_slice(b"],\"vr\":\"SH\"}");
        item_wrote = true;
    }
    {
        if item_wrote {
            out.push(b',');
        }
        out.extend_from_slice(b"\"0020000D\":{\"Value\":[");
        push_json_str(out, study);
        out.extend_from_slice(b"],\"vr\":\"UI\"}");
        item_wrote = true;
    }
    // (0032,1032) Requesting Physician is a CHILD of this sequence (PS3.4 Table
    // CC.2.5-3), not a dataset-root attribute. Ascending tag order puts it
    // before (0040,1001).
    if let Some(physician) = requesting_physician.filter(|p| !p.is_empty()) {
        if item_wrote {
            out.push(b',');
        }
        // K6: a PN is three `=`-separated groups, and each non-empty one is
        // its own JSON member (PS3.18 F.2.1/F.2.2). Writing the raw string as
        // `Alphabetic` merges an ideographic or phonetic spelling into a group
        // the reader then mis-renders; `push_pn_object` splits them exactly as
        // the patient-name path does.
        out.extend_from_slice(b"\"00321032\":{\"Value\":");
        push_pn_object(out, physician);
        out.extend_from_slice(b",\"vr\":\"PN\"}");
        item_wrote = true;
    }
    // (0040,1001) Requested Procedure ID - a REQUIRED matching key of
    // CC.2.5-3, and a key the requester reads back to correlate the step with
    // its own order. Ascending tag order puts it after (0020,000D).
    if let Some(procedure) = requested_procedure_id {
        if item_wrote {
            out.push(b',');
        }
        out.extend_from_slice(b"\"00401001\":{\"Value\":[");
        push_json_str(out, procedure);
        out.extend_from_slice(b"],\"vr\":\"SH\"}");
        item_wrote = true;
    }
    // (0040,1002) Reason for the Requested Procedure - the free-text reason the
    // requester gave, and the one nested attribute the Update transaction may
    // replace. Ascending tag order puts it after (0040,1001).
    if let Some(reason) = reason_for_requested_procedure {
        if item_wrote {
            out.push(b',');
        }
        out.extend_from_slice(b"\"00401002\":{\"Value\":[");
        push_json_str(out, reason);
        out.extend_from_slice(b"],\"vr\":\"LO\"}");
    }
    out.extend_from_slice(b"}],\"vr\":\"SQ\"}");
}

/// Append a JSON PersonName object in the `{"Value":[{"Alphabetic":..}]}`
/// frame (the caller has already written the tag + `"Value":`), reproducing
/// the serde-era `build_pn_json` + BTreeMap order: `Alphabetic` always,
/// `Ideographic`/`Phonetic` only when the raw `=`-group is non-empty.
fn push_pn_object(out: &mut Vec<u8>, raw: &str) {
    let mut groups = raw.split('=');
    let alphabetic = groups.next().unwrap_or("");
    let ideographic = groups.next().filter(|s| !s.is_empty());
    let phonetic = groups.next().filter(|s| !s.is_empty());
    out.extend_from_slice(b"[{\"Alphabetic\":");
    push_json_str(out, alphabetic);
    if let Some(i) = ideographic {
        out.extend_from_slice(b",\"Ideographic\":");
        push_json_str(out, i);
    }
    if let Some(p) = phonetic {
        out.extend_from_slice(b",\"Phonetic\":");
        push_json_str(out, p);
    }
    out.extend_from_slice(b"}]");
}

// -- Tag extraction helpers (CS-03) --------------------------------------

/// Extract a string value from a DICOMweb JSON tag entry (native DOM).
///
/// Navigates: `{tag}.Value[0]` -> string.
/// Shared by create/update workitem handlers to eliminate copy-paste.
pub fn extract_tag(obj: &JsonVal, tag: &str) -> Option<String> {
    obj.obj_get(tag)?
        .obj_get("Value")?
        .as_str_array_first()
        .map(|s| s.to_string())
}

/// Extract a PersonName from a DICOMweb JSON tag entry (native DOM).
///
/// Navigates: `{tag}.Value[0].Alphabetic/Ideographic/Phonetic` -> the three
/// `=`-joined component groups.
/// Falls back to `{tag}.Value[0]` as plain string.
/// Shared by create/update workitem handlers to eliminate copy-paste.
pub fn extract_pn(obj: &JsonVal, tag: &str) -> Option<String> {
    let entry = obj.obj_get(tag)?.obj_get("Value")?;
    let first = entry.first_of_array()?;
    match first {
        JsonVal::Obj(_) => {
            // PS3.5 6.2: a PersonName carries up to three component groups -
            // alphabetic, ideographic, phonetic - joined by `=`. `push_pn_object`
            // always writes `Alphabetic` (possibly empty) and omits the other
            // two when their group is empty, but a peer may legally send only
            // `Ideographic`: a missing group is EMPTY, not a reason to drop the
            // name. Only an all-empty PN yields None.
            let alphabetic = first
                .obj_get("Alphabetic")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ideographic = first
                .obj_get("Ideographic")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let phonetic = first
                .obj_get("Phonetic")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if alphabetic.is_empty() && ideographic.is_empty() && phonetic.is_empty() {
                return None;
            }
            let mut out = alphabetic.to_string();
            // The `=` separators are positional: an empty alphabetic group is
            // still a group, so a non-empty ideographic must be preceded by its
            // separator or it would be read back as the alphabetic group.
            if !ideographic.is_empty() || !phonetic.is_empty() {
                out.push('=');
                out.push_str(ideographic);
            }
            if !phonetic.is_empty() {
                out.push('=');
                out.push_str(phonetic);
            }
            Some(out)
        }
        JsonVal::Str(s) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// E2: each Output Information item nests its references in (0008,1199).
    ///
    /// PS3.3 Table 10-3b makes the reference pair children of a Referenced SOP
    /// Sequence inside the pointing item. Emitting the pair flat states a shape
    /// the macro does not define, and a reader that walks (0008,1199) finds
    /// nothing.
    #[test]
    fn output_items_nest_their_references_in_the_referenced_sop_sequence() {
        let mut wi = sample_workitem();
        wi.output_information = Some(vec![clarus_ports::models::OutputReference {
            sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".to_string(),
            study_instance_uid: "1.2.3".to_string(),
            series_instance_uid: "1.2.3.4".to_string(),
            sop_instance_uid: "1.2.3.4.5".to_string(),
        }]);
        let mut buf = Vec::new();
        write_workitem_json(&mut buf, &wi);
        let json = String::from_utf8(buf).expect("utf8");

        assert!(
            json.contains("\"00081199\":{\"Value\":[{\"00081150\""),
            "the reference pair must sit inside (0008,1199): {json}"
        );
        // The nested pair, then the series at the item level.
        assert!(
            json.contains(
                "\"00081155\":{\"Value\":[\"1.2.3.4.5\"],\"vr\":\"UI\"}}],\"vr\":\"SQ\"}"
            ),
            "the Referenced SOP Sequence closes before the series: {json}"
        );
        assert!(
            json.contains("\"0020000E\":{\"Value\":[\"1.2.3.4\"],\"vr\":\"UI\"}"),
            "the series stays at the item level: {json}"
        );
        // K5: the macro's own children are Type 1 / 1C and must be present on
        // the item, not only the SOP pair. A pair-only item is an incomplete
        // reference: nothing says WHAT KIND of instance it points at.
        assert!(
            json.contains("\"0040E020\":{\"Value\":[\"DICOM\"],\"vr\":\"CS\"}"),
            "Type of Instances is Type 1 in the macro: {json}"
        );
        assert!(
            json.contains("\"0020000D\":{\"Value\":[\"1.2.3\"],\"vr\":\"UI\"}"),
            "the output study is named: {json}"
        );
    }

    /// E1: the cancellation facts are children of (0074,1002), not root.
    ///
    /// PS3.3 Table C.30.1-1 places (0040,4052) and (0074,1238) inside Progress
    /// Information Sequence. A reader binding by the standard's path finds them
    /// there and nowhere else, so emitting them at root was invisible to it.
    #[test]
    fn the_cancellation_facts_are_nested_in_the_progress_sequence() {
        let mut wi = sample_workitem();
        wi.cancellation_datetime = Some("20261111120000".to_string());
        wi.reason_for_cancellation = Some("no longer needed".to_string());
        let mut buf = Vec::new();
        write_workitem_json(&mut buf, &wi);
        let json = String::from_utf8(buf).expect("utf8");

        // Present inside the sequence item.
        assert!(
            json.contains("\"00741002\":{\"vr\":\"SQ\",\"Value\":[{"),
            "the progress sequence must be emitted: {json}"
        );
        assert!(
            json.contains("\"00404052\":{\"Value\":[\"20261111120000\"],\"vr\":\"DT\"}"),
            "the cancellation datetime belongs in the item: {json}"
        );
        assert!(
            json.contains("\"00741238\":{\"Value\":[\"no longer needed\"],\"vr\":\"LT\"}"),
            "the reason belongs in the item: {json}"
        );

        // Absent at root. Each key appears exactly ONCE in the whole document,
        // and that occurrence is inside the sequence.
        for key in ["\"00404052\":", "\"00741238\":"] {
            assert_eq!(
                json.matches(key).count(),
                1,
                "{key} must appear once (inside the item), not also at root: {json}"
            );
        }
    }

    /// K1: a workitem with no cancellation facts still states the progress
    /// sequence - (0074,1002) is Final State 2 (PS3.4 Table CC.2.5-1), so a
    /// retrieval always carries the element. PS3.18 F.2.5 makes the empty form
    /// `{"vr":"SQ"}`: no `Value`, and no empty item either, because an empty
    /// item would claim a progress report that states nothing.
    #[test]
    fn an_empty_progress_sequence_is_stated_without_a_value() {
        let wi = sample_workitem();
        let mut buf = Vec::new();
        write_workitem_json(&mut buf, &wi);
        let json = String::from_utf8(buf).expect("utf8");
        assert!(
            json.contains("\"00741002\":{\"vr\":\"SQ\"}"),
            "the element is present and carries no Value: {json}"
        );
        assert!(
            !json.contains("\"00741002\":{\"Value\":["),
            "no Value member for a zero-item sequence: {json}"
        );
    }

    /// K2: the zero-length rule holds for every emitter, not only the issuer.
    ///
    /// PS3.18 F.2.5: an attribute present with zero length carries no `Value`.
    /// A blank PN must not become `Value:[{"Alphabetic":""}]` (that asserts one
    /// value whose alphabetic group is empty), a blank accession must not
    /// become `Value:[""]`, and an empty output list must not become `Value:[]`.
    #[test]
    fn a_zero_length_value_omits_the_value_member_everywhere() {
        let mut wi = sample_workitem();
        wi.patient_name = Some(String::new());
        wi.accession_number = Some(String::new());
        wi.output_information = Some(Vec::new());
        let mut out = Vec::new();
        write_workitem_json(&mut out, &wi);
        let text = std::str::from_utf8(&out).unwrap();
        assert!(
            text.contains("\"00100010\":{\"vr\":\"PN\"}"),
            "a blank PN is a zero-length attribute: {text}"
        );
        assert!(
            !text.contains("\"00100010\":{\"Value\":[{\"Alphabetic\":\"\"}]"),
            "no empty Alphabetic member: {text}"
        );
        assert!(
            !text.contains("\"00080050\":{\"Value\":[\"\"]"),
            "a blank accession is a zero-length attribute: {text}"
        );
        assert!(
            text.contains("\"00404033\":{\"vr\":\"SQ\"}"),
            "an empty output list states the sequence with no Value: {text}"
        );
        assert!(
            !text.contains("\"00404033\":{\"Value\":[]"),
            "no empty Value array for a zero-item sequence: {text}"
        );
    }

    /// K3/K4: the Worklist Label is emitted before the Procedure Step Label,
    /// and the dataset-root Study Instance UID is emitted at the root.
    ///
    /// PS3.4 Table CC.2.5-3 types (0074,1202) 2/1 and makes it a matching key,
    /// so it sits in ascending tag order before (0074,1204). The root
    /// (0020,000D) is Table CC.2.5-1's own Type 2 row: the study assigned to
    /// the OUTPUT instances, a different fact from the input study nested in
    /// the Referenced Request Sequence.
    #[test]
    fn the_worklist_label_and_the_root_study_are_emitted() {
        let mut wi = sample_workitem();
        wi.worklist_label = Some("WL-CAD".into());
        wi.step_label = Some("STEP-CAD".into());
        wi.output_study_uid = Some("1.9.9".into());
        wi.input_study_uid = Some("1.2.3.4.5".into());
        let mut out = Vec::new();
        write_workitem_json(&mut out, &wi);
        let text = std::str::from_utf8(&out).unwrap();
        assert!(
            text.contains("\"00741202\":{\"Value\":[\"WL-CAD\"],\"vr\":\"LO\"}"),
            "the Worklist Label is present: {text}"
        );
        let label = text.find("\"00741202\"").expect("label");
        let step = text.find("\"00741204\"").expect("step label");
        assert!(label < step, "(0074,1202) sorts before (0074,1204): {text}");
        // The root study comes before (0040,0002) in ascending tag order.
        let root = text.find("\"0020000D\"").expect("root study");
        let start = text.find("\"00400002\"").expect("start date");
        assert!(
            root < start,
            "the root (0020,000D) is at dataset level: {text}"
        );
        assert!(
            text.contains("\"0020000D\":{\"Value\":[\"1.9.9\"],\"vr\":\"UI\"}"),
            "the root study is the OUTPUT study, not the input one: {text}"
        );
    }

    /// K6: the requesting physician's PN groups are decomposed, so `A=I=P`
    /// yields three separate members instead of one merged Alphabetic string.
    #[test]
    fn the_requesting_physician_decomposes_its_pn_groups() {
        let mut wi = sample_workitem();
        wi.input_study_uid = Some("1.2.3.4.5".into());
        wi.requesting_physician = Some("ALPHA=IDEO=PHON".into());
        let mut out = Vec::new();
        write_workitem_json(&mut out, &wi);
        let text = std::str::from_utf8(&out).unwrap();
        assert!(
            text.contains(
                "\"00321032\":{\"Value\":[{\"Alphabetic\":\"ALPHA\",\
                 \"Ideographic\":\"IDEO\",\"Phonetic\":\"PHON\"}],\"vr\":\"PN\"}"
            ),
            "three groups become three members: {text}"
        );
        assert!(
            !text.contains("\"Alphabetic\":\"ALPHA=IDEO=PHON\""),
            "the raw `=`-joined string must not be one Alphabetic value: {text}"
        );
    }

    fn sample_workitem() -> WorkItem {
        WorkItem {
            id: Some(1),
            workitem_uid: "1.2.826.0.1.9".into(),
            workflow_state: "SCHEDULED".into(),
            station_aet: Some("CT1".into()),
            station_name: Some("Scanner A".into()),
            procedure_step_start_date: Some("20260802".into()),
            procedure_step_start_time: Some("103000".into()),
            patient_name: Some("SMIRNOV^IVAN".into()),
            patient_id: Some("PT-1".into()),
            accession_number: Some("ACC-9".into()),
            requesting_physician: Some("DR^SMITH".into()),
            // No Modality: this fixture models a workitem with no input study
            // in scope, which is the case that must keep omitting the element.
            modality: None,
            sps_priority: Some("HIGH".into()),
            transaction_uid: None,
            created_at: Some("2026-08-18T10:00:00Z".into()),
            updated_at: None,
            step_label: None,
            worklist_label: None,
            output_study_uid: None,
            input_readiness: None,
            sps_start_datetime: None,
            input_study_uid: None,
            input_information: None,
            reason_for_cancellation: None,
            cancellation_datetime: None,
            patient_id_issuer: None,
            scheduled_workitem_code: None,
            output_information: None,
            performed_workitem_code: None,
            performed_station_name_code: None,
            requested_procedure_id: None,
            performed_start_datetime: None,
            performed_end_datetime: None,
            scheduled_processing_parameters: None,
            performed_procedure_information: None,
            reason_for_requested_procedure: None,
        }
    }

    /// The identity shape of PS3.4 CC.2.5-3: the workitem UID is the SOP
    /// Instance UID, the SOP Class UID is the UPS Push class, and the study
    /// and the accession appear ONLY inside the Referenced Request Sequence
    /// (0040,A370) - the bare top-level tags are gone, because the sequence is
    /// the standard carriage and stating a fact twice is not an option.
    /// (0010,0021) Issuer of Patient ID follows the Patient ID and is
    /// ZERO-LENGTH when the site declares none - the standard's way of saying
    /// "present, no issuer" (AIW-I 4.80.4.1.2.1). Alphabetical key order is
    /// what the goldens freeze, and (0040,A370) sits between (0040,000A) and
    /// (0074,1000).
    #[test]
    fn the_top_level_tags_ascend() {
        let mut wi = sample_workitem();
        wi.input_study_uid = Some("1.2.3.4.5".into());
        // Populate the two attributes whose ascending order the (0040,40xx)
        // block depends on: (0040,4018) before (0040,4025).
        wi.scheduled_workitem_code = Some(clarus_ports::models::WorkItemCode {
            value: "CODE".into(),
            scheme: "99CLARUS".into(),
            meaning: "A step".into(),
        });
        let mut out = Vec::new();
        write_workitem_json(&mut out, &wi);
        let text = std::str::from_utf8(&out).unwrap();
        // Walk the rendered text tracking brace depth: only depth-1 keys are
        // dataset-level. A key is a `"XXXXXXXX"` string immediately followed by
        // `:` at that depth.
        let bytes = text.as_bytes();
        let mut depth = 0i32;
        let mut tags: Vec<String> = Vec::new();
        let mut i = 0usize;
        while i < bytes.len() {
            match bytes[i] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                b'"' => {
                    let start = i + 1;
                    let Some(off) = text[start..].find('"') else {
                        break;
                    };
                    let key = &text[start..start + off];
                    let after = start + off + 1;
                    if depth == 1
                        && bytes.get(after) == Some(&b':')
                        && key.len() == 8
                        && key.bytes().all(|b| b.is_ascii_digit())
                    {
                        tags.push(key.to_string());
                    }
                    i = after;
                    continue;
                }
                _ => {}
            }
            i += 1;
        }
        assert!(
            tags.len() >= 10,
            "the walk found the dataset level: {tags:?}"
        );
        let mut sorted = tags.clone();
        sorted.sort_unstable();
        assert_eq!(
            tags, sorted,
            "top-level tags must ascend (PS3.18 F.2): {text}"
        );
    }

    /// The identity shape of PS3.4 CC.2.5-3: the workitem UID is the SOP
    /// Instance UID, the SOP Class UID is the UPS Push class, and the study
    /// and the accession appear ONLY inside the Referenced Request Sequence
    /// (0040,A370) - the bare top-level tags are gone, because the sequence is
    /// the standard carriage and stating a fact twice is not an option.
    /// (0010,0021) Issuer of Patient ID follows the Patient ID and is
    /// ZERO-LENGTH when the site declares none - the standard's way of saying
    /// "present, no issuer" (AIW-I 4.80.4.1.2.1). Alphabetical key order is
    /// what the goldens freeze, and (0040,A370) sits between (0040,000A) and
    /// (0074,1000).
    #[test]
    fn write_workitem_json_carries_the_standard_identity() {
        let mut out = Vec::new();
        write_workitem_json(&mut out, &sample_workitem());
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            "{\"00080016\":{\"Value\":[\"1.2.840.10008.5.1.4.34.6.1\"],\"vr\":\"UI\"},\
             \"00080018\":{\"Value\":[\"1.2.826.0.1.9\"],\"vr\":\"UI\"},\
             \"00100010\":{\"Value\":[{\"Alphabetic\":\"SMIRNOV^IVAN\"}],\"vr\":\"PN\"},\
             \"00100020\":{\"Value\":[\"PT-1\"],\"vr\":\"LO\"},\
             \"00100021\":{\"vr\":\"LO\"},\
             \"00400002\":{\"Value\":[\"20260802\"],\"vr\":\"DA\"},\
             \"00400003\":{\"Value\":[\"103000\"],\"vr\":\"TM\"},\
             \"00400010\":{\"Value\":[\"Scanner A\"],\"vr\":\"SH\"},\
             \"00404025\":{\"Value\":[{\"00080100\":{\"Value\":[\"CT1\"],\"vr\":\"SH\"},\
             \"00080102\":{\"Value\":[\"99CLARUS\"],\"vr\":\"SH\"},\
             \"00080104\":{\"Value\":[\"AI station\"],\"vr\":\"LO\"}}],\"vr\":\"SQ\"},\
             \"00741000\":{\"Value\":[\"SCHEDULED\"],\"vr\":\"CS\"},\
             \"00741002\":{\"vr\":\"SQ\"},\
             \"00741200\":{\"Value\":[\"HIGH\"],\"vr\":\"CS\"},\
             \"00741216\":{\"vr\":\"SQ\"}}"
        );
        // The bare duplicates are GONE: the sequence is the only carriage.
        let text = std::str::from_utf8(&out).unwrap();
        assert!(
            !text.contains("\"00080050\":{")
                || text.contains("\"0040A370\":{\"Value\":[{\"00080050\""),
            "(0008,0050) must appear only inside the sequence: {text}"
        );
        assert!(
            !text.contains("\"0020000D\":{"),
            "no bare (0020,000D) at the top level: {text}"
        );
    }

    /// A zero-length (0040,0010) Scheduled Station Name is stated the same way
    /// as the issuer: the attribute is PRESENT with no `Value` member
    /// (PS3.18 F.2.5). Every attribute goes through one `emit_str` macro, so
    /// this pins the macro, not one call site.
    #[test]
    fn a_zero_length_scalar_carries_no_value_member() {
        let mut wi = sample_workitem();
        wi.station_name = Some(String::new());
        let mut out = Vec::new();
        write_workitem_json(&mut out, &wi);
        let text = std::str::from_utf8(&out).unwrap();
        assert!(
            text.contains("\"00400010\":{\"vr\":\"SH\"}"),
            "zero length is no Value member: {text}"
        );
        assert!(
            !text.contains("\"00400010\":{\"Value\":[\"\"]"),
            "an empty string is a different statement: {text}"
        );
    }

    /// A zero-length (0010,0021) is stated by OMITTING `Value`.
    ///
    /// PS3.18 F.2.5: zero length means no `Value` member at all.
    /// `{"Value":[""],"vr":"LO"}` instead claims one value exists and that it
    /// is the empty string - a different fact, and the one the XML writer
    /// deliberately does not state for the same workitem. The two renderers
    /// describe one object and must agree.
    #[test]
    fn a_zero_length_patient_id_issuer_carries_no_value_member() {
        let mut wi = sample_workitem();
        wi.patient_id_issuer = None;
        let mut out = Vec::new();
        write_workitem_json(&mut out, &wi);
        let text = std::str::from_utf8(&out).unwrap();
        assert!(
            text.contains("\"00100021\":{\"vr\":\"LO\"}"),
            "the attribute is PRESENT (type 1C) but carries no Value: {text}"
        );
        assert!(
            !text.contains("\"00100021\":{\"Value\":[\"\"]"),
            "zero length must not be written as one empty value: {text}"
        );
    }

    /// The INPUT study is stated through the Referenced Request Sequence
    /// (0040,A370); the dataset-root (0020,000D) is a DIFFERENT fact and is
    /// emitted separately from `output_study_uid`.
    ///
    /// PS3.4 Table CC.2.5-1 lists two rows: 'Study Instance UID (Referenced
    /// Request Sequence)' Type 1 and 'Study Instance UID (top level)' Type 2.
    /// The root one is the study assigned to the OUTPUT instances, so a record
    /// holding only an input study emits no root element at all - the two are
    /// not interchangeable copies of one value.
    ///
    /// The Input Information Sequence (0040,4021) is a Referenced Instances and
    /// Access Macro item (PS3.3 Table 10-3b): it names instances through
    /// (0008,1199), which a study UID alone cannot fill. A record that holds
    /// only the study therefore emits no input item.
    #[test]
    fn a_study_named_by_a_workitem_lands_in_the_referenced_request() {
        let mut wi = sample_workitem();
        wi.input_study_uid = Some("1.2.3.4.5".into());
        let mut out = Vec::new();
        write_workitem_json(&mut out, &wi);
        let text = std::str::from_utf8(&out).unwrap();
        // No root study: `output_study_uid` is None here, and the input study
        // is a DIFFERENT attribute that lives only inside the sequence.
        let head = text.split("\"00400002\"").next().unwrap();
        assert!(
            !head.contains("\"0020000D\""),
            "no root (0020,000D) when no output study is named: {text}"
        );
        // The input study appears exactly once - inside the Referenced Request.
        assert_eq!(
            text.matches("\"0020000D\":{\"Value\":[\"1.2.3.4.5\"]")
                .count(),
            1,
            "the input study rides only in the referenced request: {text}"
        );
        assert!(
            !text.contains("\"00404021\""),
            "no input item without an instance reference: {text}"
        );
        // The Referenced Request Sequence carries the study, the accession the
        // fixture sets, and the requesting physician - alphabetical nested
        // order, physician last.
        assert!(
            text.contains(
                "\"0040A370\":{\"Value\":[{\"00080050\":{\"Value\":[\"ACC-9\"],\"vr\":\"SH\"},\
                 \"0020000D\":{\"Value\":[\"1.2.3.4.5\"],\"vr\":\"UI\"},\
                 \"00321032\":{\"Value\":[{\"Alphabetic\":\"DR^SMITH\"}],\"vr\":\"PN\"}}],\"vr\":\"SQ\"}"
            ),
            "{text}"
        );
    }

    /// The Referenced Request Sequence is emitted with ONLY the values a
    /// workitem knows. PS3.3 CC.2.5-2 makes (0020,000D) Type 1 in the item, so
    /// the item exists only when a study is named; the accession and the
    /// requesting physician ride inside it and never at the top level.
    #[test]
    fn referenced_request_sequence_emits_what_is_known() {
        // Study only (no accession): the sample has ACC-9, so clear it.
        let mut wi = sample_workitem();
        wi.accession_number = None;
        wi.input_study_uid = Some("1.2.3.4.5".into());
        let mut out = Vec::new();
        write_workitem_json(&mut out, &wi);
        let text = std::str::from_utf8(&out).unwrap();
        assert!(
            text.contains(
                "\"0040A370\":{\"Value\":[{\"0020000D\":{\"Value\":[\"1.2.3.4.5\"],\"vr\":\"UI\"},\
                 \"00321032\":{\"Value\":[{\"Alphabetic\":\"DR^SMITH\"}],\"vr\":\"PN\"}}],\"vr\":\"SQ\"}"
            ),
            "study-only sequence: {text}"
        );

        // Accession only (no study): PS3.3 CC.2.5-2 requires (0020,000D) as
        // Type 1 in the item, so no item is emitted at all - an accession with
        // no study cannot be referenced.
        let mut wi = sample_workitem();
        wi.input_study_uid = None;
        let mut out = Vec::new();
        write_workitem_json(&mut out, &wi);
        let text = std::str::from_utf8(&out).unwrap();
        assert!(
            !text.contains("0040A370"),
            "no study -> no item, even with an accession: {text}"
        );
        assert!(
            !text.contains("00321032"),
            "the physician is carried only by the item: {text}"
        );

        // Neither: no sequence at all (the honest skip).
        let mut wi = sample_workitem();
        wi.accession_number = None;
        wi.input_study_uid = None;
        let mut out = Vec::new();
        write_workitem_json(&mut out, &wi);
        let text = std::str::from_utf8(&out).unwrap();
        assert!(
            !text.contains("0040A370"),
            "neither value -> no sequence: {text}"
        );
    }

    /// Modality is not a UPS attribute (PS3.4 Table CC.2.5-3), but the render
    /// carries it as a vendor extension when the workitem has one, because the
    /// DIMSE MWL service requires it as a Return key Type 1. It is placed at
    /// its alphabetical position, between (0008,0018) and (0010,0010).
    #[test]
    fn modality_is_rendered_when_set() {
        let mut wi = sample_workitem();
        wi.modality = Some("CT".into());
        let mut out = Vec::new();
        write_workitem_json(&mut out, &wi);
        let text = std::str::from_utf8(&out).unwrap();
        assert!(
            text.contains(
                "\"00080018\":{\"Value\":[\"1.2.826.0.1.9\"],\"vr\":\"UI\"},\"00080060\":{\"Value\":[\"CT\"],\"vr\":\"CS\"},\"00100010\""
            ),
            "(0008,0060) must sit between SOPInstanceUID and PatientName: {text}"
        );
    }

    /// The honest skip: a workitem with no Modality (an externally created one,
    /// or a study that declared none) omits the element entirely - a
    /// zero-length value would be a different statement.
    #[test]
    fn modality_is_omitted_when_absent_or_empty() {
        let mut out = Vec::new();
        write_workitem_json(&mut out, &sample_workitem());
        let text = std::str::from_utf8(&out).unwrap();
        assert!(!text.contains("00080060"), "absent modality: {text}");

        let mut wi = sample_workitem();
        wi.modality = Some(String::new());
        let mut out = Vec::new();
        write_workitem_json(&mut out, &wi);
        let text = std::str::from_utf8(&out).unwrap();
        assert!(!text.contains("00080060"), "empty modality: {text}");
    }

    // native tag extraction parity with the serde-Value era helpers.
    // `{tag}.Value[0]` -> string; `{tag}.Value[0].Alphabetic` -> PN group.
    #[test]
    fn extract_tag_and_pn_from_native_dom() {
        use crate::handlers::json_common::parse_json;
        let body = br#"{"00100010":{"Value":[{"Alphabetic":"SMIRNOV^IVAN","Ideographic":"\u30a4\u30ef\u30ce\u30d5"}],"vr":"PN"},"00080060":{"Value":["CT"],"vr":"CS"},"00100020":{"Value":["PT-1"],"vr":"LO"}}"#;
        let obj = parse_json(body).unwrap();

        // Plain string tag via Value[0].
        assert_eq!(extract_tag(&obj, "00080060"), Some("CT".to_string()));
        assert_eq!(extract_tag(&obj, "00100020"), Some("PT-1".to_string()));
        // PN tag: every component group present is returned, `=`-joined
        // (PS3.5 6.2). Returning only Alphabetic dropped the ideographic
        // spelling the writer had stored.
        assert_eq!(
            extract_pn(&obj, "00100010"),
            Some("SMIRNOV^IVAN=\u{30a4}\u{30ef}\u{30ce}\u{30d5}".to_string())
        );
        // Missing tag -> None.
        assert_eq!(extract_tag(&obj, "00080050"), None);
        assert_eq!(extract_pn(&obj, "00080050"), None);
    }

    #[test]
    fn extract_pn_falls_back_to_plain_string() {
        use crate::handlers::json_common::parse_json;
        // PN tag whose Value[0] is a plain string (no object groups).
        let obj = parse_json(br#"{"00100010":{"Value":["SMIRNOV^IVAN"],"vr":"PN"}}"#).unwrap();
        assert_eq!(
            extract_pn(&obj, "00100010"),
            Some("SMIRNOV^IVAN".to_string())
        );
    }

    /// All three PN component groups survive the round trip.
    ///
    /// `push_pn_object` writes Alphabetic/Ideographic/Phonetic, so a reader
    /// that returned only Alphabetic silently dropped the native-script
    /// spelling of every name that had one. PS3.5 6.2 joins the groups with
    /// `=`, and an absent group is an empty one - never a trailing `=`.
    #[test]
    fn a_person_name_round_trips_through_all_three_component_groups() {
        use crate::handlers::json_common::parse_json;

        let mut wi = sample_workitem();
        // Escaped so this source file stays ASCII: SMIRNOV^IVAN in Cyrillic.
        wi.patient_name =
            Some("SMIRNOV^IVAN=\u{0421}\u{041c}\u{0418}\u{0420}\u{041d}\u{041e}\u{0412}^\u{0418}\u{0412}\u{0410}\u{041d}".to_string());
        let mut out = Vec::new();
        write_workitem_json(&mut out, &wi);
        let obj = parse_json(&out).expect("the rendered JSON parses");
        assert_eq!(
            extract_pn(&obj, "00100010"),
            wi.patient_name,
            "the stored value is the inverse of what was written"
        );

        // Alphabetic only: the reader must not invent a trailing group.
        let only_alpha =
            parse_json(br#"{"00100010":{"vr":"PN","Value":[{"Alphabetic":"DOE^JOHN"}]}}"#)
                .expect("parses");
        assert_eq!(
            extract_pn(&only_alpha, "00100010"),
            Some("DOE^JOHN".to_string()),
            "no trailing '=' for an absent group"
        );

        // Alphabetic + Phonetic (no Ideographic): the empty middle group keeps
        // its slot, because `=x=y` and `=x` are different names.
        let no_ideo = parse_json(
            br#"{"00100010":{"vr":"PN","Value":[{"Alphabetic":"A^B","Phonetic":"P^Q"}]}}"#,
        )
        .expect("parses");
        assert_eq!(
            extract_pn(&no_ideo, "00100010"),
            Some("A^B==P^Q".to_string())
        );
    }

    /// A PN object carrying only `Ideographic` is a legal name, not an error.
    ///
    /// `push_pn_object` always emits `Alphabetic`, so this shape can only come
    /// from a peer - and PS3.5 6.2 makes a missing group an EMPTY one. The
    /// leading `=` is positional: it marks the alphabetic group as empty.
    #[test]
    fn pn_with_only_ideographic_is_not_dropped() {
        use crate::handlers::json_common::parse_json;
        let obj =
            parse_json(br#"{"00100010":{"Value":[{"Ideographic":"\u5f20\u4e09"}],"vr":"PN"}}"#)
                .expect("parses");
        assert_eq!(
            extract_pn(&obj, "00100010"),
            Some("=\u{5f20}\u{4e09}".to_string())
        );
    }

    /// An object holding no component group at all is not a name.
    #[test]
    fn pn_with_all_groups_empty_is_none() {
        use crate::handlers::json_common::parse_json;
        let obj =
            parse_json(br#"{"00100010":{"Value":[{"Alphabetic":""}],"vr":"PN"}}"#).expect("parses");
        assert_eq!(extract_pn(&obj, "00100010"), None);
    }

    /// An empty alphabetic with a phonetic group keeps the ideographic slot.
    ///
    /// `==P` is three groups; collapsing it to `=P` would move the phonetic
    /// value into the ideographic group.
    #[test]
    fn pn_with_empty_alphabetic_and_phonetic_keeps_three_groups() {
        use crate::handlers::json_common::parse_json;
        let obj =
            parse_json(br#"{"00100010":{"Value":[{"Alphabetic":"","Phonetic":"P"}],"vr":"PN"}}"#)
                .expect("parses");
        assert_eq!(extract_pn(&obj, "00100010"), Some("==P".to_string()));
    }

    /// Writer -> reader is the identity on a three-group name.
    #[test]
    fn pn_round_trip_all_three() {
        use crate::handlers::json_common::parse_json;

        // \uXXXX escapes keep this source file ASCII.
        let raw = "A=\u{0418}\u{041e}=P";
        let mut out = Vec::new();
        out.extend_from_slice(b"{\"00100010\":{\"vr\":\"PN\",\"Value\":");
        push_pn_object(&mut out, raw);
        out.extend_from_slice(b"}}");
        let obj = parse_json(&out).expect("the rendered JSON parses");
        assert_eq!(extract_pn(&obj, "00100010"), Some(raw.to_string()));
    }

    /// F2: a stored full (0040,4021) is emitted VERBATIM, so the nested
    /// Referenced SOP Sequence the requester sent survives, and the typed
    /// projection is not emitted beside it.
    #[test]
    fn a_stored_input_information_is_emitted_verbatim() {
        let stored = r#"{"Value":[{"0040E020":{"vr":"CS","Value":["DICOM"]},"0020000D":{"vr":"UI","Value":["1.2.3.4"]},"00081199":{"vr":"SQ","Value":[{"00081150":{"vr":"UI","Value":["1.2.840.10008.5.1.4.1.1.2"]},"00081155":{"vr":"UI","Value":["1.2.3.4.5"]}}]}}],"vr":"SQ"}"#;
        let mut wi = sample_workitem();
        wi.input_information = Some(stored.to_string());
        let mut out = Vec::new();
        write_workitem_json(&mut out, &wi);
        let text = std::str::from_utf8(&out).unwrap();
        assert!(
            text.contains(stored),
            "stored bytes emitted verbatim: {text}"
        );
        assert_eq!(
            text.matches("00404021").count(),
            1,
            "exactly one input sequence: {text}"
        );
        assert!(
            text.contains("00081199"),
            "nested references survive: {text}"
        );
    }

    /// F2: a stored full (0074,1216) is emitted VERBATIM, including children
    /// the typed projection does not model ((0040,4029)).
    #[test]
    fn a_stored_performed_procedure_information_is_emitted_verbatim() {
        let stored = r#"{"Value":[{"00404050":{"vr":"DT","Value":["20261109T100000"]},"00404029":{"vr":"SQ","Value":[{"00080100":{"vr":"SH","Value":["STATION-CLASS"]},"00080102":{"vr":"SH","Value":["99CLARUS"]},"00080104":{"vr":"LO","Value":["station class"]}}]}}],"vr":"SQ"}"#;
        let mut wi = sample_workitem();
        wi.performed_procedure_information = Some(stored.to_string());
        let mut out = Vec::new();
        write_workitem_json(&mut out, &wi);
        let text = std::str::from_utf8(&out).unwrap();
        assert!(
            text.contains(stored),
            "stored bytes emitted verbatim: {text}"
        );
        assert!(
            text.contains("00404029"),
            "an unmodelled child survives: {text}"
        );
    }

    /// F4: (0074,1204) Procedure Step Label renders as LO (PS3.6), so a label
    /// longer than the SH ceiling of 16 characters is not truncated.
    #[test]
    fn procedure_step_label_renders_as_lo() {
        let mut wi = sample_workitem();
        wi.step_label = Some("a label longer than sixteen chars".to_string());
        let mut out = Vec::new();
        write_workitem_json(&mut out, &wi);
        let text = std::str::from_utf8(&out).unwrap();
        assert!(
            text.contains(
                "\"00741204\":{\"Value\":[\"a label longer than sixteen chars\"],\"vr\":\"LO\"}"
            ),
            "the label renders as LO, not SH: {text}"
        );
    }
}
