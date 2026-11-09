// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! UPS-RS pure synchronous core.
//!
//! Strangler-fig extraction for the UPS worklist. The pure side owns the full
//! CRUD decision logic - parse, state machine checks, JSON/XML rendering - and
//! decides *when* to publish an event. The transport (SSE receiver) is the
//! wrappers' job.
//!
//! Work items are stored in the file-backed CBOR store
//! (`clarus_core::ups::WorkItemStore`); the SQLite UPS tier is excised.
//! This module is fully synchronous: no `axum::`, no `async`, no `tokio` calls.
//!
//! # EventSink
//! "When to publish" is business logic: created -> only on a successful INSERT;
//! state change -> only on a real transition (equal transitions are muted by
//! the sink). The caller passes `&state.ups_events`. On the
//! migration an `MpscFanoutBus` implements the same trait - call sites need
//! zero changes.

#![forbid(unsafe_code)]

use clarus_ports::models::{OutputReference, WorkItem, WorkItemCode};

use crate::state::AppState;

use super::ups::event_bus::UpsEvent;
use super::ups::json::{extract_pn, extract_tag, write_workitem_json};
use super::ups::state_machine::{
    CHANGE_STATES, is_final_state, is_valid_state, is_valid_transition,
};
use super::ups::types::{UpsQuery, form_urlencode_query, parse_form_query};
use crate::handlers::json_common::{JsonVal, err_json, err_msg_json, parse_json};

// -- EventSink -----------------------------------------------------------

/// Abstraction for publishing UPS workitem events.
///
/// The pure CRUD logic publishes through this trait so the receiver can change
/// on the migration to an mpsc fan-out without touching any
/// call site. `UpsEventBus` is the current `Send + Sync` implementation.
pub trait EventSink: Send + Sync {
    /// Publish a raw workitem event to all subscribers.
    fn publish(&self, event: UpsEvent);
    /// Publish a creation event (`previous_state` empty, `new_state = SCHEDULED`).
    ///
    /// `filter` carries the workitem's subscription filter values (Procedure
    /// Step Label, Scheduled Workitem Code, the input study, the patient and
    /// the rest), which the channel filter matches on, PLUS the CURRENT
    /// (0040,4041) Input Readiness State the Event Report must carry (PS3.4
    /// CC.2.4.3). The caller passes what it just wrote, so no extra read.
    fn notify_created(&self, workitem_uid: &str, filter: super::ups::event_bus::FilterRouting);
    /// Publish a state-change event; mutes equal transitions.
    fn notify_state_change(
        &self,
        workitem_uid: &str,
        previous: &str,
        new: &str,
        filter: super::ups::event_bus::FilterRouting,
    );
}

impl EventSink for super::ups::event_bus::UpsEventBus {
    fn publish(&self, event: UpsEvent) {
        // `send` itself fills the monotonic id and timestamp.
        self.send(event);
    }

    fn notify_created(&self, workitem_uid: &str, filter: super::ups::event_bus::FilterRouting) {
        self.notify_created(workitem_uid, filter);
    }

    fn notify_state_change(
        &self,
        workitem_uid: &str,
        previous: &str,
        new: &str,
        filter: super::ups::event_bus::FilterRouting,
    ) {
        self.notify_state_change(workitem_uid, previous, new, filter);
    }
}

// -- Pure response types -------------------------------------------------

/// A fully-rendered UPS response: status, content type, extra headers, body.
/// The wrapper maps this onto the axum transport.
pub struct UpsResp {
    pub status: u16,
    pub content_type: &'static str,
    /// Extra headers (e.g. search truncation `Warning`). The wrappers copy
    /// them verbatim; this keeps the wire byte-for-byte.
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

/// A fully-rendered UPS error: status + `{"error":..., "message":...}` body bytes.
pub struct UpsErr {
    pub status: u16,
    pub body: Vec<u8>,
}

// -- UID validation (PS3.18 Section 11 - numeric dotted UID) --------------------

fn invalid_uid() -> UpsErr {
    UpsErr {
        status: 400,
        body: err_msg_json("Bad Request", "Invalid workitem UID"),
    }
}

fn is_valid_uid(uid: &str) -> bool {
    !uid.is_empty() && uid.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn internal_err(message: &str) -> UpsErr {
    UpsErr {
        status: 500,
        body: err_json(message),
    }
}

fn body_not_object() -> UpsErr {
    UpsErr {
        status: 400,
        body: err_msg_json(
            "Bad Request",
            "Body must be a DICOMweb JSON array of one dataset",
        ),
    }
}

/// The single dataset of a DICOM JSON request body (PS3.18 F.2).
///
/// F.2 defines the model as an ARRAY of datasets, so a conformant client sends
/// `[{...}]`; dcm4chee-arc and the older writers in this tree send the bare
/// object, so both shapes are accepted. Anything else - an empty array, an
/// array of several datasets, or a scalar - names no dataset and is refused
/// rather than silently treated as an empty body, which is what made a
/// conformant array reported as a missing attribute.
///
/// `pub(crate)` because the Storage Commitment request body has the same
/// single-dataset shape and must accept the same two spellings; one rule, one
/// function, so the two readers cannot drift.
pub(crate) fn single_dataset(parsed: &JsonVal) -> Option<&JsonVal> {
    match parsed {
        JsonVal::Obj(_) => Some(parsed),
        JsonVal::Arr(items) if items.len() == 1 => match &items[0] {
            JsonVal::Obj(_) => Some(&items[0]),
            _ => None,
        },
        _ => None,
    }
}

/// Negotiate the response content type from the Accept header (axum-free).
fn negotiate(accept: Option<&str>) -> &'static str {
    super::qido_pure::negotiate(accept)
}

// -- CBOR workitem store binding (C-014) ---------------------------

/// The file-backed workitem store rooted at `state.workitems_dir`.
///
/// The store is cheap to construct (just a `PathBuf`), so each pure handler
/// builds it from the already-configured `AppState.workitems_dir`. This keeps
/// the swap local to `ups_pure` - no `AppState` field change, so no test
/// fixture requires editing (ups_smoke stays green byte-identical).
pub(crate) fn workitem_store(state: &AppState) -> clarus_core::ups::WorkItemStore {
    clarus_core::ups::WorkItemStore::new(state.workitems_dir.clone())
}

/// A `YYYY-MM-DD HH:MM:SS` UTC timestamp (the CBOR store's
/// `created_at`/`updated_at` wire format).
pub(crate) fn now_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{year:04}-{month:02}-{day:02} {hh:02}:{mm:02}:{ss:02}")
}

/// Convert a proleptic-Gregorian day count (days since 1970-01-01) to
/// `(year, month, day)` - the civil-from-days algorithm shared by POSIX.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Ports `WorkItem` <-> core `WorkItemRecord` conversion (the boundary layer).
fn workitem_to_record(wi: &WorkItem) -> clarus_core::ups::WorkItemRecord {
    clarus_core::ups::WorkItemRecord {
        id: wi.id,
        workitem_uid: wi.workitem_uid.clone(),
        workflow_state: wi.workflow_state.clone(),
        modality: wi.modality.clone().unwrap_or_default(),
        station_aet: wi.station_aet.clone(),
        station_name: wi.station_name.clone(),
        procedure_step_start_date: wi.procedure_step_start_date.clone(),
        procedure_step_start_time: wi.procedure_step_start_time.clone(),
        patient_name: wi.patient_name.clone(),
        patient_id: wi.patient_id.clone(),
        accession_number: wi.accession_number.clone(),
        requesting_physician: wi.requesting_physician.clone(),
        sps_priority: wi.sps_priority.clone(),
        transaction_uid: wi.transaction_uid.clone(),
        created_at: wi.created_at.clone(),
        updated_at: wi.updated_at.clone(),
        step_label: wi.step_label.clone(),
        input_readiness: wi.input_readiness.clone(),
        sps_start_datetime: wi.sps_start_datetime.clone(),
        input_study_uid: wi.input_study_uid.clone(),
        reason_for_cancellation: wi.reason_for_cancellation.clone(),
        cancellation_datetime: wi.cancellation_datetime.clone(),
        patient_id_issuer: wi.patient_id_issuer.clone(),
        scheduled_workitem_code: wi.scheduled_workitem_code.as_ref().map(|code| {
            clarus_core::ups::WorkItemCode {
                value: code.value.clone(),
                scheme: code.scheme.clone(),
                meaning: code.meaning.clone(),
            }
        }),
        output_information: wi.output_information.as_ref().map(|outputs| {
            outputs
                .iter()
                .map(|out| clarus_core::ups::OutputReference {
                    sop_class_uid: out.sop_class_uid.clone(),
                    series_instance_uid: out.series_instance_uid.clone(),
                    sop_instance_uid: out.sop_instance_uid.clone(),
                })
                .collect()
        }),
        performed_workitem_code: wi.performed_workitem_code.as_ref().map(|code| {
            clarus_core::ups::WorkItemCode {
                value: code.value.clone(),
                scheme: code.scheme.clone(),
                meaning: code.meaning.clone(),
            }
        }),
        performed_station_name_code: wi.performed_station_name_code.as_ref().map(|code| {
            clarus_core::ups::WorkItemCode {
                value: code.value.clone(),
                scheme: code.scheme.clone(),
                meaning: code.meaning.clone(),
            }
        }),
        requested_procedure_id: wi.requested_procedure_id.clone(),
        performed_start_datetime: wi.performed_start_datetime.clone(),
        performed_end_datetime: wi.performed_end_datetime.clone(),
        scheduled_processing_parameters: wi.scheduled_processing_parameters.clone(),
        reason_for_requested_procedure: wi.reason_for_requested_procedure.clone(),
    }
}

fn record_to_workitem(rec: &clarus_core::ups::WorkItemRecord) -> WorkItem {
    WorkItem {
        id: rec.id,
        workitem_uid: rec.workitem_uid.clone(),
        workflow_state: rec.workflow_state.clone(),
        // The record stores an empty string for "no modality" (the field is
        // append-only, so it cannot become an Option on disk). At the boundary
        // it becomes `None` again, which is what lets the render omit
        // (0008,0060) instead of emitting a zero-length element.
        modality: (!rec.modality.is_empty()).then(|| rec.modality.clone()),
        station_aet: rec.station_aet.clone(),
        station_name: rec.station_name.clone(),
        procedure_step_start_date: rec.procedure_step_start_date.clone(),
        procedure_step_start_time: rec.procedure_step_start_time.clone(),
        patient_name: rec.patient_name.clone(),
        patient_id: rec.patient_id.clone(),
        accession_number: rec.accession_number.clone(),
        requesting_physician: rec.requesting_physician.clone(),
        sps_priority: rec.sps_priority.clone(),
        transaction_uid: rec.transaction_uid.clone(),
        created_at: rec.created_at.clone(),
        updated_at: rec.updated_at.clone(),
        step_label: rec.step_label.clone(),
        input_readiness: rec.input_readiness.clone(),
        sps_start_datetime: rec.sps_start_datetime.clone(),
        input_study_uid: rec.input_study_uid.clone(),
        reason_for_cancellation: rec.reason_for_cancellation.clone(),
        cancellation_datetime: rec.cancellation_datetime.clone(),
        patient_id_issuer: rec.patient_id_issuer.clone(),
        scheduled_workitem_code: rec
            .scheduled_workitem_code
            .as_ref()
            .map(|code| WorkItemCode {
                value: code.value.clone(),
                scheme: code.scheme.clone(),
                meaning: code.meaning.clone(),
            }),
        output_information: rec.output_information.as_ref().map(|outputs| {
            outputs
                .iter()
                .map(|out| OutputReference {
                    sop_class_uid: out.sop_class_uid.clone(),
                    series_instance_uid: out.series_instance_uid.clone(),
                    sop_instance_uid: out.sop_instance_uid.clone(),
                })
                .collect()
        }),
        performed_workitem_code: rec
            .performed_workitem_code
            .as_ref()
            .map(|code| WorkItemCode {
                value: code.value.clone(),
                scheme: code.scheme.clone(),
                meaning: code.meaning.clone(),
            }),
        performed_station_name_code: rec.performed_station_name_code.as_ref().map(|code| {
            WorkItemCode {
                value: code.value.clone(),
                scheme: code.scheme.clone(),
                meaning: code.meaning.clone(),
            }
        }),
        requested_procedure_id: rec.requested_procedure_id.clone(),
        performed_start_datetime: rec.performed_start_datetime.clone(),
        performed_end_datetime: rec.performed_end_datetime.clone(),
        scheduled_processing_parameters: rec.scheduled_processing_parameters.clone(),
        reason_for_requested_procedure: rec.reason_for_requested_procedure.clone(),
    }
}

/// Map a store error to the `UpsErr` used by the handlers.
fn store_err(e: clarus_core::ups::StoreError) -> UpsErr {
    crate::clog_error!(error = %e, "UPS workitem store error");
    internal_err("Internal server error")
}

// -- Create (PS3.18 Section 11.5) -----------------------------------------------

/// `POST /dicomweb/workitems/{uid}` - UPS Create.
///
/// Validates the workitem UID, rejects duplicates (409), checks the type-1
/// attribute set of PS3.4 Table CC.2.5-3, forces the SCHEDULED state, inserts
/// through the CBOR workitem store (`clarus_core::ups::WorkItemStore`),
/// publishes a `created` event on success and returns 201 with the workitem
/// JSON.
///
/// `base_url` is `{scheme}://{host}` (the same value the QIDO RetrieveURL is
/// built from). PS3.18 11.4.2 requires the SCP to return the new workitem's
/// URL "in the Location Header Field", and 11.4.3.2.1 lists Location in the
/// success response, so a create that names a resource must say where it
/// lives. `None` (no usable Host) omits the header rather than inventing one.
pub fn ups_create(
    state: &AppState,
    sink: &dyn EventSink,
    workitem_uid: &str,
    body: &[u8],
    accept: Option<&str>,
    base_url: Option<&str>,
) -> Result<UpsResp, UpsErr> {
    let _content_type = negotiate(accept);

    // Validate UID format
    if !is_valid_uid(workitem_uid) {
        return Err(invalid_uid());
    }

    // Parse body (native DOM;:from_slice).
    let parsed = parse_json(body).ok_or_else(body_not_object)?;
    let body_obj = single_dataset(&parsed).ok_or_else(body_not_object)?;

    // PS3.4 Table CC.2.5-3: SOP Common attributes are not part of the
    // N-CREATE dataset. SOP Instance UID (0008,0018) is conveyed by the
    // `workitem` query parameter (the Affected SOP Instance UID); SOP Class
    // UID (0008,0016) is set by the SCP (UPS Push, hardcoded at render). A
    // payload carrying either is non-conformant, so it is refused instead of
    // silently ignored - the strict SCP reading dcm4chee-arc also applies.
    let mut forbidden = Vec::new();
    for tag in ["00080016", "00080018"] {
        if body_obj.obj_get(tag).is_some() {
            forbidden.push(tag);
        }
    }
    if !forbidden.is_empty() {
        return Err(UpsErr {
            status: 400,
            body: err_msg_json(
                "Bad Request",
                &format!(
                    "Not allowed attribute(s) in Create body: {}; the Workitem UID \
                     is the `workitem` query parameter and the SOP Class UID is set by the server",
                    forbidden.join(", ")
                ),
            ),
        });
    }

    // Check for duplicate (CBOR store is the UPS source of truth).
    let store = workitem_store(state);
    match store.find_by_uid(workitem_uid) {
        Ok(Some(_)) => {
            return Err(UpsErr {
                status: 409,
                body: err_msg_json(
                    "Conflict",
                    &format!("Workitem '{}' already exists", workitem_uid),
                ),
            });
        }
        Ok(None) => {}
        Err(e) => return Err(store_err(e)),
    }

    // Required tags: the type-1 set of PS3.4 Table CC.2.5-3 at N-CREATE.
    // Modality is deliberately absent - UPS has no Modality attribute in
    // 2026c, so demanding one made every conformant client fail.
    let (step_label, input_readiness, sps_start_datetime, sps_priority) =
        match required_create_tags(body_obj) {
            Ok(v) => v,
            Err(missing) => {
                return Err(UpsErr {
                    status: 400,
                    body: err_msg_json("Bad Request", &format!("Missing required tag: {missing}")),
                });
            }
        };

    // The state is SCHEDULED, always. Table CC.1.1-2 row 1: the instance is
    // created with an empty Transaction UID and the SCHEDULED state, so a
    // payload asking for anything else is refused rather than honoured -
    // honouring it would create a workitem born final, which no legal
    // transition can ever leave.
    if let Some(requested) = extract_tag(body_obj, "00741000")
        && requested != "SCHEDULED"
    {
        return Err(UpsErr {
            status: 400,
            body: err_msg_json(
                "Bad Request",
                &format!("Procedure Step State must be SCHEDULED at create, got '{requested}'"),
            ),
        });
    }

    // The input study, in the order the standard puts the two carriages:
    // Referenced Request Sequence (0040,A370 > 0020,000D) first - it is the
    // key the requester searches by (CC.2.5-3 makes Study Instance UID
    // optional but PRESENT in the sequence) - then the Input Information
    // Sequence (0040,4021 > 0020,000D), which is the route this server has
    // always read. The two name one fact, so the first that is present wins.
    let input_study_uid = extract_first_item_tag(body_obj, "0040A370", "0020000D")
        .or_else(|| extract_input_study_uid(body_obj));

    // Both are read BEFORE the record is built: either can refuse the request,
    // and a workitem must not be stored on a body the server rejected.
    let scheduled_processing_parameters = extract_scheduled_processing_parameters(body_obj)?;

    let workitem = WorkItem {
        id: None,
        workitem_uid: workitem_uid.to_string(),
        workflow_state: "SCHEDULED".to_string(),
        station_aet: station_slot(body_obj),
        station_name: extract_tag(body_obj, "00400010"),
        procedure_step_start_date: extract_tag(body_obj, "00400002"),
        procedure_step_start_time: extract_tag(body_obj, "00400003"),
        patient_name: extract_pn(body_obj, "00100010")
            .or_else(|| extract_tag(body_obj, "00100010")),
        patient_id: extract_tag(body_obj, "00100020"),
        // Accession Number: the standard carriage is inside the Referenced
        // Request Sequence (CC.2.5-3 marks it Required there), and the bare
        // (0008,0050) is what the server read before. The sequence wins when
        // both are present - it is the form the search key mirrors, so a
        // workitem created from it is findable by the query that named it.
        accession_number: extract_first_item_tag(body_obj, "0040A370", "00080050")
            .or_else(|| extract_tag(body_obj, "00080050")),
        // (0040,1001) Requested Procedure ID lives in the same sequence and is
        // a REQUIRED matching key there (PS3.4 Table CC.2.5-3), so it is read
        // from the sequence item only - there is no bare-tag fallback,
        // because the standard names no bare attribute for it.
        requested_procedure_id: extract_first_item_tag(body_obj, "0040A370", "00401001"),
        requesting_physician: extract_pn(body_obj, "00321032")
            .or_else(|| extract_tag(body_obj, "00321032")),
        // (0008,0060) is NOT read from the body. The UPS IOD (PS3.4 Table
        // CC.2.5-3) defines no Modality, so a client cannot state one, and the
        // server has no study to derive one from on this path. An externally
        // created workitem therefore carries no Modality, and the render omits
        // the element - the honest skip rather than a fabricated value.
        modality: None,
        sps_priority: Some(sps_priority),
        transaction_uid: None,
        created_at: None,
        updated_at: None,
        step_label: Some(step_label),
        input_readiness: Some(input_readiness),
        sps_start_datetime: Some(sps_start_datetime),
        input_study_uid,
        reason_for_cancellation: None,
        cancellation_datetime: None,
        // (0010,0021) and (0040,4018): stored as received, exactly like
        // every other attribute the server does not interpret. The zero
        // length convention of 0010,0021 survives the round trip because
        // `extract_tag` returns the empty string rather than None.
        patient_id_issuer: extract_tag(body_obj, "00100021"),
        scheduled_workitem_code: extract_scheduled_workitem_code_checked(body_obj, state)?,
        // The performed report and the output list are UPDATE-time
        // attributes (PS3.18 11.6). A create that carried them would be
        // stating results for work nobody has done, so they are not read
        // here at all - the workitem is born with neither.
        output_information: None,
        performed_workitem_code: None,
        performed_station_name_code: None,
        // (0040,4050)/(0040,4051) are the performer's own accounting of when
        // it ran: they arrive with the Update that reports the outcome, never
        // with the create, so the workitem is born knowing neither.
        performed_start_datetime: None,
        performed_end_datetime: None,
        // Both are filled right here, from the body read above: the
        // parameters element is stored opaquely and the reason is a nested
        // attribute of the Referenced Request Sequence.
        scheduled_processing_parameters,
        reason_for_requested_procedure: extract_first_item_tag(body_obj, "0040A370", "00401002"),
    };

    // Insert via the CBOR store (fresh timestamps, no Transaction UID).
    //
    // A workitem is created with no Transaction UID. PS3.18 11.6: the UID is
    // issued by whoever performs the SCHEDULED -> IN PROGRESS transition, not
    // by the create. Pre-issuing one here made every conformant claimant fail
    // the optimistic lock: the client sends its own UID, the stored one is
    // ours, and the mismatch is reported as a conflict on a workitem nobody
    // else has touched.
    let stamp = now_stamp();
    let mut record = workitem_to_record(&workitem);
    record.transaction_uid = None;
    record.created_at = Some(stamp.clone());
    record.updated_at = Some(stamp);

    match store.insert(&record) {
        Ok(()) => {
            // CMP-004: Notify SSE subscribers of workitem creation
            let routing = super::ups::event_bus::FilterRouting::from_record(&record);
            sink.notify_created(&workitem.workitem_uid, routing.clone());

            // PS3.18 11.10: notify Worklist Subscription subscribers. Runs on
            // the push worker thread - the request path only fills the queue.
            // The filter keys are the Procedure Step Label and the scheduled
            // code the requester wrote, so a code-filtered subscription is
            // answered from the record in hand.
            crate::push::push_to_subscribers_with_values(
                &workitem.workitem_uid,
                &crate::push::FilterValues::from_record(&record),
            );

            let inserted = store.find_by_uid(&workitem.workitem_uid).ok().flatten();

            let response_wi = match inserted {
                Some(ref rec) => record_to_workitem(rec),
                None => workitem,
            };
            // PS3.18 11.4.2 / 11.4.3.2.1: the create response carries the
            // created resource's URL. Added here and not in
            // `render_workitem_response`, which is shared with Update and Get
            // - neither of those defines a Location.
            let mut resp = render_workitem_response(201, &response_wi, accept)?;
            if let Some(base) = base_url {
                resp.headers.push((
                    "Location",
                    format!("{base}/dicomweb/workitems/{workitem_uid}"),
                ));
            }
            Ok(resp)
        }
        // Two creates of the same UID racing: the pre-insert lookup above sees
        // nothing for both, and the store rejects the loser at the atomic
        // insert. That is the same fact as the lookup finding an existing
        // workitem, so it gets the same answer - 409 Conflict - instead of
        // surfacing a storage-level CAS failure as 500.
        Err(clarus_core::ups::StoreError::CasConflict(_)) => Err(UpsErr {
            status: 409,
            body: err_msg_json(
                "Conflict",
                &format!("Workitem '{workitem_uid}' already exists"),
            ),
        }),
        Err(e) => Err(store_err(e)),
    }
}

// -- Update (PS3.18 Section 11.6) -----------------------------------------------

/// `PUT /dicomweb/workitems/{uid}` - UPS Update.
///
/// Validates the state machine (409 on invalid transition), optimistic-lock
/// on Transaction UID, updates through the CBOR workitem store
/// (`clarus_core::ups::WorkItemStore`), publishes a state change event only
/// when the workflow state actually changed, and returns 200.
pub fn ups_update(
    state: &AppState,
    sink: &dyn EventSink,
    workitem_uid: &str,
    body: &[u8],
    accept: Option<&str>,
) -> Result<UpsResp, UpsErr> {
    let _content_type = negotiate(accept);

    if !is_valid_uid(workitem_uid) {
        return Err(invalid_uid());
    }

    // Find existing workitem (CBOR store).
    let store = workitem_store(state);
    let existing = match store.find_by_uid(workitem_uid) {
        Ok(Some(rec)) => record_to_workitem(&rec),
        Ok(None) => {
            return Err(UpsErr {
                status: 404,
                body: err_msg_json(
                    "Not Found",
                    &format!("Workitem '{}' not found", workitem_uid),
                ),
            });
        }
        Err(e) => return Err(store_err(e)),
    };

    let parsed = parse_json(body).ok_or_else(body_not_object)?;
    let body_obj = single_dataset(&parsed).ok_or_else(body_not_object)?;

    // PS3.18 11.7.1.4: a Change UPS State payload carries the Procedure Step
    // State and the Transaction UID, and nothing else is read. The state is
    // required; the set of legal values is IN PROGRESS, COMPLETED and
    // CANCELED (SCHEDULED is only ever the created state - Table CC.1.1-2
    // answers C303H to it on every column).
    let Some(new_state) = extract_tag(body_obj, "00741000") else {
        return Err(bad_request(
            "Missing required tag: (0074,1000) Procedure Step State",
        ));
    };
    if !CHANGE_STATES.contains(&new_state.as_str()) {
        return Err(bad_request(&format!(
            "Procedure Step State must be one of {CHANGE_STATES:?}, got '{new_state}'"
        )));
    }

    // CC.2.1-1: the Transaction UID is type 1 in every state change. A
    // request that omits it is refused here rather than further down, because
    // "missing" and "wrong" have the same answer (C301) and the same code.
    let Some(request_transaction_uid) = extract_tag(body_obj, "00081195") else {
        return Err(bad_request(
            "Missing required tag: (0008,1195) Transaction UID",
        ));
    };

    // Table CC.1.1-2, the two rows that depend on who is asking. A workitem
    // with no stored UID can only be taken to IN PROGRESS - that is the claim
    // that installs the lock. With a stored UID, the request must present it.
    match existing.transaction_uid.as_deref() {
        None => {
            if new_state != "IN PROGRESS" {
                return Err(bad_request(&format!(
                    "Workitem is not claimed: state may only be set to IN PROGRESS, \
                     not '{new_state}'"
                )));
            }
        }
        Some(stored) if stored != request_transaction_uid => {
            return Err(bad_request(&format!(
                "Transaction UID mismatch: expected '{stored}', got '{request_transaction_uid}'"
            )));
        }
        Some(_) => {}
    }

    // The transition table itself. Equal terminal transitions are successes
    // (B306H/B304H) and stay silent; every other refusal is 400, because
    // 11.7.3-1 reserves 409 for a concurrent modification.
    if !is_valid_transition(&existing.workflow_state, &new_state) {
        return Err(bad_request(&format!(
            "Invalid state transition: '{}' -> '{}'",
            existing.workflow_state, new_state
        )));
    }

    // PS3.4 CC.2.4 obliges the SCP to refuse a claim while the input is
    // INCOMPLETE, and this server IS the SCP. The readiness debounce exists so
    // a performer cannot work on a partially stored study; enforcing it only
    // in a client that happens to check leaves every other client free to
    // claim an unfinished CAD workitem and produce results over part of a
    // study - results nobody downstream can tell apart from complete ones.
    if new_state == "IN PROGRESS"
        && existing.input_readiness.as_deref() == Some(super::readiness::READINESS_INCOMPLETE)
    {
        return Err(bad_request(
            "Input Readiness State (0040,4041) is INCOMPLETE: the workitem cannot be claimed yet",
        ));
    }

    let state_changed = new_state != existing.workflow_state;
    let previous_state = existing.workflow_state.clone();

    // The lock the workitem holds after this request. A claim installs the
    // caller's UID; later transitions keep the stored one. It is never
    // reissued, or the claimant's lock would be void on the next request.
    let stored_tuid = existing
        .transaction_uid
        .clone()
        .or_else(|| Some(request_transaction_uid.clone()));
    // The CAS precondition is what is on disk right now, before the write.
    let cas_tuid = existing.transaction_uid.clone();

    let stamp = now_stamp();

    // A state change touches the state and the two attributes the standard
    // assigns to the transition; the rest of the record is carried over
    // untouched. Merging arbitrary attributes here is what 11.7.1.4 does not
    // allow, and it is what made a stray tag silently rewrite the workitem.
    //
    // CANCELED -> CANCELED is a SUCCESS (Table CC.1.1-2 answers B304H, a
    // warning that reports the state the workitem is already in), so it walks
    // the same path as a real transition. It is not one: the cancelling
    // instant (0040,4052) belongs to the cancellation that happened, and
    // stamping it again would rewrite history to the moment of a redundant
    // request. Only a workitem that was not already cancelled gets a new
    // stamp.
    let cancellation_now = new_state == "CANCELED" && existing.workflow_state != "CANCELED";
    let updated_workitem = WorkItem {
        workflow_state: new_state,
        transaction_uid: stored_tuid.clone(),
        reason_for_cancellation: if cancellation_now {
            extract_tag(body_obj, "00741238").or(existing.reason_for_cancellation)
        } else {
            existing.reason_for_cancellation
        },
        cancellation_datetime: if cancellation_now {
            Some(stamp.clone())
        } else {
            existing.cancellation_datetime
        },
        updated_at: None,
        ..existing
    };

    // CAS update on the CBOR store. The precondition is the Transaction UID
    // the caller already validated, so a workitem changed underneath this
    // request is reported as a conflict rather than silently overwritten.
    let mut record = workitem_to_record(&updated_workitem);
    record.transaction_uid = stored_tuid;
    record.updated_at = Some(stamp);

    match store.cas_update(&record, cas_tuid.as_deref()) {
        Ok(()) => {
            // CMP-004: Notify SSE subscribers of state change
            if state_changed {
                // The filter keys travel with the transition for the same
                // reason they do on the create path: the WebSocket channel
                // asks the subscription filter, and a subscription narrowed by
                // Procedure Step Label or Scheduled Workitem Code must be
                // answered from the record in hand.
                sink.notify_state_change(
                    workitem_uid,
                    &previous_state,
                    &updated_workitem.workflow_state,
                    super::ups::event_bus::FilterRouting::from_record(&record),
                );
            }

            render_workitem_response(200, &updated_workitem, accept)
        }
        Err(clarus_core::ups::StoreError::CasConflict(_)) => Err(UpsErr {
            status: 409,
            body: err_msg_json(
                "Conflict",
                "Transaction UID mismatch: workitem changed concurrently",
            ),
        }),
        Err(e) => Err(store_err(e)),
    }
}

// -- Request Cancellation (PS3.18 Section 11.8) --------------------------

/// `POST /dicomweb/workitems/{uid}/cancelrequest?requester={aetitle}` -
/// Request Cancellation.
///
/// PS3.18 11.8 lets a user agent that does NOT own a workitem ask for it to
/// be canceled; an owner cancels through Change State (11.7) instead. This is
/// a PRODUCT FEATURE, not an AIW-I obligation: AIW-I Table 50.1-1 requires
/// RAD-80..RAD-87 and RAD-109, and Request Cancellation has no RAD number. It
/// exists because the Task Requester (Weasis) has a "cancel queued" action
/// that sends exactly this request for a workitem it does not own.
///
/// # Behavior (CC.2.2.3)
///
/// - SCHEDULED: the SCP changes the state to CANCELED, meeting the Final State
///   requirements (Reason for Cancellation when the payload gives one, and the
///   Procedure Step Cancellation DateTime). Answered `202 Accepted` - the
///   request was accepted for processing, which is what 11.8's success status
///   means; the state change is the SCP's own act, not an echo of the request.
/// - COMPLETED or CANCELED: a terminal workitem cannot be canceled, so `409`.
///   A repeated cancel on CANCELED is the same refusal: the state is terminal,
///   and 11.8 explicitly lists "Workitem in the COMPLETED state" as a conflict.
/// - IN PROGRESS: CC.2.2.3 has the SCP inform the performing SCU with a "UPS
///   Cancel Requested" N-EVENT-REPORT. This server is the Task Manager, not
///   the performer, and the cancel-event fan-out is not implemented in this
///   release, so the honest answer is `409` - the same status the standard
///   names for "no possibility the performer will be informed". Silently
///   accepting would promise a cancellation nobody performs.
/// - Not found: `404`.
///
/// `requester` is optional (Table 11.7.2.1-1) and is not stored: it names who
/// asked, and the standard gives it no attribute to live in. `None` is legal
/// (an anonymous requester is not a conflict), which is why an absent
/// parameter is not an error.
///
/// Not ap101-gated: a Request Cancellation is a rare control-plane write (a
/// queued workitem, once), not a per-instance or per-search hot path, so there
/// is no allocation budget to defend here.
pub fn ups_request_cancel(
    state: &AppState,
    sink: &dyn EventSink,
    workitem_uid: &str,
    body: &[u8],
    _requester: Option<&str>,
    accept: Option<&str>,
) -> Result<UpsResp, UpsErr> {
    let _content_type = negotiate(accept);

    if !is_valid_uid(workitem_uid) {
        return Err(invalid_uid());
    }

    let store = workitem_store(state);
    let existing = match store.find_by_uid(workitem_uid) {
        Ok(Some(rec)) => record_to_workitem(&rec),
        Ok(None) => {
            return Err(UpsErr {
                status: 404,
                body: err_msg_json(
                    "Not Found",
                    &format!("Workitem '{}' not found", workitem_uid),
                ),
            });
        }
        Err(e) => return Err(store_err(e)),
    };

    // The state decides everything, and it is read BEFORE the payload is even
    // parsed: a terminal workitem is a conflict whatever the request says.
    match existing.workflow_state.as_str() {
        "SCHEDULED" => {}
        "COMPLETED" | "CANCELED" => {
            return Err(UpsErr {
                status: 409,
                body: err_msg_json(
                    "Conflict",
                    &format!(
                        "Workitem is in the terminal state '{}' and cannot be canceled",
                        existing.workflow_state
                    ),
                ),
            });
        }
        "IN PROGRESS" => {
            return Err(UpsErr {
                status: 409,
                body: err_msg_json(
                    "Conflict",
                    "Workitem is IN PROGRESS and owned by its performer: this origin \
                     server is the Task Manager, not the performer, and cannot inform \
                     the owner in this release",
                ),
            });
        }
        other => {
            return Err(bad_request(&format!(
                "Workitem is in an unknown state '{other}'"
            )));
        }
    }

    // The payload is OPTIONAL (11.8 lists reason / contact display name /
    // contact URI). Only the reason has an attribute to live in, so only it is
    // read; an absent or empty body is a legal cancel with no stated reason.
    // A body that is not an object is refused rather than ignored, the same
    // rule every other UPS payload follows.
    let reason = if body.is_empty() {
        None
    } else {
        let parsed = parse_json(body).ok_or_else(body_not_object)?;
        let body_obj = single_dataset(&parsed).ok_or_else(body_not_object)?;
        extract_tag(body_obj, "00741238")
    };

    let stamp = now_stamp();
    let previous_state = existing.workflow_state.clone();
    let updated = WorkItem {
        workflow_state: "CANCELED".to_string(),
        reason_for_cancellation: reason.or(existing.reason_for_cancellation),
        // (0040,4052) is a Final State requirement (CC.2.2.3): the moment the
        // cancellation happened is part of the state, not of the request.
        cancellation_datetime: Some(stamp.clone()),
        updated_at: None,
        ..existing
    };

    let mut record = workitem_to_record(&updated);
    record.updated_at = Some(stamp);

    // NO transaction-UID precondition: a SCHEDULED workitem has no lock (only
    // the claim issues one), so `None` is the honest expectation. The store's
    // CAS still refuses a concurrent write on the UID itself.
    match store.cas_update(&record, None) {
        Ok(()) => {
            sink.notify_state_change(
                workitem_uid,
                &previous_state,
                &updated.workflow_state,
                super::ups::event_bus::FilterRouting::from_record(&record),
            );
            // 11.8's success status, with the updated workitem as the body so
            // a requester sees what it asked for.
            render_workitem_response(202, &updated, accept)
        }
        Err(clarus_core::ups::StoreError::CasConflict(_)) => Err(UpsErr {
            status: 409,
            body: err_msg_json(
                "Conflict",
                "Workitem changed while the cancel request was being processed",
            ),
        }),
        Err(e) => Err(store_err(e)),
    }
}

// -- Update (PS3.18 Section 11.6) ----------------------------------------

/// `POST /dicomweb/workitems/{uid}?{TransactionUid}` - UPS Update.
///
/// PS3.18 2026c 11.3 Table 11.3-1 and 11.6.1: the Update transaction is a
/// **POST** on the workitem resource with the Transaction UID as a **query
/// parameter**, and its payload carries the attributes to modify. It
/// corresponds to the UPS DIMSE N-SET operation - a different operation from
/// Change State (11.7, `PUT .../state`), which is what this server used to
/// serve on every workitem write.
///
/// # Why a POST and how it is told apart from Create
///
/// Create (11.4.1.1) targets `/workitems?{workitem}` - the UID is a QUERY
/// parameter and there is no path segment after `workitems`. Update targets
/// `/workitems/{workitem}` - the UID is a PATH segment and the Transaction
/// UID is the query parameter. The two therefore never share a request shape:
/// a path segment means update, its absence means create. That is the
/// standard's own distinction and it is what the router implements, so no
/// payload sniffing is involved.
///
/// # Merge set
///
/// AIW-I 4.84.4.1.2.1 obliges the Task Performer to report, on the update
/// that precedes the completed transition, the objects it created and the
/// codes describing itself. PS3.3 Table C.30.3-1 puts every one of those
/// attributes inside ONE item of (0074,1216) Unified Procedure Step Performed
/// Procedure Sequence, so that item is the entire merge set:
///
/// - (0040,4033) Output Information Sequence, child of the item - replaced whole;
/// - (0040,4019) Performed Workitem Code Sequence, child of the item - replaced whole;
/// - (0040,4028) Performed Station Name Code Sequence, child of the item - replaced whole;
/// - (0040,4050)/(0040,4051) start and end datetimes, children of the item.
///
/// The former top-level spellings of the three sequences are NOT accepted as
/// an alias: the flat form is not the standard's carriage, a conformant client
/// never sends it, and keeping it would mean this server stores the same fact
/// in two shapes that a reader of one would miss in the other.
///
/// They are the ONLY attributes this handler writes. PS3.18 11.6.1.4 lets a
/// request carry any element to modify and defers to PS3.4 CC.2.6.2, but this
/// server claims a specific, minimal update set instead of a general
/// attribute-merge: a general merge would have to re-implement the UPS
/// attribute conformance table, and every attribute it got wrong would be a
/// silent corruption of a record a performer is holding. An element outside
/// the set is therefore REJECTED with 400 rather than ignored, so a client
/// that meant something this server does not implement finds out at once
/// instead of believing its update landed.
///
/// # State
///
/// The workflow state is not touched and no event is published: 11.6 updates
/// attributes and 11.7 changes state, and a workitem whose state moved
/// because of an N-SET-shaped request would be one the standard cannot
/// describe.
///
/// # Locking
///
/// The Transaction UID is type 1 here as it is in Change State, and it is
/// compared against the stored one: a mismatch is 400 (PS3.4 CC.2.6.3), and a
/// concurrent write between the read and the CAS write is 409.
pub fn ups_update_attributes(
    state: &AppState,
    workitem_uid: &str,
    body: &[u8],
    transaction_uid_query: Option<&str>,
    accept: Option<&str>,
) -> Result<UpsResp, UpsErr> {
    let _content_type = negotiate(accept);

    if !is_valid_uid(workitem_uid) {
        return Err(invalid_uid());
    }

    let store = workitem_store(state);
    let existing = match store.find_by_uid(workitem_uid) {
        Ok(Some(rec)) => record_to_workitem(&rec),
        Ok(None) => {
            return Err(UpsErr {
                status: 404,
                body: err_msg_json(
                    "Not Found",
                    &format!("Workitem '{}' not found", workitem_uid),
                ),
            });
        }
        Err(e) => return Err(store_err(e)),
    };

    let parsed = parse_json(body).ok_or_else(body_not_object)?;
    let body_obj = single_dataset(&parsed).ok_or_else(body_not_object)?;

    // The Transaction UID arrives in one of TWO standard-sanctioned places:
    //
    // - CC.2.6.2 (N-SET data set): for an IN PROGRESS workitem the SCU SHALL
    //   provide (0008,1195) as an Attribute, which is exactly what the
    //   DICOMweb payload is - the Data Set 11.6.1.4 requires to comply with
    //   CC.2.6.2. dcm4chee-arc reads it ONLY here.
    // - PS3.18 11.6.1.2: the Transaction UID Query Parameter.
    //
    // Both name one fact, so the server accepts either. When a request
    // carries BOTH they must agree: two different locking values in one
    // request is a contradiction, and answering 200 on a guess would let a
    // caller believe a lock it did not present.
    let body_transaction_uid = extract_tag(body_obj, "00081195");
    let request_transaction_uid = match (body_transaction_uid.as_deref(), transaction_uid_query) {
        (Some(body), Some(query)) if body != query => {
            return Err(bad_request(&format!(
                "Transaction UID present in both the payload ('{body}') and the query \
                 ('{query}') with different values"
            )));
        }
        (Some(body), _) => body,
        (None, Some(query)) => query,
        (None, None) => {
            return Err(bad_request(
                "Missing required Transaction UID (0008,1195): send it as a payload \
                 attribute or the TransactionUid query parameter",
            ));
        }
    };

    // The stored UID is what the caller must present. An unclaimed workitem
    // has none, and 11.6 cannot install one: only the SCHEDULED -> IN
    // PROGRESS transition issues the lock (11.6.3.2 makes that an N-SET
    // failure for a workitem with no Transaction UID), so an update of an
    // unclaimed workitem is refused rather than performed unlocked.
    match existing.transaction_uid.as_deref() {
        Some(stored) if stored == request_transaction_uid => {}
        Some(stored) => {
            return Err(bad_request(&format!(
                "Transaction UID mismatch: expected '{stored}', got '{request_transaction_uid}'"
            )));
        }
        None => {
            return Err(bad_request(
                "Workitem has no Transaction UID: it must be claimed before it can be updated",
            ));
        }
    }

    // A workitem in a Final State cannot be updated. PS3.18 11.6.2 states it
    // in one sentence and then again in its status table: "If the Workitem is
    // in the COMPLETED or CANCELED state, the response shall be a 400 (Bad
    // Request) failure response", and 400 covers "the Target Workitem was in
    // the COMPLETED or CANCELED state". The transaction is the UPS DIMSE
    // N-SET, and PS3.4 CC.2.6.2 admits no N-SET once the performed procedure
    // step is final - the record is closed and the objects it produced are
    // attested by the completion report, not by later edits.
    //
    // The status is 400 and NOT 409: 11.6.2 reserves 409 for "the request is
    // inconsistent with the current state of the Target Workitem", which is a
    // different failure (a present-but-contradictory request), and borrowing
    // the sibling 11.8 Cancel status here would report the wrong reason.
    if is_final_state(&existing.workflow_state) {
        return Err(UpsErr {
            status: 400,
            body: err_msg_json(
                "Bad Request",
                &format!(
                    "Workitem is in the terminal state '{}' and cannot be updated",
                    existing.workflow_state
                ),
            ),
        });
    }

    // Everything the request carries has to be in the merge set, EXCEPT the
    // Transaction UID: it is the locking Attribute (CC.2.6.2), not a value
    // this transaction merges, so a payload carrying it is a standard-shaped
    // request rather than an unknown element. It was consumed above.
    for key in body_obj.keys() {
        if key == "00081195" {
            continue;
        }
        if !UPDATE_MERGE_SET.contains(&key) {
            return Err(bad_request(&format!(
                "Attribute '{key}' is not updatable by this implementation; \
                 the Update transaction writes {UPDATE_MERGE_SET:?} and the locking \
                 Transaction UID (0008,1195)"
            )));
        }
    }

    // A payload naming no updatable attribute is an empty update: it says
    // nothing, so it is refused rather than answered 200. A payload that
    // carries ONLY the Transaction UID is the same statement - the UID is the
    // lock, not an attribute this transaction merges.
    let has_mergeable = body_obj
        .keys()
        .iter()
        .any(|key| UPDATE_MERGE_SET.contains(key));
    if !has_mergeable {
        return Err(bad_request(
            "Update payload carries no attributes to modify",
        ));
    }

    let mut updated = existing.clone();

    // (0074,1204) Procedure Step Label and (0074,1200) Scheduled Procedure
    // Step Priority: the two attributes a requester retunes while the step is
    // still queued. Both are read before the performed report so a payload
    // carrying an invalid priority fails before anything is written.
    if let Some(label) = body_obj.obj_get("00741204") {
        updated.step_label = Some(extract_required_label(label, "00741204")?);
    }
    if let Some(priority) = body_obj.obj_get("00741200") {
        updated.sps_priority = Some(extract_required_priority(priority, "00741200")?);
    }

    // (0040,4018) Scheduled Workitem Code Sequence: Table CC.2.5-3 types it
    // 3/1 at N-SET (the SCP SHALL handle its update), so it is read here with
    // the SAME coding-scheme check the create path applies. A code whose
    // scheme nobody declared cannot be matched by any performer, so storing
    // it would hand out a workitem no rule can match - the silent acceptance
    // this server refuses everywhere else.
    if let Some(code) = extract_scheduled_workitem_code_checked(body_obj, state)? {
        updated.scheduled_workitem_code = Some(code);
    }

    // (0074,1210) Scheduled Processing Parameters Sequence: replaced WHOLE,
    // because it is the performing system's parameter list and a partial
    // merge would leave a mix no requester composed. An empty `Value` array
    // stores an empty sequence - "this workitem now carries no parameters" -
    // which Table CC.2.5-3 admits at N-SET (type 2, and a type 2 attribute
    // may be sent with zero length to say exactly that).
    if body_obj.obj_get("00741210").is_some() {
        updated.scheduled_processing_parameters =
            extract_scheduled_processing_parameters(body_obj)?;
    }

    // (0040,A370) Referenced Request Sequence: only (0040,1002) Reason for the
    // Requested Procedure is updatable; see
    // [`extract_referenced_request_for_update`].
    if let Some(reference) = body_obj.obj_get("0040A370") {
        updated.reason_for_requested_procedure =
            Some(extract_referenced_request_for_update(reference)?);
    }

    // (0074,1216) Unified Procedure Step Performed Procedure Sequence - the
    // one item carrying the whole performer report. PS3.3 Table C.30.3-1 makes
    // every performed attribute a CHILD of this item: the two datetimes, the
    // performed codes, and the output list. A conformant client therefore sends
    // one nested item rather than four top-level attributes, and dcm4chee-arc
    // 5.35.2 discards the flat form outright (it answers 200 and stores
    // nothing), which is what made a completed workitem name no results.
    //
    // The item replaces the state it carries and leaves the rest alone: an item
    // naming only the output list does not erase a start time stored by an
    // earlier Update. Each child is therefore read independently below, exactly
    // as 11.6.1.4 replaces the elements a request carries.
    if let Some(item) = performed_item(body_obj)? {
        // (0040,4033) Output Information Sequence. Present -> REPLACED whole.
        // A present-but-empty sequence is refused: a COMPLETED workitem with an
        // empty output list would claim it produced nothing while reporting
        // success, which is the one statement AIW-I 4.84.4.1.2.1 exists to
        // prevent.
        if let Some(sequence) = item.obj_get("00404033") {
            let outputs = extract_output_information(sequence)?;
            if outputs.is_empty() {
                return Err(bad_request(
                    "(0040,4033) Output Information Sequence is present but empty; \
                     omit it to say nothing, or list the created instances",
                ));
            }
            updated.output_information = Some(outputs);
        }

        // (0040,4019)/(0040,4028) the performer's own codes, one item each.
        if item.obj_get("00404019").is_some() {
            updated.performed_workitem_code = extract_code_sequence(item, "00404019")?;
        }
        if item.obj_get("00404028").is_some() {
            updated.performed_station_name_code = extract_code_sequence(item, "00404028")?;
        }

        // (0040,4050)/(0040,4051) when the performer says it ran. Read as DT
        // string values: the report is the performer's own account, so this
        // server stores what it is told rather than reading a clock on its
        // behalf (the two would disagree, and the performer's is the record).
        if let Some(start) = item.obj_get("00404050") {
            updated.performed_start_datetime = Some(extract_required_dt(start, "00404050")?);
        }
        if let Some(end) = item.obj_get("00404051") {
            updated.performed_end_datetime = Some(extract_required_dt(end, "00404051")?);
        }
    }

    // The state is carried over untouched, and nothing is published: this is
    // an attribute update, not a transition.
    let stamp = now_stamp();
    let mut record = workitem_to_record(&updated);
    record.updated_at = Some(stamp);

    match store.cas_update(&record, Some(request_transaction_uid)) {
        Ok(()) => render_workitem_response(200, &updated, accept),
        Err(clarus_core::ups::StoreError::CasConflict(_)) => Err(UpsErr {
            status: 409,
            body: err_msg_json(
                "Conflict",
                "Transaction UID mismatch: workitem changed concurrently",
            ),
        }),
        Err(e) => Err(store_err(e)),
    }
}

/// The one item of (0074,1216) Unified Procedure Step Performed Procedure
/// Sequence, or `None` when the payload does not carry the sequence.
///
/// PS3.3 Table C.30.3-1 types the item "Zero or one Item": one item is the
/// performer's report. An empty sequence is REFUSED rather than treated as
/// absent: the payload named an attribute to modify and then said nothing
/// about it, and answering 200 would tell a performer its report landed when
/// no item was ever read. A sequence with two items is refused too, because
/// the table allows one and choosing between two reports would be inventing a
/// fact.
fn performed_item(
    body_obj: &crate::handlers::json_common::JsonVal,
) -> Result<Option<&crate::handlers::json_common::JsonVal>, UpsErr> {
    let Some(sequence) = body_obj.obj_get("00741216") else {
        return Ok(None);
    };
    let items = sequence
        .obj_get("Value")
        .and_then(|v| v.as_array())
        .ok_or_else(|| bad_request("(0074,1216) must be a sequence with a Value array"))?;
    match items.len() {
        0 => Err(bad_request(
            "(0074,1216) Unified Procedure Step Performed Procedure Sequence is \
             present but empty; omit it to say nothing, or send the one item that \
             reports the performance",
        )),
        1 => Ok(items.first()),
        _ => Err(bad_request(
            "(0074,1216) carries more than one item; Table C.30.3-1 allows zero or one",
        )),
    }
}

/// Read a type-1 DT value ("(0040,4050) Performed Procedure Step Start
/// DateTime" and its end counterpart).
///
/// The performer that ran the step is the only party that knows when, so the
/// value is required to be present and non-empty once the element is sent: an
/// empty one would record "a performance that began at no time". The string is
/// stored verbatim - DT carries a range, a precision, and a timezone, and
/// re-formatting it would lose what the performer stated.
fn extract_required_dt(
    value: &crate::handlers::json_common::JsonVal,
    tag: &str,
) -> Result<String, UpsErr> {
    let text = value
        .obj_get("Value")
        .and_then(|v| v.first_of_array())
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if text.is_empty() {
        return Err(bad_request(&format!(
            "({tag}) Performed Procedure Step DateTime is present but empty"
        )));
    }
    Ok(text.to_string())
}

/// The attributes the Update transaction (PS3.18 11.6) writes.
///
/// Exactly the AIW-I 4.84.4.1.2.1 performer report: what was produced, and
/// the two codes describing who produced it, plus the three attributes a
/// requester may still tune while the step runs - the label, the priority,
/// and the performing system's parameter list (PS3.4 Table CC.2.5-3 types
/// all three 3/2 at N-SET, so a requester that changes its mind has a
/// standard way to say so) - and (0040,4018) Scheduled Workitem Code
/// Sequence, which Table CC.2.5-3 types 3/1 at N-SET: the SCP SHALL handle
/// its update. Anything else in a payload is refused (see
/// [`ups_update_attributes`]).
const UPDATE_MERGE_SET: &[&str] = &[
    "00741216", "00741204", "00741200", "00741210", "0040A370", "00404018",
];

/// Read (0074,1204) Procedure Step Label from an Update payload.
///
/// Type 1 in Table C.30.2-1: a label is the human handle the worklist is read
/// by, so the element is required to carry a non-empty SH once it is sent. An
/// empty one would blank the handle while reporting success, which is the
/// same silent acceptance an absent Item refuses.
fn extract_required_label(
    value: &crate::handlers::json_common::JsonVal,
    tag: &str,
) -> Result<String, UpsErr> {
    let text = value
        .obj_get("Value")
        .and_then(|v| v.first_of_array())
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if text.trim().is_empty() {
        return Err(bad_request(&format!(
            "({tag}) is present but empty; omit it to leave it unchanged"
        )));
    }
    Ok(text.to_string())
}

/// Read (0074,1200) Scheduled Procedure Step Priority from an Update payload.
///
/// The value set is enumerated (HIGH/MEDIUM/LOW in Table C.30.2-1), so an
/// unrecognized string is refused instead of stored: a priority no performer
/// recognizes would sort the workitem into no queue at all.
fn extract_required_priority(
    value: &crate::handlers::json_common::JsonVal,
    tag: &str,
) -> Result<String, UpsErr> {
    let text = extract_required_label(value, tag)?;
    if !matches!(text.as_str(), "HIGH" | "MEDIUM" | "LOW") {
        return Err(bad_request(&format!(
            "({tag}) Scheduled Procedure Step Priority '{text}' is not one of \
             HIGH, MEDIUM, LOW (PS3.3 Table C.30.2-1)"
        )));
    }
    Ok(text)
}

/// Read (0040,A370) Referenced Request Sequence from an Update payload.
///
/// Table CC.2.5-3 marks the SEQUENCE itself "Not allowed" at N-SET, but
/// (0040,1002) Reason for the Requested Procedure lives inside it and nowhere
/// else, so a requester that corrects the reason has to send the item. This
/// server therefore accepts the sequence in exactly the shape that carries
/// that one fact - a single item whose only attribute is (0040,1002) - and
/// refuses anything else rather than merging a subtree it cannot round-trip:
/// the study UID and the accession number are "Not allowed" at N-SET (see
/// `standard/ps3.4/ups-cc-2.5-3-nset-merge.json`), so accepting them would
/// report a write that never lands.
fn extract_referenced_request_for_update(
    value: &crate::handlers::json_common::JsonVal,
) -> Result<String, UpsErr> {
    let items = value
        .obj_get("Value")
        .and_then(|v| v.as_array())
        .ok_or_else(|| bad_request("(0040,A370) must be a sequence with a Value array"))?;
    let Some(item) = items.first() else {
        return Err(bad_request(
            "(0040,A370) Referenced Request Sequence is present but carries no item; \
             omit it to leave it unchanged",
        ));
    };
    if items.len() > 1 {
        return Err(bad_request(
            "(0040,A370) carries more than one item; this implementation updates \
             a single requested procedure",
        ));
    }
    for key in item.keys() {
        if key != "00401002" {
            return Err(bad_request(&format!(
                "(0040,A370) nested attribute '{key}' is not updatable by this \
                 implementation; only (0040,1002) Reason for the Requested \
                 Procedure is"
            )));
        }
    }
    let element = item.obj_get("00401002").ok_or_else(|| {
        bad_request("(0040,A370) item carries no (0040,1002) Reason for the Requested Procedure")
    })?;
    let text = extract_required_label(element, "00401002")?;
    Ok(text)
}

/// Read (0040,4033) Output Information Sequence into reference pairs.
///
/// `sequence` is the SEQUENCE element itself - when it is nested inside
/// (0074,1216) the caller passes the item's own (0040,4033) and not the whole
/// payload, so the reader never has to know which level it was called from.
///
/// Each item is a (0008,1150)/(0008,1155) pair. An item with no instance UID
/// names nothing and is refused rather than stored, because a reference a
/// consumer cannot resolve is worse than no reference at all.
fn extract_output_information(
    sequence: &crate::handlers::json_common::JsonVal,
) -> Result<Vec<OutputReference>, UpsErr> {
    let items = sequence
        .obj_get("Value")
        .and_then(|v| v.as_array())
        .ok_or_else(|| bad_request("(0040,4033) must be a sequence with a Value array"))?;

    let mut outputs = Vec::with_capacity(items.len());
    for item in items {
        let instance = item
            .obj_get("00081155")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.first_of_array())
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if instance.is_empty() {
            return Err(bad_request(
                "(0040,4033) item has no (0008,1155) Referenced SOP Instance UID",
            ));
        }
        let class = item
            .obj_get("00081150")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.first_of_array())
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // (0020,000E) Series Instance UID is 1C in the Referenced Instances and
        // Access Macro (PS3.3 Table 10-3b), so a performer may name it and a
        // consumer needs it to build a retrieve URL. Absent or empty leaves it
        // empty rather than refusing the item.
        let series = item
            .obj_get("0020000E")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.first_of_array())
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        outputs.push(OutputReference {
            sop_class_uid: class,
            series_instance_uid: series,
            sop_instance_uid: instance,
        });
    }
    Ok(outputs)
}

/// Read a one-item `CodeSequenceMacro` attribute: value, scheme, meaning.
///
/// `parent` is the object that HOLDS `tag` - the payload for an attribute at
/// the top level of the data set, or the (0074,1216) item when the macro is a
/// child of the performer report, which is where PS3.3 Table C.30.3-1 puts
/// both performed codes.
///
/// PS3.3 Table 8.8-1 makes the Code Value type 1, so an item without one is
/// refused. An absent attribute yields `None`, which is the "the performer
/// states nothing here" case AIW-I 4.84.4.1.2.1 allows ("the same code as
/// Scheduled, if present").
fn extract_code_sequence(
    parent: &crate::handlers::json_common::JsonVal,
    tag: &str,
) -> Result<Option<WorkItemCode>, UpsErr> {
    let item = parent
        .obj_get(tag)
        .and_then(|v| v.obj_get("Value"))
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .ok_or_else(|| bad_request(&format!("({tag}) must be a sequence with one item")))?;

    let read = |inner: &str| -> String {
        item.obj_get(inner)
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.first_of_array())
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let value = read("00080100");
    if value.is_empty() {
        return Err(bad_request(&format!(
            "({tag}) item has no (0008,0100) Code Value"
        )));
    }
    Ok(Some(WorkItemCode {
        value,
        scheme: read("00080102"),
        meaning: read("00080104"),
    }))
}

/// A 400 with a plain message, the code 11.7.3-1 gives for a request that is
/// invalid for the workitem's current state and for a missing/incorrect
/// Transaction UID.
fn bad_request(message: &str) -> UpsErr {
    UpsErr {
        status: 400,
        body: err_msg_json("Bad Request", message),
    }
}

/// The type-1 attributes of PS3.4 Table CC.2.5-3 at N-CREATE, in the order
/// the standard lists them so the message names the first that is missing.
///
/// Returns the four values on success, or the human-readable name of the
/// missing attribute.
fn required_create_tags(
    body: &crate::handlers::json_common::JsonVal,
) -> Result<(String, String, String, String), &'static str> {
    let priority = extract_tag(body, "00741200");
    let label = extract_tag(body, "00741204");
    let start = extract_tag(body, "00404005");
    let readiness = extract_tag(body, "00404041");

    match (priority, label, start, readiness) {
        (None, ..) => Err("(0074,1200) Scheduled Procedure Step Priority"),
        (_, None, ..) => Err("(0074,1204) Procedure Step Label"),
        (_, _, None, _) => Err("(0040,4005) Scheduled Procedure Step Start DateTime"),
        (_, _, _, None) => Err("(0040,4041) Input Readiness State"),
        (Some(priority), Some(label), Some(start), Some(readiness)) => {
            Ok((label, readiness, start, priority))
        }
    }
}

/// The station slot of a create body.
///
/// PS3.4 Table CC.2.5-3 carries the station in (0040,4025) Scheduled Station
/// Name Code Sequence and gives (0040,0001) Scheduled Station AE Title no row,
/// so the Code Value is the only channel. AIW-I 4.81.4.1.2.1 Table
/// 4.81.4.1.2.1-3 then makes that Code Value the AE Title a performer claims,
/// which is why a station-oriented search (AIW-I 4.81.4.1.2.2) and the claim
/// guard are looking at the same string. A create that carries no code names
/// no station, and the store keeps the slot empty rather than inventing one.
fn station_slot(body: &crate::handlers::json_common::JsonVal) -> Option<String> {
    extract_workitem_code(body, "00404025").map(|code| code.value)
}

/// Read the input study UID from the first item of (0040,4021) Input
/// Information Sequence.
///
/// The sequence is the standard's only route from a workitem to the study it
/// operates on (Study Instance UID is 1C in CC.2.5-3 and has nowhere else to
/// live in the JSON shape). Absent or shapeless sequence -> `None`, which the
/// caller treats as "this workitem names no study".
fn extract_input_study_uid(body: &crate::handlers::json_common::JsonVal) -> Option<String> {
    extract_first_item_tag(body, "00404021", "0020000D")
}

/// Read one tag from the FIRST item of a one-level sequence.
///
/// The sequence shape is `{seq: {Value: [{tag: {Value: [..]}}], vr: "SQ"}}` -
/// the DICOMweb JSON encoding, where the item is an element of `Value` and the
/// tag inside it is a normal element of that item. Absent or shapeless
/// sequence -> `None`.
///
/// Shared by the two sequences a Create body may carry: Input Information
/// Sequence (0040,4021) and Referenced Request Sequence (0040,A370).
fn extract_first_item_tag(
    body: &crate::handlers::json_common::JsonVal,
    seq_tag: &str,
    item_tag: &str,
) -> Option<String> {
    let first = body.obj_get(seq_tag)?.obj_get("Value")?.first_of_array()?;
    extract_tag(first, item_tag)
}

/// Read a one-item code sequence: the first item's Code Value, Coding Scheme
/// Designator and Code Meaning.
///
/// Used for (0040,4018) Scheduled Workitem Code Sequence on the create path.
/// The server does not INTERPRET the code (AIW-I 50.4.1.2 leaves the code set
/// to deployment agreement), it stores it so the workitem it hands out is the
/// one the requester sent. A sequence item with no Code Value is not a code
/// and is dropped rather than stored as a half-code.
fn extract_workitem_code(
    body: &crate::handlers::json_common::JsonVal,
    tag: &str,
) -> Option<WorkItemCode> {
    let first = body.obj_get(tag)?.obj_get("Value")?.first_of_array()?;
    let value = extract_tag(first, "00080100")?;
    if value.trim().is_empty() {
        return None;
    }
    Some(WorkItemCode {
        value,
        scheme: extract_tag(first, "00080102").unwrap_or_default(),
        meaning: extract_tag(first, "00080104").unwrap_or_default(),
    })
}

/// Read a code sequence, distinguishing "absent" from "present but empty".
///
/// [`extract_workitem_code`] answers `None` for both, which is right where the
/// caller only needs the value. On a SEARCH body the difference is the
/// difference between an unfiltered question and a malformed one, so this
/// variant reports the malformed case as an error instead of dropping the
/// filter. `name` is the sequence's DICOM keyword, used in the diagnostics so
/// a client is told which of its sequences is wrong.
fn extract_workitem_code_checked(
    body: &crate::handlers::json_common::JsonVal,
    tag: &str,
    name: &str,
) -> Result<Option<WorkItemCode>, UpsErr> {
    let Some(seq) = body.obj_get(tag) else {
        return Ok(None);
    };
    let Some(first) = seq.obj_get("Value").and_then(|v| v.first_of_array()) else {
        return Err(UpsErr {
            status: 400,
            body: err_msg_json(
                "Bad Request",
                &format!("{name} must carry one item with a Code Value"),
            ),
        });
    };
    let Some(value) = extract_tag(first, "00080100") else {
        return Err(UpsErr {
            status: 400,
            body: err_msg_json(
                "Bad Request",
                &format!("{name} item has no Code Value (0008,0100)"),
            ),
        });
    };
    if value.trim().is_empty() {
        return Err(UpsErr {
            status: 400,
            body: err_msg_json(
                "Bad Request",
                &format!("{name} Code Value (0008,0100) is empty"),
            ),
        });
    }
    Ok(Some(WorkItemCode {
        value,
        scheme: extract_tag(first, "00080102").unwrap_or_default(),
        meaning: extract_tag(first, "00080104").unwrap_or_default(),
    }))
}

/// Read (0040,4018) Scheduled Workitem Code Sequence and check its scheme.
///
/// PS3.3 8.2: a Coding Scheme Designator beginning with `99` is private or
/// local, and any site may use such schemes; every other designator names a
/// vocabulary the site has to have declared (`[ups.coding_schemes]`). A code
/// whose scheme nobody knows cannot be looked up by any performer, so storing
/// it would hand out a workitem that no rule can match - the silent
/// acceptance this server refuses everywhere else.
///
/// An EMPTY designator is refused too: (0008,0102) is type 1 inside a code
/// item (PS3.3 Table 8.8-1), and an item without one is not a code.
///
/// The Code Value and Code Meaning are NOT validated - they are the site's
/// own vocabulary, and a length rule would reject a legitimate long code for
/// no conformance gain.
///
/// The tag is part of the diagnostic name so the Create and Update paths tell
/// a client WHICH sequence is wrong, not just its keyword: a payload carrying
/// several sequences would otherwise have no way to know which one the
/// refusal is about.
fn extract_scheduled_workitem_code_checked(
    body: &crate::handlers::json_common::JsonVal,
    state: &AppState,
) -> Result<Option<WorkItemCode>, UpsErr> {
    const NAME: &str = "Scheduled Workitem Code Sequence (0040,4018)";
    let Some(code) = extract_workitem_code_checked(body, "00404018", NAME)? else {
        return Ok(None);
    };
    let scheme = code.scheme.trim();
    if scheme.is_empty() {
        return Err(UpsErr {
            status: 400,
            body: err_msg_json(
                "Bad Request",
                "Missing Coding Scheme Designator (0008,0102) in Scheduled \
                 Workitem Code Sequence",
            ),
        });
    }
    if !state.config.ups.allows_scheme(scheme) {
        return Err(UpsErr {
            status: 400,
            body: err_msg_json(
                "Bad Request",
                &format!(
                    "Unknown Coding Scheme Designator: {scheme}. Add it to \
                     [ups.coding_schemes] or use a 99-prefixed private scheme."
                ),
            ),
        });
    }
    Ok(Some(code))
}

/// Read (0074,1210) Scheduled Processing Parameters Sequence into its stored
/// form.
///
/// The element belongs to the PERFORMING system (PS3.3 Table C.30.2-1: its
/// items are Content Items whose Concept Names the server has no vocabulary
/// for), so the server does not model it - it serializes the parsed subtree
/// and hands it back on every read. That is what "store as-is, return
/// unchanged" can mean for a server that does not interpret the value: the
/// VALUE subtree survives exactly, and only the outer wrapper is re-emitted
/// in this server's own `{"Value":...,"vr":"SQ"}` shape.
///
/// An absent element is `None`. A present one that is not a sequence is
/// refused rather than stored as a shape no reader could use - the same
/// "present but malformed" discipline the code-sequence reader applies. An
/// EMPTY `Value` array is legal and stores an empty sequence: PS3.4 Table
/// CC.2.5-3 types the attribute 3/2 at N-SET, and "the performer gets no
/// parameters" is a real state, not a missing one.
fn extract_scheduled_processing_parameters(
    body: &crate::handlers::json_common::JsonVal,
) -> Result<Option<String>, UpsErr> {
    let Some(entry) = body.obj_get("00741210") else {
        return Ok(None);
    };
    let Some(value) = entry.obj_get("Value") else {
        return Err(UpsErr {
            status: 400,
            body: err_msg_json(
                "Bad Request",
                "Scheduled Processing Parameters Sequence (0074,1210) has no Value array",
            ),
        });
    };
    if !matches!(value, crate::handlers::json_common::JsonVal::Arr(_)) {
        return Err(UpsErr {
            status: 400,
            body: err_msg_json(
                "Bad Request",
                "Scheduled Processing Parameters Sequence (0074,1210) Value must be an array",
            ),
        });
    }
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(br#"{"Value":"#);
    crate::handlers::json_common::write_json_val(&mut out, value);
    out.extend_from_slice(br#","vr":"SQ"}"#);
    // The writer above only emits ASCII delimiters and the escaped form of
    // any string, so the result is valid JSON by construction.
    Ok(Some(String::from_utf8_lossy(&out).into_owned()))
}

// -- Delete (R-03) -------------------------------------------------------
/// `DELETE /dicomweb/workitems/{uid}` - UPS Delete.
///
/// Non-standard administrative extension for cleaning up test data. Returns
/// 204 No Content on success, 404 if not found.
pub fn ups_delete(
    state: &AppState,
    _sink: &dyn EventSink,
    workitem_uid: &str,
    _accept: Option<&str>,
) -> Result<UpsResp, UpsErr> {
    if !is_valid_uid(workitem_uid) {
        return Err(invalid_uid());
    }

    // delete from the CBOR store.
    match workitem_store(state).delete(workitem_uid) {
        Ok(true) => Ok(UpsResp {
            status: 204,
            content_type: "",
            headers: Vec::new(),
            body: Vec::new(),
        }),
        Ok(false) => Err(UpsErr {
            status: 404,
            body: err_json("Workitem not found"),
        }),
        Err(e) => Err(store_err(e)),
    }
}

// -- Search (PS3.18 Section 11.4) -----------------------------------------------

/// The station key a search filters on.
///
/// PS3.4 Table CC.2.5-3 carries the station as (0040,4025) Scheduled Station
/// Name Code Sequence and gives (0040,0001) Scheduled Station AE Title no row
/// at all, so the code sequence is the only carrier and there is nothing to
/// prefer it over. AIW-I 4.81.4.1.2.1 Table 4.81.4.1.2.1-3 puts the performer
/// AE Title in its Code Value, so a station-oriented query and the claim guard
/// name the same string.
#[must_use]
fn station_key(query: &UpsQuery) -> Option<&str> {
    query.scheduled_station_code_value.as_deref()
}

/// `GET /dicomweb/workitems` - UPS Search.
///
/// Read-only: parses the URL query string, validates the workflow state,
/// queries the storage backend and renders the DICOMweb JSON array. Adds a
/// `Warning` header when the result is truncated by the limit.
pub fn ups_search(
    state: &AppState,
    query_str: Option<&str>,
    accept: Option<&str>,
    service: &str,
) -> Result<UpsResp, UpsErr> {
    let content_type = negotiate(accept);

    // Parse the query string into UpsQuery (native percent-decoder, no axum).
    // An unrecognised key is refused rather than ignored: a dropped matching
    // key widens the result set to the whole worklist, which a caller cannot
    // distinguish from a genuinely broad match (PS3.18 8.3.4 fixes the key
    // vocabulary, so an unknown key is a malformed request).
    let mut query: UpsQuery = match query_str {
        Some(s) if !s.is_empty() => match parse_form_query(s) {
            Ok(q) => q,
            Err(msg) => {
                return Err(UpsErr {
                    status: 400,
                    body: err_msg_json("Bad Request", &msg),
                });
            }
        },
        _ => UpsQuery::default(),
    };

    // Server default for an absent explicit `limit` (12.09): `[ups]
    // default_limit` (0 = unlimited). Explicit `limit=0` is normalized to
    // unlimited, mirroring the QIDO `resolve_limit` semantics.
    if query.limit == Some(0) {
        query.limit = None;
    } else if query.limit.is_none() {
        let d = state.config.ups.default_limit;
        if d != 0 {
            query.limit = Some(d);
        }
    }

    // Validate workflow_state if provided
    if let Some(ref state_val) = query.workflow_state
        && !is_valid_state(state_val)
    {
        return Err(UpsErr {
            status: 400,
            body: err_msg_json(
                "Bad Request",
                &format!(
                    "Invalid Procedure Step State: '{}'. Must be one of: {:?}",
                    state_val,
                    super::ups::state_machine::VALID_STATES
                ),
            ),
        });
    }

    let filter = clarus_core::ups::StoreFilter {
        workitem_uid: None,
        // (0020,000D) names the STUDY; the workitem's own UID is not a search
        // key here, so `workitem_uid` stays unset on this path.
        study_uid: query.study_uid.as_deref(),
        // (0040,A370 > 0008,0050) Accession Number. An absent or empty key
        // matches everything, the same zero-length-key convention the
        // Modality line below applies.
        accession_number: query.accession_number.as_deref().filter(|a| !a.is_empty()),
        step_label: query.step_label.as_deref(),
        station_aet: station_key(&query),
        // (0040,4018) Scheduled Workitem Code Sequence > (0008,0100) Code
        // Value - a REQUIRED matching key (PS3.4 Table CC.2.5-3). The record
        // stores the whole code item, so the predicate compares its Code
        // Value; a workitem with no code does not match a code query.
        scheduled_workitem_code_value: query.scheduled_workitem_code_value.as_deref(),
        requested_procedure_id: query.requested_procedure_id.as_deref(),
        patient_id: query.patient_id.as_deref(),
        // An empty parameter is the same statement as an absent one: match
        // all, which is the DICOMweb convention for a zero-length key.
        modality: query.modality.as_deref().filter(|m| !m.is_empty()),
        workflow_state: query.workflow_state.as_deref(),
        limit: query.limit,
        offset: query.offset,
    };
    let result = workitem_store(state).search(&filter);

    match result {
        Ok((records, total_matches)) => {
            let workitems: Vec<WorkItem> = records.iter().map(record_to_workitem).collect();

            // PS3.18 8.3.4.4.1: `remaining = matches - offset - returned`,
            // saturating; the Warning uses the fixed wording and names the
            // host:port the request was sent to.
            let remaining = (total_matches as u64)
                .saturating_sub(query.offset.unwrap_or(0))
                .saturating_sub(workitems.len() as u64);
            let mut headers = Vec::new();
            if remaining > 0 {
                headers.push((
                    "Warning",
                    format!(
                        "299 {service}: There are {remaining} additional results that can be requested"
                    ),
                ));
            }

            // -- XML path (typed writer, always-on) ---------
            if content_type == super::DICOM_XML_CONTENT_TYPE {
                // A search answer is a LIST: PS3.18 8.7.2 carries a list of
                // models as multipart/related with one document per model, so
                // each workitem keeps its own attribute set instead of being
                // flattened into one model root.
                //
                // `UpsResp::content_type` is a `&'static str` and the multipart
                // media type carries a per-response boundary, so it travels as
                // a header and the static field stays empty (the renderer
                // skips an empty content type).
                let boundary = clarus_adapters::boundary_id();
                let body = super::ups::xml::workitems_to_multipart(&workitems, &boundary);
                let mut multipart_headers = headers;
                multipart_headers.push((
                    "Content-Type",
                    super::ups::xml::xml_multipart_content_type(&boundary),
                ));
                return Ok(UpsResp {
                    status: 200,
                    content_type: "",
                    headers: multipart_headers,
                    body,
                });
            }

            // -- JSON path (default) -------------------------------
            let mut body = Vec::new();
            body.push(b'[');
            for (i, wi) in workitems.iter().enumerate() {
                if i > 0 {
                    body.push(b',');
                }
                write_workitem_json(&mut body, wi);
            }
            body.push(b']');
            Ok(UpsResp {
                status: 200,
                content_type: super::DICOM_JSON_CONTENT_TYPE,
                headers,
                body,
            })
        }
        Err(e) => Err(store_err(e)),
    }
}

// -- Get one workitem (PS3.18 11.3) -------------------------------------

/// `GET /dicomweb/workitems/{uid}` - UPS Retrieve.
///
/// Returns the single workitem as a DICOMweb JSON object (not an array);
/// 404 when not found. served from the CBOR workitem store.
pub fn ups_get(
    state: &AppState,
    workitem_uid: &str,
    accept: Option<&str>,
) -> Result<UpsResp, UpsErr> {
    let _content_type = negotiate(accept);

    if !is_valid_uid(workitem_uid) {
        return Err(invalid_uid());
    }

    match workitem_store(state).find_by_uid(workitem_uid) {
        Ok(Some(rec)) => {
            let wi = record_to_workitem(&rec);
            render_workitem_response(200, &wi, accept)
        }
        Ok(None) => Err(UpsErr {
            status: 404,
            body: err_msg_json(
                "Not Found",
                &format!("Workitem '{}' not found", workitem_uid),
            ),
        }),
        Err(e) => Err(store_err(e)),
    }
}

// -- Search-by-body (PS3.18 11.4 POST) ----------------------------------

/// Convert a DICOMweb JSON search body into the `UpsQuery` parameter form
/// used by the GET search, so the POST `?search` endpoint dispatches onto the
/// exact same code path (byte-identical results).
///
/// Recognised request keys (same tags the GET query uses):
///   `0020000D` study instance uid, `0040A370.00080050` accession number,
///   `00741204` step label, `00404025`
///   Scheduled Station Name Code Sequence Code Value (the AIW-I form of the
///   station key), `00100020` patient_id, `00741000` procedure
///   step state, `00404018` Scheduled Workitem Code Sequence Code Value.
/// Unknown tags are ignored (DICOMweb matching semantics: present keys filter).
pub fn ups_search_from_body(
    state: &AppState,
    body: &[u8],
    accept: Option<&str>,
    service: &str,
) -> Result<UpsResp, UpsErr> {
    let parsed = parse_json(body).ok_or_else(body_not_object)?;
    let obj = single_dataset(&parsed).ok_or_else(body_not_object)?;
    let query = query_from_body(obj)?;

    // Reroute onto the shared search path (also validates workflow_state).
    let query_str = form_urlencode_query(&query);
    ups_search(state, Some(&query_str), accept, service)
}

/// The `UpsQuery` a search body states.
///
/// Split out so the attribute reading is testable without an `AppState`: every
/// defect in this function is a filter that quietly disappears, and a filter
/// that disappears produces an UNFILTERED answer rather than an error, so the
/// only way to catch one is to assert on the query it parsed.
fn query_from_body(obj: &JsonVal) -> Result<UpsQuery, UpsErr> {
    let mut query = UpsQuery::default();
    if let Some(v) = extract_tag(obj, "0020000D") {
        query.study_uid = Some(v);
    }
    // (0008,0050) lives INSIDE (0040,0370) Referenced Request Sequence, so the
    // first item is where a conformant body puts it. The scalar helper on the
    // root could only ever match a non-standard body, and a dotted key like
    // "0040A370.00080050" matches nothing at all - `extract_tag` is an exact
    // object lookup, so the dotted spelling silently read no accession and the
    // request answered with an unfiltered worklist. A root-level (0008,0050) is
    // still honoured: some clients flatten it, and refusing them would trade
    // one silent filter loss for another.
    if let Some(v) =
        extract_first_item_tag(obj, "0040A370", "00080050").or_else(|| extract_tag(obj, "00080050"))
    {
        query.accession_number = Some(v);
    }
    if let Some(v) = extract_tag(obj, "00741204") {
        query.step_label = Some(v);
    }
    // (0040,4025) ScheduledStationNameCodeSequence is a CODE SEQUENCE, not a
    // scalar: reading it with the scalar helper returned None for every
    // conformant body, and a filter that silently disappears turns the search
    // into an UNFILTERED worklist - the client is answered with workitems it
    // did not ask for and has no way to tell. PS3.4 Table CC.2.5-3 types the
    // attribute SQ, so the sequence helper is the only correct reader.
    //
    // A sequence that is PRESENT but carries no Code Value is not the same as
    // an absent filter: the client asked to be narrowed and cannot be. Falling
    // through to "no filter" would answer with the whole worklist of every
    // station - another performer's work, on a shared archive. The request is
    // therefore refused, which is the only answer the client can act on.
    if let Some(code) =
        extract_workitem_code_checked(obj, "00404025", "Scheduled Station Name Code Sequence")?
    {
        query.scheduled_station_code_value = Some(code.value);
    }
    // (0040,4018) is the SAME shape and the same trap: the unchecked reader
    // answers `None` for "absent" and for "present but malformed" alike, so a
    // body whose Scheduled Workitem Code Sequence carries no Code Value was
    // silently answered with the whole worklist instead of being refused.
    // PS3.4 Table CC.2.5-3 types it SQ, so it is read the same way.
    if let Some(code) =
        extract_workitem_code_checked(obj, "00404018", "Scheduled Workitem Code Sequence")?
    {
        query.scheduled_workitem_code_value = Some(code.value);
    }
    if let Some(v) = extract_first_item_tag(obj, "0040A370", "00401001") {
        query.requested_procedure_id = Some(v);
    }
    if let Some(v) = extract_tag(obj, "00100020") {
        query.patient_id = Some(v);
    }
    // (0008,0060) Modality. The GET search filters on it, so a body that states
    // it must filter identically: ignoring it answered a search with every
    // modality's workitems, and the caller had no way to tell.
    if let Some(v) = extract_tag(obj, "00080060") {
        query.modality = Some(v);
    }
    if let Some(v) = extract_tag(obj, "00741000") {
        query.workflow_state = Some(v);
    }
    Ok(query)
}

// -- Response rendering (JSON / XML) -------------------------------------

/// Render a workitem into a 200/201 response.
///
/// The JSON path uses the native `workitem_to_dicomweb_json` render. The XML
/// path uses the typed `workitem_to_xml`, so the always-on XML
/// producer is independent of the serde bridge.
fn render_workitem_response(
    status: u16,
    wi: &WorkItem,
    accept: Option<&str>,
) -> Result<UpsResp, UpsErr> {
    let content_type = negotiate(accept);

    if content_type == super::DICOM_XML_CONTENT_TYPE {
        let xml = super::ups::xml::workitem_to_xml(wi).into_bytes();
        return Ok(UpsResp {
            status,
            content_type: super::DICOM_XML_CONTENT_TYPE,
            headers: Vec::new(),
            body: xml,
        });
    }

    let mut body = Vec::new();
    write_workitem_json(&mut body, wi);
    Ok(UpsResp {
        status,
        content_type: super::DICOM_JSON_CONTENT_TYPE,
        headers: Vec::new(),
        body,
    })
}

// -- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::ups::event_bus::FilterRouting;
    use std::sync::{Arc, Mutex};

    /// m19: the station and accession filters must be READ, not silently lost.
    ///
    /// Both attributes are sequences in a conformant body. Reading them with
    /// the scalar helper yields None, and a query with one fewer key does not
    /// fail - it searches a wider set. The client is then answered with
    /// workitems it did not ask for and has no way to notice, which is why
    /// this is asserted on the parsed query rather than on a response.
    #[test]
    fn a_body_search_reads_the_station_and_accession_sequences() {
        // PS3.18 11.4 body: (0040,4025) ScheduledStationNameCodeSequence and
        // (0040,0370) ReferencedRequestSequence > (0008,0050) AccessionNumber.
        let body = br#"{
            "00404025": {"vr":"SQ","Value":[
                {"00080100":{"vr":"SH","Value":["CT01"]},
                 "00080102":{"vr":"SH","Value":["99CLARUS"]},
                 "00080104":{"vr":"LO","Value":["CT room 1"]}}]},
            "0040A370": {"vr":"SQ","Value":[
                {"00080050":{"vr":"SH","Value":["ACC-7"]},
                 "00401001":{"vr":"SH","Value":["RP-7"]}}]}
        }"#;
        let parsed = parse_json(body).expect("valid json");
        let obj = single_dataset(&parsed).expect("one data set");
        let Some(query) = query_from_body(obj).ok() else {
            panic!("a conformant body must parse");
        };

        assert_eq!(
            query.scheduled_station_code_value.as_deref(),
            Some("CT01"),
            "the station code sequence must reach the query"
        );
        assert_eq!(
            query.accession_number.as_deref(),
            Some("ACC-7"),
            "the accession inside ReferencedRequestSequence must reach the query"
        );
        // (0040,1001) Requested Procedure ID rides the same item.
        assert_eq!(query.requested_procedure_id.as_deref(), Some("RP-7"));
    }

    /// The flattened spellings some clients send are still honoured, so the
    /// fix does not trade one silently-dropped filter for another.
    #[test]
    fn a_body_search_accepts_the_flattened_accession() {
        let body = br#"{"00080050":{"vr":"SH","Value":["ACC-9"]}}"#;
        let parsed = parse_json(body).expect("valid json");
        let obj = single_dataset(&parsed).expect("one data set");
        let Some(query) = query_from_body(obj).ok() else {
            panic!("a flattened accession still parses");
        };
        assert_eq!(query.accession_number.as_deref(), Some("ACC-9"));
    }

    /// A station code with no Code Value is not a code: it must not widen the
    /// query either, because an empty filter value matches nothing useful and
    /// a fabricated one matches something wrong.
    ///
    /// The request is REFUSED rather than answered unfiltered: a client that
    /// asked for one station's work and left the Code Value out cannot be
    /// given every station's work in reply - on a shared archive that is
    /// another performer's list.
    #[test]
    fn a_body_search_refuses_a_station_item_with_no_code_value() {
        let body = br#"{"00404025":{"vr":"SQ","Value":[
            {"00080102":{"vr":"SH","Value":["99CLARUS"]}}]}}"#;
        let parsed = parse_json(body).expect("valid json");
        let obj = single_dataset(&parsed).expect("one data set");
        let status = match query_from_body(obj) {
            Ok(_) => panic!("a valueless code sequence must be refused"),
            Err(e) => e.status,
        };
        assert_eq!(status, 400, "the client must be told, not widened");
    }

    /// (0040,4018) is the same trap as (0040,4025) and used to be read with the
    /// UNCHECKED helper: a Scheduled Workitem Code Sequence with no Code Value
    /// answered `None`, the filter was dropped, and the reply was every
    /// workitem in the store - the opposite of what the client asked for.
    #[test]
    fn a_body_search_refuses_a_workitem_code_item_with_no_code_value() {
        let body = br#"{"00404018":{"vr":"SQ","Value":[
            {"00080102":{"vr":"SH","Value":["99CLARUS"]}}]}}"#;
        let parsed = parse_json(body).expect("valid json");
        let obj = single_dataset(&parsed).expect("one data set");
        let status = match query_from_body(obj) {
            Ok(_) => panic!("a valueless workitem code sequence must be refused"),
            Err(e) => e.status,
        };
        assert_eq!(status, 400, "the client must be told, not widened");
    }

    /// The diagnostic names the sequence the client actually got wrong, so a
    /// body carrying both sequences does not blame the wrong one.
    #[test]
    fn a_workitem_code_error_names_the_workitem_sequence() {
        let body = br#"{"00404018":{"vr":"SQ","Value":[]}}"#;
        let parsed = parse_json(body).expect("valid json");
        let obj = single_dataset(&parsed).expect("one data set");
        let err = query_from_body(obj).expect_err("an empty sequence is refused");
        let text = String::from_utf8_lossy(&err.body).to_string();
        assert!(
            text.contains("Scheduled Workitem Code Sequence"),
            "the message names the offending sequence: {text}"
        );
        assert!(
            !text.contains("Scheduled Station Name"),
            "and must not blame the other sequence: {text}"
        );
    }

    /// A recording sink that captures events without any transport.
    #[derive(Default)]
    struct RecordingSink {
        events: Arc<Mutex<Vec<UpsEvent>>>,
    }

    impl RecordingSink {
        fn events(&self) -> Vec<UpsEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl EventSink for RecordingSink {
        fn publish(&self, event: UpsEvent) {
            self.events.lock().unwrap().push(event);
        }
        fn notify_created(&self, uid: &str, filter: FilterRouting) {
            self.publish(UpsEvent {
                workitem_uid: uid.to_string(),
                previous_state: String::new(),
                new_state: "SCHEDULED".to_string(),
                timestamp: String::new(),
                event_id: 0,
                step_label: filter.step_label.clone(),
                scheduled_code: filter.scheduled_code.clone(),
                input_readiness: filter.input_readiness.clone().unwrap_or_default(),
                filter: Box::new(filter),
            });
        }
        fn notify_state_change(&self, uid: &str, previous: &str, new: &str, filter: FilterRouting) {
            // Reference behaviour: mute equal transitions.
            if previous != new {
                self.publish(UpsEvent {
                    workitem_uid: uid.to_string(),
                    previous_state: previous.to_string(),
                    new_state: new.to_string(),
                    timestamp: String::new(),
                    event_id: 0,
                    step_label: filter.step_label.clone(),
                    scheduled_code: filter.scheduled_code.clone(),
                    input_readiness: filter.input_readiness.clone().unwrap_or_default(),
                    filter: Box::new(filter),
                });
            }
        }
    }

    /// A fake `AppState` is not cheap to construct here, so we test the pure
    /// decision helpers directly (state machine + event-decision), which is
    /// where the business rules live.
    #[test]
    fn notify_state_change_emits_only_on_real_transition() {
        let sink = RecordingSink::default();
        sink.notify_state_change("uid1", "SCHEDULED", "IN PROGRESS", FilterRouting::default());
        assert_eq!(sink.events().len(), 1);
        assert_eq!(sink.events()[0].workitem_uid, "uid1");
        assert_eq!(sink.events()[0].previous_state, "SCHEDULED");
        assert_eq!(sink.events()[0].new_state, "IN PROGRESS");

        // Equal transition -> muted.
        sink.notify_state_change(
            "uid1",
            "IN PROGRESS",
            "IN PROGRESS",
            FilterRouting::default(),
        );
        assert_eq!(sink.events().len(), 1, "equal transition must not emit");
    }

    #[test]
    fn notify_created_emits_scheduled_with_empty_previous() {
        let sink = RecordingSink::default();
        sink.notify_created("uid9", FilterRouting::default());
        let ev = &sink.events()[0];
        assert_eq!(ev.workitem_uid, "uid9");
        assert_eq!(ev.previous_state, "");
        assert_eq!(ev.new_state, "SCHEDULED");
    }

    #[test]
    fn uid_validation() {
        assert!(is_valid_uid("1.2.826.0.1.3680043.10.1234.8101"));
        assert!(!is_valid_uid(""));
        assert!(!is_valid_uid("1.2.abc"));
        assert!(!is_valid_uid("12a34"));
    }
}
