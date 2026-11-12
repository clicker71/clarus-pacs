// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Subscribe to Worklist / Subscribe to Filtered Worklist (PS3.18 11.10/11.11).
//!
//! The subscriber registers a URL with this server; this server then POSTs
//! created workitems to it. The resource tree is the standard's own:
//!
//! ```text
//! /dicomweb/workitems/1.2.840.10008.5.1.4.34.5/subscribers/{subscriber}
//! /dicomweb/workitems/1.2.840.10008.5.1.4.34.5.1/subscribers/{subscriber}
//! ```
//!
//! (PS3.18 Table 11.1.1-1: the two URI Templates for Worklist Subscription and
//! Filtered Worklist Subscription; the UIDs are the SOP Class UIDs of
//! `1.2.840.10008.5.1.4.34.5` / `.5.1`.)
//!
//! # Honest filter
//! The filtered form supports exactly one matching key, Procedure Step Label
//! (0074,1204) - a standard single-value matching key of PS3.4 Table CC.2.5-3.
//! A filter naming anything else is REFUSED with 400 and a message naming the
//! unsupported key. Silently ignoring a filter would make the subscriber
//! believe it will not receive those workitems, which is worse than an error.
//! The unfiltered form (34.5) matches every workitem.
//!
//! Modality is not offered as a key: UPS has no Modality attribute, so a
//! subscriber filtering on it would never match anything.

#![forbid(unsafe_code)]

use crate::handlers::json_common::{JsonVal, err_msg_json, parse_json};
use crate::push::{Subscription, subscriptions};

/// One filter key: the spellings that name it, and the record field it reads.
///
/// PS3.18 11.10.1.2 defines the valid filter keys as the UPS IOD attributes
/// (PS3.3 B.26.2), matched with C-FIND semantics (8.3.4.1 -> PS3.4 C.2.2.2).
/// This table is therefore derived from two facts, not invented:
///
/// - the KEY must be a PS3.3 B.26.2 attribute of the UPS IOD, so it is a
///   legal filter key at all;
/// - the RECORD must store it, because a key this server cannot match is
///   refused rather than silently dropped (see [`FilterKey::from_wire`]).
///
/// A CC.2.5-3 matching key the record does not carry (Worklist Label, the
/// human performers, the SPS expiration, ...) has no row here and is still a
/// loud 400. That is the honest refusal, not an oversight: accepting
/// `WorklistLabel=X` and matching nothing would deliver every event to a
/// subscriber that asked for a subset.
///
/// `wire` holds the spellings in lower case (the query string has no case
/// convention of its own); hex and PS3.6 keyword forms both name one key, and
/// a nested attribute may additionally be written in its sequence-qualified
/// form, which is what PS3.18 8.3.4.1's dotted grammar describes.
struct FilterKey {
    wire: &'static [&'static str],
    field: FilterField,
}

/// Which record attribute a filter key constrains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterField {
    StudyUid,
    PatientId,
    StepLabel,
    CodeValue,
    CodeScheme,
    WorkflowState,
    InputReadiness,
    SpsPriority,
    SpsStartDatetime,
    PatientName,
    PatientIdIssuer,
    AccessionNumber,
    RequestedProcedureId,
    StationAet,
}

/// Every filter key this server accepts, in the order they are documented.
///
/// The set is the intersection of "a legal PS3.18 11.10.1.2 key" and "a field
/// the workitem record stores". Read it as the specification of what a
/// subscription can honestly narrow on.
const FILTER_KEYS: &[FilterKey] = &[
    // (0020,000D) Study Instance UID, top level. CC.2.5-3 types it O, and it
    // is what a viewer subscribes with when it follows a study.
    FilterKey {
        wire: &["0020000d", "studyinstanceuid"],
        field: FilterField::StudyUid,
    },
    // (0040,A370) > (0020,000D) the SAME fact reached through the Referenced
    // Request Sequence, where CC.2.5-3 also carries Study Instance UID (O).
    // Both spellings are one field: a request names one study either way, and
    // two fields would let a filter mean two things at once.
    FilterKey {
        wire: &[
            "0040a370.0020000d",
            "referencedrequestsequence.studyinstanceuid",
        ],
        field: FilterField::StudyUid,
    },
    // (0010,0020) Patient ID - a REQUIRED matching key (CC.2.5-3, R).
    FilterKey {
        wire: &["00100020", "patientid"],
        field: FilterField::PatientId,
    },
    FilterKey {
        wire: &["00741204", "proceduresteplabel"],
        field: FilterField::StepLabel,
    },
    // Sequence Matching (PS3.4 C.2.2.2.6): the two attributes of ONE item.
    FilterKey {
        wire: &[
            "00404018.00080100",
            "scheduledworkitemcodesequence.codevalue",
        ],
        field: FilterField::CodeValue,
    },
    FilterKey {
        wire: &[
            "00404018.00080102",
            "scheduledworkitemcodesequence.codingschemedesignator",
        ],
        field: FilterField::CodeScheme,
    },
    FilterKey {
        wire: &["00741000", "procedurestepstate"],
        field: FilterField::WorkflowState,
    },
    FilterKey {
        wire: &["00404041", "inputreadinessstate"],
        field: FilterField::InputReadiness,
    },
    FilterKey {
        wire: &["00741200", "scheduledproceduresteppriority"],
        field: FilterField::SpsPriority,
    },
    FilterKey {
        wire: &["00404005", "scheduledprocedurestepstartdatetime"],
        field: FilterField::SpsStartDatetime,
    },
    FilterKey {
        wire: &["00100010", "patientname"],
        field: FilterField::PatientName,
    },
    FilterKey {
        wire: &["00100021", "issuerofpatientid"],
        field: FilterField::PatientIdIssuer,
    },
    // (0008,0050) Accession Number. CC.2.5-3 places it inside the Referenced
    // Request Sequence (R); the bare top-level spelling is the one field for
    // the one fact, which the search parser also accepts.
    FilterKey {
        wire: &[
            "00080050",
            "0040a370.00080050",
            "accessionnumber",
            "referencedrequestsequence.accessionnumber",
        ],
        field: FilterField::AccessionNumber,
    },
    FilterKey {
        wire: &[
            "00401001",
            "0040a370.00401001",
            "requestedprocedureid",
            "referencedrequestsequence.requestedprocedureid",
        ],
        field: FilterField::RequestedProcedureId,
    },
    // (0040,4025) > (0008,0100) Scheduled Station Name Code Sequence Code
    // Value. The stored station is an AE Title, so this aliases the code value
    // to it - the same vendor aliasing the search parser applies. AIW-I
    // 4.81.4.1.2.1 keys a station-oriented search on exactly this code value.
    FilterKey {
        wire: &[
            "00404025.00080100",
            "scheduledstationnamecodesequence.codevalue",
        ],
        field: FilterField::StationAet,
    },
];

/// A rendered subscription response: status, content type, body.
pub struct SubscribeResp {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Parse and validate a subscribe request body, then register it.
///
/// `subscriber` is the `{subscriber}` URI template variable (the AE Title).
/// The body carries the notification endpoint; for the filtered form it may
/// also carry a `filter` object with the supported matching key.
///
/// `query` is the raw query string of the POST, from which the standard's
/// `deletionlock` parameter is read (PS3.18 11.10: "If present with a value
/// of true the Subscription will be created with a Deletion Lock").
pub fn subscribe(subscriber: &str, body: &[u8], filtered: bool, query: &str) -> SubscribeResp {
    subscribe_scoped(subscriber, body, filtered, None, query)
}

/// Register a subscription, optionally scoped to ONE workitem.
///
/// `workitem_uid` is `Some` only for PS3.18 11.10.1.1's Workitem Subscription
/// template (`/workitems/{workitem}/subscribers/{subscriber}`); the Worklist
/// and Filtered Worklist forms pass `None` and are unchanged.
pub fn subscribe_scoped(
    subscriber: &str,
    body: &[u8],
    filtered: bool,
    workitem_uid: Option<&str>,
    query: &str,
) -> SubscribeResp {
    if subscriber.is_empty() {
        return bad_request("Empty subscriber AE Title");
    }
    // The AE Title becomes part of a URI path segment; a slash or a control
    // character would corrupt the route, so refuse rather than escape.
    if !subscriber
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return bad_request("Subscriber AE Title must be A-Z, a-z, 0-9, '_', '-' or '.'");
    }

    // The Deletion Lock is read BEFORE the body so a malformed parameter is
    // refused without registering anything. An ABSENT parameter is the
    // standard's optional case and means "no lock"; a present one must be
    // exactly true or false, and anything else is a 400 like every other
    // invalid parameter this handler refuses.
    let deletion_lock = match parse_deletion_lock(query) {
        Ok(lock) => lock,
        Err(msg) => return bad_request(&msg),
    };

    // PS3.18 11.10 types the request payload as "none", so an ABSENT body is
    // the standard's own request, not a malformed one. An empty object stands
    // in for it: the endpoint is optional and there is nothing else to parse.
    let obj = if body.iter().all(|b| b.is_ascii_whitespace()) {
        JsonVal::Obj(Vec::new())
    } else {
        match parse_json(body) {
            Some(o @ JsonVal::Obj(_)) => o,
            _ => return bad_request("Body must be a DICOMweb JSON object"),
        }
    };

    // The endpoint is OPTIONAL (PS3.18 11.10's URI template carries it in the
    // request body, and a subscriber that opens a WebSocket Notification
    // Connection has no need for the origin to reach it inbound). Absent or
    // empty means CHANNEL-ONLY: the push fan-out skips this subscription and
    // its Event Reports arrive over its open channel.
    //
    // Present is still validated exactly as before: a URL that was never
    // valid is a configuration error the caller can fix immediately, whereas
    // one that becomes unreachable later is a push problem.
    let endpoint = match obj.obj_get("endpoint").and_then(|v| v.as_str()) {
        Some(e) if !e.trim().is_empty() => {
            if let Err(e) = crate::push::workitem_path(e, "1") {
                return bad_request(&e);
            }
            Some(e.to_string())
        }
        // Explicitly empty reads as "no endpoint" rather than a malformed
        // one: an operator clearing the field is asking for exactly what an
        // absent field asks for.
        _ => None,
    };

    // PS3.18 Table 11.10.1-2 ties `filter` to the Filtered Worklist resource.
    // On the plain Worklist resource it is not a standard parameter, so it is
    // REFUSED rather than ignored: silently dropping it hands the subscriber
    // every workitem's events while it believes it asked for a subset, which
    // is worse than a clear error.
    //
    // The parameter NAME is matched case-insensitively, as every HTTP query
    // parameter name is: `Filter=` spelling the same parameter differently
    // slipped past an exact comparison and registered exactly the unfiltered
    // subscription this guard exists to refuse.
    // The parameter is also legal as a BODY member on the Filtered resource
    // (11.11), so the plain resource must refuse it there too: checking only
    // the query string let `{"filter":{...}}` register an UNFILTERED
    // subscription while the caller believed it had asked for a subset - and
    // it answered 201, so nothing signalled the mismatch.
    if !filtered
        && (obj.obj_get("filter").is_some()
            || query.split('&').any(|kv| {
                let key = kv.split_once('=').map_or(kv, |(k, _)| k);
                key.eq_ignore_ascii_case("filter")
            }))
    {
        return bad_request("filter is only valid on the Filtered Worklist Subscription resource");
    }

    let filter = if filtered {
        match parse_filter(query, &obj) {
            Ok(m) => m,
            Err(msg) => return bad_request(&msg),
        }
    } else {
        WorklistFilter::default()
    };

    subscriptions().subscribe(Subscription {
        subscriber: subscriber.to_string(),
        endpoint,
        step_label: filter.step_label.clone(),
        code_value: filter.code_value.clone(),
        code_scheme: filter.code_scheme.clone(),
        deletion_lock_at: deletion_lock.then(now_secs),
        workitem_uid: workitem_uid.map(str::to_string),
        study_uid: filter.study_uid.clone(),
        patient_id: filter.patient_id.clone(),
        workflow_state: filter.workflow_state.clone(),
        input_readiness: filter.input_readiness.clone(),
        sps_priority: filter.sps_priority.clone(),
        sps_start_datetime: filter.sps_start_datetime.clone(),
        patient_name: filter.patient_name.clone(),
        patient_id_issuer: filter.patient_id_issuer.clone(),
        accession_number: filter.accession_number.clone(),
        requested_procedure_id: filter.requested_procedure_id.clone(),
        station_aet: filter.station_aet.clone(),
    });
    crate::clog_info!(
        subscriber = %subscriber,
        filtered = filtered,
        workitem = %workitem_uid.unwrap_or("<worklist>"),
        step_label = %filter.step_label.as_deref().unwrap_or("<any>"),
        code_value = %filter.code_value.as_deref().unwrap_or("<any>"),
        code_scheme = %filter.code_scheme.as_deref().unwrap_or("<any>"),
        deletion_lock = deletion_lock,
        "UPS subscription registered"
    );

    SubscribeResp {
        status: 201,
        body: subscription_json(subscriber, filtered, &filter, deletion_lock),
    }
}

/// Remove a subscription. Idempotent from the client's point of view: an
/// unknown subscriber is a 204 as well, because the caller's intent (this
/// subscriber is not subscribed) holds either way.
pub fn unsubscribe(subscriber: &str) -> SubscribeResp {
    if subscriber.is_empty() {
        return bad_request("Empty subscriber AE Title");
    }
    let removed = subscriptions().unsubscribe(subscriber);
    crate::clog_info!(subscriber = %subscriber, removed = removed, "UPS unsubscribe");
    SubscribeResp {
        status: 204,
        body: Vec::new(),
    }
}

/// Remove one scope of `subscriber`, leaving its other subscriptions alone.
///
/// The Workitem Subscription template addresses one workitem, so its DELETE
/// removes that scope only. The whole-AE form ([`unsubscribe`]) would also drop
/// the subscriber's Worklist subscription, which the requester never named.
pub fn unsubscribe_scoped(subscriber: &str, workitem_uid: &str) -> SubscribeResp {
    if subscriber.is_empty() {
        return bad_request("Empty subscriber AE Title");
    }
    let removed = subscriptions().unsubscribe_scoped(subscriber, Some(workitem_uid));
    crate::clog_info!(
        subscriber = %subscriber,
        workitem = %workitem_uid,
        removed = removed,
        "UPS unsubscribe (scoped)"
    );
    SubscribeResp {
        status: 204,
        body: Vec::new(),
    }
}

/// One parsed filter: every key is optional, and only the keys present
/// constrain the match.
///
/// A flat struct of `Option<String>`, not a map: the key set is fixed and each
/// field has exactly one meaning, so the matcher is one comparison per key
/// with no lookup and no `String` built on the match path (the ap101 rule).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorklistFilter {
    /// (0020,000D) Study Instance UID, either spelling.
    pub study_uid: Option<String>,
    /// (0010,0020) Patient ID.
    pub patient_id: Option<String>,
    /// (0074,1204) Procedure Step Label.
    pub step_label: Option<String>,
    /// (0040,4018) > (0008,0100) Code Value.
    pub code_value: Option<String>,
    /// (0040,4018) > (0008,0102) Coding Scheme Designator.
    pub code_scheme: Option<String>,
    /// (0074,1000) Procedure Step State.
    pub workflow_state: Option<String>,
    /// (0040,4041) Input Readiness State.
    pub input_readiness: Option<String>,
    /// (0074,1200) Scheduled Procedure Step Priority.
    pub sps_priority: Option<String>,
    /// (0040,4005) Scheduled Procedure Step Start DateTime.
    pub sps_start_datetime: Option<String>,
    /// (0010,0010) Patient's Name.
    pub patient_name: Option<String>,
    /// (0010,0021) Issuer of Patient ID.
    pub patient_id_issuer: Option<String>,
    /// (0008,0050) Accession Number.
    pub accession_number: Option<String>,
    /// (0040,1001) Requested Procedure ID.
    pub requested_procedure_id: Option<String>,
    /// (0040,4025) > (0008,0100) station code value, aliased to the AE Title.
    pub station_aet: Option<String>,
}

impl WorklistFilter {
    /// Whether the filter constrains nothing (match every workitem).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        // The struct is all-`Option` and derives `PartialEq`, so "no key is
        // set" IS "equal to the default". Spelling out fourteen `is_none()`
        // checks would be a second copy of the field list that could go stale
        // the moment a key is added.
        self == &Self::default()
    }

    /// The slot for one field (the read side).
    fn slot(&self, field: FilterField) -> &Option<String> {
        match field {
            FilterField::StudyUid => &self.study_uid,
            FilterField::PatientId => &self.patient_id,
            FilterField::StepLabel => &self.step_label,
            FilterField::CodeValue => &self.code_value,
            FilterField::CodeScheme => &self.code_scheme,
            FilterField::WorkflowState => &self.workflow_state,
            FilterField::InputReadiness => &self.input_readiness,
            FilterField::SpsPriority => &self.sps_priority,
            FilterField::SpsStartDatetime => &self.sps_start_datetime,
            FilterField::PatientName => &self.patient_name,
            FilterField::PatientIdIssuer => &self.patient_id_issuer,
            FilterField::AccessionNumber => &self.accession_number,
            FilterField::RequestedProcedureId => &self.requested_procedure_id,
            FilterField::StationAet => &self.station_aet,
        }
    }

    /// The mutable slot for one field (the parser's write side).
    fn slot_mut(&mut self, field: FilterField) -> &mut Option<String> {
        match field {
            FilterField::StudyUid => &mut self.study_uid,
            FilterField::PatientId => &mut self.patient_id,
            FilterField::StepLabel => &mut self.step_label,
            FilterField::CodeValue => &mut self.code_value,
            FilterField::CodeScheme => &mut self.code_scheme,
            FilterField::WorkflowState => &mut self.workflow_state,
            FilterField::InputReadiness => &mut self.input_readiness,
            FilterField::SpsPriority => &mut self.sps_priority,
            FilterField::SpsStartDatetime => &mut self.sps_start_datetime,
            FilterField::PatientName => &mut self.patient_name,
            FilterField::PatientIdIssuer => &mut self.patient_id_issuer,
            FilterField::AccessionNumber => &mut self.accession_number,
            FilterField::RequestedProcedureId => &mut self.requested_procedure_id,
            FilterField::StationAet => &mut self.station_aet,
        }
    }
}

impl FilterKey {
    /// The key one wire spelling names, or an error naming the spelling.
    ///
    /// The refusal message lists what IS supported rather than only what is
    /// not: an operator who sent `WorklistLabel` needs to know whether the
    /// mistake was the spelling or the key, and a list answers that in one
    /// read.
    fn from_wire(key: &str) -> Result<FilterField, String> {
        let lower = key.to_ascii_lowercase();
        for entry in FILTER_KEYS {
            if entry.wire.iter().any(|w| *w == lower) {
                return Ok(entry.field);
            }
        }
        Err(format!(
            "Unsupported filter key '{key}': {}",
            supported_keys()
        ))
    }
}

/// The supported key set, rendered for an error message.
fn supported_keys() -> String {
    // Built once per ERROR, never per event: this is off every match path the
    // ap101 rule covers.
    let mut out = String::from("this server supports Study Instance UID (0020,000D or ");
    out.push_str("0040A370.0020000D), Patient ID (0010,0020), Procedure Step Label ");
    out.push_str("(0074,1204), the Scheduled Workitem Code Sequence keys ");
    out.push_str("(00404018.00080100, 00404018.00080102), Procedure Step State ");
    out.push_str("(0074,1000), Input Readiness State (0040,4041), Scheduled Procedure ");
    out.push_str("Step Priority (0074,1200), Scheduled Procedure Step Start DateTime ");
    out.push_str("(0040,4005), Patient Name (0010,0010), Issuer of Patient ID ");
    out.push_str("(0010,0021), Accession Number (0008,0050), Requested Procedure ID ");
    out.push_str("(0040,1001) and Scheduled Station Name Code Sequence Code Value ");
    out.push_str("(00404025.00080100)");
    out
}

/// Extract the filter from a filtered-subscription request.
///
/// The standard form is the `filter` QUERY parameter (PS3.18 11.10.1.1: the
/// Filtered Worklist Subscription resource is selected by the URI template,
/// and 11.10.1.4 makes the subscription payload "no payload"). The keys
/// follow 8.3.4.1: a matching attribute is named by its tag, and an attribute
/// INSIDE a sequence is named by the sequence tag and the inner tag joined
/// with a dot.
///
/// ```text
/// POST .../workitems/1.2.840.10008.5.1.4.34.5.1/subscribers/CLINFER
///      ?filter=00404018.00080102=99CLARUS
///
/// POST ...?filter=00741204=CAD&filter=00404018.00080100=CXR-AI
/// ```
///
/// The **body** form (`{"filter":{...}}`) is also accepted: it is what this
/// server implemented before the query form, it costs one extra parse to
/// keep, and a subscriber already deployed keeps working. When BOTH are
/// present the query wins, because that is the form the standard names and
/// the one a conformant client sends.
fn parse_filter(query: &str, obj: &JsonVal) -> Result<WorklistFilter, String> {
    let mut filter = parse_filter_query(query)?;
    if filter.is_empty() {
        filter = parse_filter_body(obj)?;
    }
    Ok(filter)
}

/// The filter as query parameters (PS3.18 8.3.4.1 matching keys).
fn parse_filter_query(query: &str) -> Result<WorklistFilter, String> {
    let mut filter = WorklistFilter::default();
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if !key.eq_ignore_ascii_case("filter") {
            continue;
        }
        // Decode the WHOLE value before splitting it. A conformant user agent
        // percent-encodes the matching-key separator, because `=` is a reserved
        // delimiter inside a query value (RFC 3986 2.2) - Weasis's Java
        // `URLEncoder` does exactly that, sending
        // `filter=ReferencedRequestSequence.StudyInstanceUID%3D<uid>`
        // (PS3.18 8.3.4.1 matching keys). Splitting the RAW value looked for an
        // `=` character that was never on the wire and answered 400 to a
        // correct request.
        //
        // Splitting after the decode also means the TAG reaches
        // `FilterKey::from_wire` in its decoded form, which is what it needs: a
        // key written `ReferencedRequestSequence%2EStudyInstanceUID` must
        // resolve like the literal spelling. The pattern is therefore already
        // decoded and gets no second pass - a second decode would corrupt a
        // value that legitimately contains `%` (`A%2520B` means `A%20B`, not
        // `A B`).
        let value = percent_decode(value);
        let Some((tag, pattern)) = value.split_once('=') else {
            return Err(format!(
                "Invalid filter '{value}': expected a matching key like '00741204=VALUE'"
            ));
        };
        // Repeated keys are legal in the standard's grammar and are combined
        // below; a repeated TAG is not - PS3.18 8.3.4.1: "Each attribute
        // shall not be repeated". Accepting two values for one key would make
        // the subscription's meaning depend on a parse order nobody
        // documented.
        let field = FilterKey::from_wire(tag)?;
        if filter.slot(field).is_some() {
            return Err(format!(
                "Filter key '{tag}' is repeated: each matching attribute shall appear once \
                 (PS3.18 8.3.4.1)"
            ));
        }
        if pattern.trim().is_empty() {
            return Err(format!("Filter key '{tag}' must carry a non-empty value"));
        }
        *filter.slot_mut(field) = Some(pattern.to_string());
    }
    Ok(filter)
}

/// Percent-decode one query value (the `+`-as-space form PS3.18 uses for
/// matching keys, the same subset the server uses elsewhere).
///
/// A malformed escape is left as written rather than refused: the value is a
/// matching key, so passing it through unchanged means it matches nothing -
/// which is the honest outcome for a key that cannot be decoded.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
            {
                (Some(hi), Some(lo)) => {
                    out.push(hi * 16 + lo);
                    i += 3;
                }
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The legacy filter body, kept for subscribers deployed before the query
/// form existed.
fn parse_filter_body(obj: &JsonVal) -> Result<WorklistFilter, String> {
    let Some(filter) = obj.obj_get("filter") else {
        return Ok(WorklistFilter::default());
    };
    let JsonVal::Obj(entries) = filter else {
        return Err("Field 'filter' must be a JSON object".to_string());
    };
    let mut out = WorklistFilter::default();
    for (key, value) in entries {
        let field = FilterKey::from_wire(key)?;
        // The key may carry the DICOMweb value shape or a bare string.
        let text = match value {
            JsonVal::Str(s) => Some(s.clone()),
            v => v
                .obj_get("Value")
                .and_then(|x| x.as_str_array_first())
                .map(str::to_string),
        };
        match text {
            Some(m) if !m.trim().is_empty() => *out.slot_mut(field) = Some(m),
            _ => return Err(format!("Filter key {key} must carry a non-empty value")),
        }
    }
    Ok(out)
}

/// Read the standard's `deletionlock` query parameter (PS3.18 11.10).
///
/// The parameter's syntax is fixed by PS3.18 Table 11.10.1-2:
///
/// ```text
/// deletionlock = "deletionlock=" true / false
/// ```
///
/// Returns `Ok(false)` when the parameter is ABSENT - the standard marks it
/// optional (the `{?deletionlock}` URI template), and an absent lock is a
/// subscription without one. A present parameter must be exactly `true` or
/// `false`; anything else is refused rather than treated as false, because
/// silently dropping a lock the subscriber asked for would leave it believing
/// the workitem is protected when it is not.
///
/// The comparison is case-insensitive: PS3.18 query-parameter VALUES carry
/// the `true`/`false` lexical forms, and a client sending `True` means the
/// same thing. The KEY is matched case-insensitively too, the usual
/// query-parameter convention this server already uses for `limit`.
/// Read the Deletion Lock out of a query string, for the allocation gate.
///
/// Thin wrapper over the parse the handler uses, exposed so the ap101 binary
/// can measure the real entry point rather than a copy of it.
pub fn deletion_lock_for_query(query: &str) -> Result<bool, String> {
    parse_deletion_lock(query)
}

/// Parse the optional `deletionlock` query parameter (PS3.18 11.10).
///
/// `absent -> Ok(false)`: the parameter is optional - the standard's own
/// template is `{?deletionlock}` - so a request without it asks for an
/// ordinary subscription. Any other value is refused rather than coerced to
/// `false`, because a subscriber that asked for a lock and silently did not
/// get one would believe the workitem is protected when it is not.
fn parse_deletion_lock(query: &str) -> Result<bool, String> {
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if !key.eq_ignore_ascii_case("deletionlock") {
            continue;
        }
        return match value.trim() {
            v if v.eq_ignore_ascii_case("true") => Ok(true),
            v if v.eq_ignore_ascii_case("false") => Ok(false),
            other => Err(format!(
                "Invalid deletionlock value '{other}': expected true or false"
            )),
        };
    }
    Ok(false)
}

/// Seconds since the Unix epoch, for dating a Deletion Lock.
///
/// A clock before 1970 yields `0`, which reads as a lock taken at the epoch -
/// i.e. one that the retention sweep will consider expired. That is the
/// permissive direction on a broken clock, and the retention sweep is a
/// background policy rather than a safety guarantee.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The canonical spelling echoed back for each accepted field.
///
/// One row per field, so the response cannot silently omit a key the
/// subscription actually carries - which is what happened while this list was
/// three hardcoded pairs next to a fourteen-key parser.
const FILTER_ECHO: &[(FilterField, &str)] = &[
    (FilterField::StudyUid, "0020000D"),
    (FilterField::PatientId, "00100020"),
    (FilterField::StepLabel, "00741204"),
    (FilterField::CodeValue, "00404018.00080100"),
    (FilterField::CodeScheme, "00404018.00080102"),
    (FilterField::WorkflowState, "00741000"),
    (FilterField::InputReadiness, "00404041"),
    (FilterField::SpsPriority, "00741200"),
    (FilterField::SpsStartDatetime, "00404005"),
    (FilterField::PatientName, "00100010"),
    (FilterField::PatientIdIssuer, "00100021"),
    (FilterField::AccessionNumber, "00080050"),
    (FilterField::RequestedProcedureId, "00401001"),
    (FilterField::StationAet, "00404025.00080100"),
];

/// The 201 response body: what was registered, so the caller can verify it.
fn subscription_json(
    subscriber: &str,
    filtered: bool,
    filter: &WorklistFilter,
    deletion_lock: bool,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(200);
    body.push(b'{');
    body.extend_from_slice(b"\"subscriber\":");
    crate::handlers::json_common::push_json_str_loose(&mut body, subscriber);
    body.extend_from_slice(b",\"filtered\":");
    body.extend_from_slice(if filtered { b"true" } else { b"false" });
    // The lock is echoed because the subscriber has no other way to read it
    // back: this server exposes no GET for a subscription, and PS3.4 CC.2.3.1
    // makes the state (Subscribed with/without Lock) the subscriber's own
    // record of what it asked for.
    body.extend_from_slice(b",\"deletionlock\":");
    body.extend_from_slice(if deletion_lock { b"true" } else { b"false" });
    // The filter comes back in the QUERY syntax it was accepted in
    // (PS3.18 8.3.4.1 matching keys), so a caller can copy it straight into
    // the re-subscribe request. The hex form is echoed for every key: it is
    // unambiguous, whereas a request may have used the keyword spelling.
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    for (field, hex) in FILTER_ECHO {
        if let Some(value) = filter.slot(*field).as_deref() {
            pairs.push((hex, value));
        }
    }
    if !pairs.is_empty() {
        // A JSON array of `tag=value` STRINGS, not object keys: the response
        // echoes the filter in the QUERY syntax it was accepted in, so a
        // caller can copy an element straight into a re-subscribe request.
        body.extend_from_slice(b",\"filter\":[");
        for (index, (tag, value)) in pairs.iter().enumerate() {
            if index > 0 {
                body.push(b',');
            }
            let mut pair = String::with_capacity(tag.len() + value.len() + 1);
            pair.push_str(tag);
            pair.push('=');
            pair.push_str(value);
            crate::handlers::json_common::push_json_str_loose(&mut body, &pair);
        }
        body.push(b']');
    }
    body.push(b'}');
    body
}

fn bad_request(msg: &str) -> SubscribeResp {
    SubscribeResp {
        status: 400,
        body: err_msg_json("Bad Request", msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_json(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    /// The match input for a test that only cares about the step label.
    ///
    /// The other keys are absent, which for a subscription that does not
    /// carry them constrains nothing (PS3.18 8.3.4.1: every PRESENT key must
    /// hold) - so a label-only fixture answers exactly what the old
    /// `accepts_for(sub, uid, label)` signature did.
    fn label(text: &str) -> crate::push::FilterValues<'_> {
        crate::push::FilterValues {
            step_label: text,
            ..Default::default()
        }
    }

    // -- Workitem Subscription template (PS3.18 11.10.1.1) ----------------

    /// Query parameter NAMES are case-insensitive. `Filter=` on the plain
    /// Worklist resource is the same refusal as `filter=`: an exact match let
    /// the different spelling through and registered exactly the unfiltered
    /// subscription the guard exists to refuse.
    #[test]
    fn a_filter_parameter_is_refused_whatever_its_case() {
        for query in [
            "filter=00741204=CAD",
            "Filter=00741204=CAD",
            "FILTER=00741204=CAD",
            "fIlTeR=00741204=CAD",
        ] {
            let resp = subscribe(
                "SUB_AE",
                br#"{"00741204":{"Value":[{"00080100":{"Value":["CAD"]}}]}}"#,
                false,
                query,
            );
            assert_eq!(
                resp.status, 400,
                "query {query:?} must be refused on the plain Worklist resource"
            );
        }
    }

    /// A parameter that merely CONTAINS the word is not the parameter.
    #[test]
    fn a_differently_named_parameter_is_not_refused() {
        let resp = subscribe(
            "SUB_AE",
            br#"{"00741204":{"Value":[{"00080100":{"Value":["CAD"]}}]}}"#,
            false,
            "filters=1",
        );
        assert_ne!(
            resp.status, 400,
            "only the parameter named `filter` is reserved"
        );
    }
    /// A workitem-scoped subscription stores the scope and nothing else.
    #[test]
    fn a_workitem_subscription_records_its_scope() {
        let r = subscribe_scoped("WISUB", &body_json("{}"), false, Some("1.2.3.4.5"), "");
        assert_eq!(r.status, 201);
        let subs = subscriptions().snapshot();
        let sub = subs
            .iter()
            .find(|s| s.subscriber == "WISUB")
            .expect("registered");
        assert_eq!(sub.workitem_uid.as_deref(), Some("1.2.3.4.5"));
        subscriptions().unsubscribe("WISUB");
    }

    /// The scope is a real filter: an event for another workitem is refused,
    /// an event for the scoped one is accepted.
    #[test]
    fn a_workitem_subscription_matches_only_its_own_workitem() {
        let _ = subscriptions().unsubscribe("WISUB2");
        subscribe_scoped("WISUB2", &body_json("{}"), false, Some("1.2.3.4.5"), "");
        let reg = subscriptions();
        assert!(
            reg.accepts_for("WISUB2", "1.2.3.4.5", &label("CAD")),
            "the scoped workitem matches"
        );
        assert!(
            !reg.accepts_for("WISUB2", "1.2.3.4.6", &label("CAD")),
            "a different workitem must not match a workitem subscription"
        );
        // The Worklist form is unaffected: an unscoped subscription still
        // matches any workitem.
        let _ = subscriptions().unsubscribe("WLSUB");
        subscribe("WLSUB", &body_json("{}"), false, "");
        assert!(reg.accepts_for("WLSUB", "9.9.9", &label("CAD")));
        assert!(reg.accepts_for("WLSUB", "1.2.3.4.5", &label("CAD")));
        let _ = subscriptions().unsubscribe("WISUB2");
        let _ = subscriptions().unsubscribe("WLSUB");
    }

    /// A scope and a filter together still both apply.
    #[test]
    fn a_scoped_subscription_still_honours_its_other_keys() {
        let _ = subscriptions().unsubscribe("WISUB3");
        subscribe_scoped(
            "WISUB3",
            &body_json("{}"),
            true,
            Some("1.2.3.4.5"),
            "filter=00741204=CAD",
        );
        let reg = subscriptions();
        assert!(reg.accepts_for("WISUB3", "1.2.3.4.5", &label("CAD")));
        assert!(
            !reg.accepts_for("WISUB3", "1.2.3.4.5", &label("MR")),
            "the step label still narrows a scoped subscription"
        );
        let _ = subscriptions().unsubscribe("WISUB3");
    }

    // -- T3: `filter` on the plain Worklist resource ----------------------

    /// PS3.18 Table 11.10.1-2 ties `filter` to the Filtered Worklist resource.
    /// Accepting it on the plain resource would register a subscriber that
    /// receives everything while believing it asked for a subset.
    #[test]
    fn filter_on_the_plain_worklist_resource_is_refused() {
        // Scoped to THIS subscriber, not to the registry length. The registry
        // is process-wide and the suite runs tests in parallel, so
        // `len()` before and after measures every other test's registrations
        // as well - which is exactly how this test used to fail intermittently
        // while the code under it was correct.
        const AE: &str = "PLAINFILT";
        let _ = subscriptions().unsubscribe(AE);
        let r = subscribe(AE, &body_json("{}"), false, "filter=00741204=CAD");
        assert_eq!(
            r.status, 400,
            "a filter on the plain resource must be refused, not ignored"
        );
        let text = String::from_utf8(r.body).unwrap();
        assert!(
            text.contains("Filtered Worklist Subscription resource"),
            "the message must name the resource that accepts it: {text}"
        );
        assert!(
            !subscriptions()
                .snapshot()
                .iter()
                .any(|s| s.subscriber.eq_ignore_ascii_case(AE)),
            "nothing may be registered by a refused request"
        );
    }

    /// The same resource WITHOUT a filter still registers - the refusal is
    /// about the parameter, not the route.
    #[test]
    fn plain_worklist_without_a_filter_still_registers() {
        let _ = subscriptions().unsubscribe("PLAINOK");
        let r = subscribe("PLAINOK", &body_json("{}"), false, "");
        assert_eq!(r.status, 201);
        let _ = subscriptions().unsubscribe("PLAINOK");
    }

    /// The Filtered resource keeps accepting `filter`, so the refusal above
    /// does not spill over into the standard form.
    #[test]
    fn filter_on_the_filtered_resource_is_still_accepted() {
        let _ = subscriptions().unsubscribe("FILTEDOK");
        let r = subscribe("FILTEDOK", &body_json("{}"), true, "filter=00741204=CAD");
        assert_eq!(r.status, 201);
        let _ = subscriptions().unsubscribe("FILTEDOK");
    }

    #[test]
    fn unfiltered_subscription_registers() {
        let r = subscribe(
            "CLINFER",
            &body_json("{\"endpoint\":\"http://127.0.0.1:8124/ups\"}"),
            false,
            "",
        );
        assert_eq!(r.status, 201);
        let text = String::from_utf8(r.body).unwrap();
        assert!(text.contains("\"subscriber\":\"CLINFER\""), "{text}");
        assert!(text.contains("\"filtered\":false"), "{text}");
        subscriptions().unsubscribe("CLINFER");
    }

    #[test]
    fn filtered_subscription_carries_the_step_label_from_the_body() {
        // The LEGACY form: the filter in the body. Kept working for a
        // subscriber deployed before the query form existed.
        let r = subscribe(
            "CADSUB",
            &body_json(
                "{\"endpoint\":\"http://127.0.0.1:8124/ups\",\"filter\":{\"00741204\":\"CAD\"}}",
            ),
            true,
            "",
        );
        assert_eq!(r.status, 201);
        let text = String::from_utf8(r.body).unwrap();
        assert!(text.contains("\"filtered\":true"), "{text}");
        assert!(text.contains("00741204=CAD"), "{text}");
        subscriptions().unsubscribe("CADSUB");
    }

    /// The STANDARD form: the filter is a `filter=` QUERY parameter, in the
    /// matching-key syntax of PS3.18 8.3.4.1.
    #[test]
    fn filtered_subscription_reads_the_query_form() {
        let r = subscribe("QSUB", &body_json("{}"), true, "filter=00741204=CAD");
        assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
        let text = String::from_utf8(r.body).unwrap();
        assert!(text.contains("00741204=CAD"), "{text}");
        subscriptions().unsubscribe("QSUB");
    }

    /// The nested-sequence keys the code filter uses, in the query form, and
    /// BOTH of them together.
    #[test]
    fn the_code_filter_keys_are_accepted_in_the_query() {
        let r = subscribe(
            "CODESUB",
            &body_json("{}"),
            true,
            "filter=00404018.00080102=99CLARUS",
        );
        assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
        let text = String::from_utf8(r.body).unwrap();
        assert!(text.contains("00404018.00080102=99CLARUS"), "{text}");

        let r = subscribe(
            "CODESUB2",
            &body_json("{}"),
            true,
            "filter=00404018.00080100=CXR-AI&filter=00404018.00080102=99CLARUS",
        );
        assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
        let text = String::from_utf8(r.body).unwrap();
        assert!(text.contains("00404018.00080100=CXR-AI"), "{text}");
        assert!(text.contains("00404018.00080102=99CLARUS"), "{text}");

        subscriptions().unsubscribe("CODESUB");
        subscriptions().unsubscribe("CODESUB2");
    }

    /// RED FACT. The filter keys are accepted in their PS3.6 KEYWORD spellings,
    /// which is what PS3.18 8.3.4.1 defines ("See Section 8.3.4.1 for the
    /// syntax of matching keys") and what a conformant subscriber sends.
    ///
    /// Before this a keyword key was an UNKNOWN key, so the subscription was
    /// refused with 400 and the client could not subscribe at all.
    #[test]
    fn the_keyword_filter_keys_are_accepted() {
        // Query form: all three keywords.
        let r = subscribe(
            "KWSUB",
            &body_json("{}"),
            true,
            "filter=ProcedureStepLabel=CAD\
             &filter=ScheduledWorkitemCodeSequence.CodeValue=CXR-AI\
             &filter=ScheduledWorkitemCodeSequence.CodingSchemeDesignator=99CLARUS",
        );
        assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
        let text = String::from_utf8(r.body).unwrap();
        assert!(text.contains("00741204=CAD"), "{text}");
        assert!(text.contains("00404018.00080100=CXR-AI"), "{text}");
        assert!(text.contains("00404018.00080102=99CLARUS"), "{text}");

        // Body form: the same three keywords.
        let body = body_json(
            r#"{"filter":{"ProcedureStepLabel":"CAD","ScheduledWorkitemCodeSequence.CodeValue":"CXR-AI"}}"#,
        );
        let r = subscribe("KWSUB2", &body, true, "");
        assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
        let text = String::from_utf8(r.body).unwrap();
        assert!(text.contains("00741204=CAD"), "{text}");
        assert!(text.contains("00404018.00080100=CXR-AI"), "{text}");

        // Keyword and hex name the same filter, so both are accepted and the
        // response renders the SAME filter list (the subscriber name differs,
        // so only the filter part is compared).
        let hex = subscribe("KWSUB3", &body_json("{}"), true, "filter=00741204=CAD");
        assert_eq!(hex.status, 201);
        let kw = subscribe(
            "KWSUB4",
            &body_json("{}"),
            true,
            "filter=ProcedureStepLabel=CAD",
        );
        assert_eq!(kw.status, 201);
        let filter_of = |body: Vec<u8>| {
            let text = String::from_utf8(body).unwrap();
            let start = text.find("\"filter\":").expect("a filter in the response");
            text[start..].to_string()
        };
        assert_eq!(
            filter_of(hex.body),
            filter_of(kw.body),
            "the keyword filter renders identically to its hex twin"
        );

        subscriptions().unsubscribe("KWSUB");
        subscriptions().unsubscribe("KWSUB2");
        subscriptions().unsubscribe("KWSUB3");
        subscriptions().unsubscribe("KWSUB4");
    }

    /// An unsupported key is STILL refused with 400, keyword or not: the
    /// accepted set is closed, and a typo must not become a silent no-filter.
    ///
    /// `WorklistLabel` is the right probe: it IS a PS3.4 CC.2.5-3 matching key
    /// (R), so it is a legal filter key by PS3.18 11.10.1.2 - and it is still
    /// refused here, because the workitem record does not store it. Accepting
    /// it and matching nothing would hand a subscriber every workitem's events
    /// while it believed it had asked for a subset.
    #[test]
    fn an_unsupported_key_is_still_refused() {
        for key in ["00741202", "WorklistLabel", "ProcedureStepLabelz"] {
            let r = subscribe("BADKW", &body_json("{}"), true, &format!("filter={key}=X"));
            assert_eq!(r.status, 400, "{key} is refused, not ignored");
            let text = String::from_utf8(r.body).unwrap();
            assert!(text.contains("Unsupported filter key"), "{text}");
            assert!(!subscriptions().unsubscribe("BADKW"));
        }
    }

    /// PS3.18 8.3.4.1: "Each attribute shall not be repeated". Repeating a
    /// tag is refused rather than resolved by parse order.
    #[test]
    fn a_repeated_filter_key_is_refused() {
        let r = subscribe(
            "REPSUB",
            &body_json("{}"),
            true,
            "filter=00741204=CAD&filter=00741204=OTHER",
        );
        assert_eq!(r.status, 400, "a repeated matching attribute is illegal");
        let text = String::from_utf8(r.body).unwrap();
        assert!(text.contains("repeated"), "{text}");
        assert!(
            !subscriptions().unsubscribe("REPSUB"),
            "nothing was registered"
        );
    }

    /// A filter value is percent-decoded, so an unusual code arrives as
    /// written.
    #[test]
    fn a_filter_value_is_percent_decoded() {
        let r = subscribe(
            "PCTSUB",
            &body_json("{}"),
            true,
            "filter=00741204=CAD%20STEP",
        );
        assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
        let text = String::from_utf8(r.body).unwrap();
        assert!(text.contains("00741204=CAD STEP"), "decoded: {text}");
        subscriptions().unsubscribe("PCTSUB");
    }

    /// The second live-e2e defect: a conformant client encodes the matching-key
    /// SEPARATOR, and the parser split the raw value looking for a literal `=`.
    ///
    /// `=` is a reserved delimiter in a query value (RFC 3986 2.2), so Weasis's
    /// Java `URLEncoder` sends `filter=<key>%3D<uid>` (PS3.18 8.3.4.1 matching
    /// keys). Before the decode moved ahead of the split, that spelling had no
    /// raw `=` at all and was answered 400 - the correct request was refused
    /// while the un-encoded spelling worked, which is why a hand-written curl
    /// could not reproduce the field failure.
    #[test]
    fn the_matching_key_separator_may_be_percent_encoded() {
        const UID: &str = "1.2.826.0.1.3680043.8.498.1";
        // Every key the pair must accept, in both the spelling Weasis sends and
        // this server's own hex form - the encoding of the separator is a
        // transport detail, not a different key.
        let cases = [
            (
                "ENC_STUDY_SEQ",
                "ReferencedRequestSequence.StudyInstanceUID",
            ),
            ("ENC_STUDY_SEQ_HEX", "0040A370.0020000D"),
            ("ENC_STUDY_BARE", "StudyInstanceUID"),
            ("ENC_STEP_LABEL", "00741204"),
        ];
        for (subscriber, key) in cases {
            let query = format!("filter={key}%3D{UID}");
            let r = subscribe(subscriber, &body_json("{}"), true, &query);
            assert_eq!(
                r.status,
                201,
                "an encoded separator is the conformant spelling: {query} -> {}",
                String::from_utf8_lossy(&r.body)
            );
            subscriptions().unsubscribe(subscriber);
        }

        // And the decoded pair really is the key/value the raw form produces,
        // not a key with "%3D" stuck to its name.
        let encoded = subscribe(
            "ENC_SAME",
            &body_json("{}"),
            true,
            &format!("filter=StudyInstanceUID%3D{UID}"),
        );
        let raw = subscribe(
            "RAW_SAME",
            &body_json("{}"),
            true,
            &format!("filter=StudyInstanceUID={UID}"),
        );
        assert_eq!(encoded.status, 201);
        assert_eq!(raw.status, 201);
        let filter_of = |body: Vec<u8>| {
            let text = String::from_utf8(body).unwrap();
            let start = text.find("\"filter\":").expect("a filter in the response");
            text[start..].to_string()
        };
        assert_eq!(
            filter_of(encoded.body),
            filter_of(raw.body),
            "the encoded and raw separators must register the SAME filter"
        );
        subscriptions().unsubscribe("ENC_SAME");
        subscriptions().unsubscribe("RAW_SAME");
    }

    /// The decode happens ONCE, on the whole value.
    ///
    /// Decoding the pattern a second time (which the old code did, and which is
    /// the obvious way to "fix" the split) would turn a value that legitimately
    /// contains `%` into a different value: `%2520` means a literal `%20`, and a
    /// double decode would deliver a space instead. This pins the single pass.
    #[test]
    fn the_filter_value_is_decoded_exactly_once() {
        let r = subscribe("PCTONCE", &body_json("{}"), true, "filter=00741204=A%2520B");
        assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
        let text = String::from_utf8(r.body).unwrap();
        assert!(
            text.contains("00741204=A%20B"),
            "one decode leaves a literal %20, not a space: {text}"
        );
        subscriptions().unsubscribe("PCTONCE");
    }

    // -- T1/T2: the CC.2.5-3 keys the record stores ----------------------

    /// The live-e2e defect: Weasis subscribes with `StudyInstanceUID=<uid>`
    /// and this server answered 400, so the client fell back to polling.
    ///
    /// PS3.4 CC.2.5-3 types Study Instance UID a matching key (O), so both the
    /// top-level and the Referenced Request Sequence spelling must be accepted,
    /// and they name ONE field, because both say "the study this workitem is
    /// about".
    #[test]
    fn the_study_uid_is_a_filter_key_in_both_spellings() {
        for (subscriber, key) in [
            ("STUDY_TOP", "StudyInstanceUID"),
            ("STUDY_HEX", "0020000D"),
            ("STUDY_SEQ", "ReferencedRequestSequence.StudyInstanceUID"),
            ("STUDY_SEQ_HEX", "0040A370.0020000D"),
        ] {
            let r = subscribe(
                subscriber,
                &body_json("{}"),
                true,
                &format!("filter={key}=1.2.840.10008.5.1.4.1.1.2.1.1"),
            );
            assert_eq!(r.status, 201, "{key} is a valid filter key");
            subscriptions().unsubscribe(subscriber);
        }

        // One field, so a workitem whose input study matches is accepted and
        // one whose does not is refused - whichever spelling was registered.
        let r = subscribe(
            "STUDY_BEHAVIOUR",
            &body_json("{}"),
            true,
            "filter=StudyInstanceUID=1.2.3",
        );
        assert_eq!(r.status, 201);
        let hit = crate::push::FilterValues {
            study_uid: Some("1.2.3"),
            ..Default::default()
        };
        let miss = crate::push::FilterValues {
            study_uid: Some("9.9.9"),
            ..Default::default()
        };
        assert!(subscriptions().accepts_for("STUDY_BEHAVIOUR", "1.2.3", &hit));
        assert!(!subscriptions().accepts_for("STUDY_BEHAVIOUR", "1.2.3", &miss));
        subscriptions().unsubscribe("STUDY_BEHAVIOUR");
    }

    /// Every CC.2.5-3 key the record stores parses in BOTH spellings and
    /// renders in the response, so the echo cannot silently omit a key.
    #[test]
    fn every_record_backed_key_parses_in_hex_and_keyword_form() {
        let cases = [
            ("00741000", "ProcedureStepState"),
            ("00404041", "InputReadinessState"),
            ("00741200", "ScheduledProcedureStepPriority"),
            ("00404005", "ScheduledProcedureStepStartDateTime"),
            ("00100010", "PatientName"),
            ("00100021", "IssuerOfPatientID"),
            ("00080050", "AccessionNumber"),
            ("00401001", "RequestedProcedureID"),
            (
                "00404025.00080100",
                "ScheduledStationNameCodeSequence.CodeValue",
            ),
        ];
        for (hex, keyword) in cases {
            for key in [hex, keyword] {
                let r = subscribe(
                    "EVERYKEY",
                    &body_json("{}"),
                    true,
                    &format!("filter={key}=V1"),
                );
                assert_eq!(r.status, 201, "{key} must be accepted");
                let text = String::from_utf8(r.body).unwrap();
                assert!(
                    text.contains(&format!("{hex}=V1")),
                    "{key} must be echoed back as {hex}: {text}"
                );
                subscriptions().unsubscribe("EVERYKEY");
            }
        }
    }

    /// Each record-backed key narrows the match on its OWN field.
    ///
    /// The falsification target: swapping any arm in `FilterKey::from_wire`
    /// or the matcher's table makes exactly the affected case go red.
    #[test]
    fn each_record_backed_key_narrows_on_its_own_field() {
        /// One case: the wire key, the value it filters on, and the setter that
        /// plants that value on a match input.
        type Case = (
            &'static str,
            &'static str,
            fn(&mut crate::push::FilterValues<'static>),
        );
        let checks: [Case; 9] = [
            ("00741000", "SCHEDULED", |v| {
                v.workflow_state = Some("SCHEDULED")
            }),
            ("00404041", "READY", |v| v.input_readiness = Some("READY")),
            ("00741200", "HIGH", |v| v.sps_priority = Some("HIGH")),
            ("00404005", "20260101120000", |v| {
                v.sps_start_datetime = Some("20260101120000")
            }),
            ("00100010", "DOE^JOHN", |v| {
                v.patient_name = Some("DOE^JOHN")
            }),
            ("00100021", "ISSUER", |v| {
                v.patient_id_issuer = Some("ISSUER")
            }),
            ("00080050", "ACC-1", |v| v.accession_number = Some("ACC-1")),
            ("00401001", "RP-1", |v| {
                v.requested_procedure_id = Some("RP-1")
            }),
            ("00404025.00080100", "CT1", |v| v.station_aet = Some("CT1")),
        ];
        for (tag, good, set) in checks {
            let r = subscribe(
                "NARROW",
                &body_json("{}"),
                true,
                &format!("filter={tag}={good}"),
            );
            assert_eq!(r.status, 201, "{tag} must be accepted");
            let mut hit = crate::push::FilterValues::default();
            set(&mut hit);
            assert!(
                subscriptions().accepts_for("NARROW", "1.2.3", &hit),
                "{tag} must accept a workitem carrying its value"
            );
            // The same workitem with that one field absent is NOT a match: the
            // subscription asked for a value and an absent value does not
            // demonstrably carry it.
            let absent = crate::push::FilterValues::default();
            assert!(
                !subscriptions().accepts_for("NARROW", "1.2.3", &absent),
                "{tag} must refuse a workitem that carries no such value"
            );
            subscriptions().unsubscribe("NARROW");
        }
    }

    /// A filter combining keys still ANDs them (PS3.18 8.3.4.1).
    #[test]
    fn several_keys_on_one_subscription_all_have_to_hold() {
        let r = subscribe(
            "ANDED",
            &body_json("{}"),
            true,
            "filter=StudyInstanceUID=1.2.3&filter=PatientID=P1",
        );
        assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
        let both = crate::push::FilterValues {
            study_uid: Some("1.2.3"),
            patient_id: Some("P1"),
            ..Default::default()
        };
        let one_only = crate::push::FilterValues {
            study_uid: Some("1.2.3"),
            ..Default::default()
        };
        assert!(subscriptions().accepts_for("ANDED", "1.2.3", &both));
        assert!(!subscriptions().accepts_for("ANDED", "1.2.3", &one_only));
        subscriptions().unsubscribe("ANDED");
    }

    #[test]
    fn modality_is_not_a_filter_key() {
        // UPS has no Modality attribute, so a subscriber filtering on it
        // would never match anything - which is worse than an error.
        let r = subscribe(
            "MODSUB",
            &body_json(
                "{\"endpoint\":\"http://127.0.0.1:8124/ups\",\"filter\":{\"00080060\":\"CT\"}}",
            ),
            true,
            "",
        );
        assert_eq!(r.status, 400, "Modality is not a matching key");
        let text = String::from_utf8(r.body).unwrap();
        assert!(text.contains("00080060"), "{text}");
        assert!(
            !subscriptions().unsubscribe("MODSUB"),
            "the refused call may not register a subscriber"
        );
    }

    /// The patient matching keys ARE accepted now.
    ///
    /// They used to be refused on the argument that AIW-I 4.86.4.1.2 makes
    /// patient matching conditional, so accepting the keys without the
    /// matching would lie. That argument no longer applies: PS3.18 11.10.1.2
    /// makes valid filter keys the UPS IOD attributes, Patient Demographic
    /// (PS3.3 C.2.3) is one of those modules, and PS3.4 CC.2.5-3 types Patient
    /// ID (0010,0020) a REQUIRED matching key the record does store. The keys
    /// are matched for real, so they are accepted for real.
    #[test]
    fn the_patient_matching_keys_are_accepted_and_matched() {
        for key in ["00100020", "PatientID"] {
            let r = subscribe(
                "PATSUB",
                &body_json("{}"),
                true,
                &format!("filter={key}=P123"),
            );
            assert_eq!(r.status, 201, "{key} is a valid filter key");
            subscriptions().unsubscribe("PATSUB");
        }

        // And the key really narrows: only the matching patient's workitem is
        // accepted.
        let r = subscribe("PATSUB2", &body_json("{}"), true, "filter=PatientID=P123");
        assert_eq!(r.status, 201);
        let hit = crate::push::FilterValues {
            patient_id: Some("P123"),
            ..Default::default()
        };
        let miss = crate::push::FilterValues {
            patient_id: Some("P999"),
            ..Default::default()
        };
        assert!(subscriptions().accepts_for("PATSUB2", "1.2.3", &hit));
        assert!(!subscriptions().accepts_for("PATSUB2", "1.2.3", &miss));
        subscriptions().unsubscribe("PATSUB2");
    }

    #[test]
    fn a_key_the_record_does_not_store_is_refused_not_ignored() {
        // Patient Name IS stored, so it is accepted; the refusal case now has
        // to name something the record genuinely lacks. Scheduled Procedure
        // Step Expiration DateTime (0040,4008) is a CC.2.5-3 matching key (O)
        // with no record field, so it is a loud 400 rather than a filter that
        // silently matches everything.
        let r = subscribe(
            "SUB",
            &body_json(
                "{\"endpoint\":\"http://127.0.0.1:8124/ups\",\"filter\":{\"00404008\":\"20260101\"}}",
            ),
            true,
            "",
        );
        assert_eq!(r.status, 400, "an unstored key must be an honest error");
        let text = String::from_utf8(r.body).unwrap();
        assert!(
            text.contains("00404008"),
            "message must name the key: {text}"
        );
        // The registry is process-wide and shared with tests running in
        // parallel threads, so the global count is racy here. Ask the
        // registry about THIS name instead: the refused call must not have
        // registered "SUB", whoever else is subscribed right now.
        assert!(
            !subscriptions().unsubscribe("SUB"),
            "the refused call may not register a subscriber"
        );
    }

    #[test]
    fn empty_filter_matches_everything() {
        let r = subscribe(
            "EMPTY",
            &body_json("{\"endpoint\":\"http://127.0.0.1:8124/ups\",\"filter\":{}}"),
            true,
            "",
        );
        assert_eq!(r.status, 201);
        let text = String::from_utf8(r.body).unwrap();
        assert!(
            !text.contains("\"filter\""),
            "no filter means match all: {text}"
        );
        subscriptions().unsubscribe("EMPTY");
    }

    /// PS3.18 11.10 types the Subscribe request payload as "none": a body
    /// carrying nothing is the standard's own request, so an empty or absent
    /// payload must register the subscription rather than answer 400. A
    /// non-empty body that is not a JSON object is still refused.
    #[test]
    fn an_empty_body_is_accepted_and_a_non_object_is_refused() {
        for body in [&b""[..], b"{}", b"   "] {
            let r = subscribe("EMPTYBODY", body, false, "");
            assert_eq!(
                r.status,
                201,
                "an empty payload must register the subscription, got {} ({})",
                r.status,
                String::from_utf8_lossy(&r.body)
            );
            let _ = subscriptions().unsubscribe("EMPTYBODY");
        }
        // A malformed non-object body is still a client error.
        assert_eq!(
            subscribe("EMPTYBODY", b"[]", false, "").status,
            400,
            "a JSON array is not a DICOMweb JSON object"
        );
        assert_eq!(
            subscribe("EMPTYBODY", b"not json", false, "").status,
            400,
            "a non-JSON body must be refused"
        );
    }

    #[test]
    fn invalid_endpoint_is_refused_but_absent_is_accepted() {
        // S1: an ABSENT endpoint is a channel-only subscription, not an error
        // (PS3.18 11.10 makes the notification URI optional; a subscriber
        // that opens a WebSocket channel does not need one).
        let no_endpoint = subscribe("S", &body_json("{}"), false, "");
        assert_eq!(no_endpoint.status, 201, "an absent endpoint is accepted");
        assert!(
            String::from_utf8_lossy(&no_endpoint.body).contains("\"endpoint\":null")
                || !String::from_utf8_lossy(&no_endpoint.body).contains("http"),
            "the body must not invent an endpoint: {}",
            String::from_utf8_lossy(&no_endpoint.body)
        );
        let _ = subscriptions().unsubscribe("S");

        // An explicitly EMPTY endpoint reads the same way: the operator asked
        // for no push URI, which is exactly what an absent field asks for.
        let empty_endpoint = subscribe("S", &body_json("{\"endpoint\":\"\"}"), false, "");
        assert_eq!(empty_endpoint.status, 201, "an empty endpoint is accepted");
        let _ = subscriptions().unsubscribe("S");

        // A PRESENT endpoint is still validated: this one was never valid and
        // the caller can fix it immediately.
        assert_eq!(
            subscribe("S", &body_json("{\"endpoint\":\"https://x/y\"}"), false, "").status,
            400
        );

        // The remaining refusals are unchanged.
        assert_eq!(subscribe("S,", &body_json("{}"), false, "").status, 400);
        assert_eq!(subscribe("", &body_json("{}"), false, "").status, 400);
        assert_eq!(subscribe("S", b"not json", false, "").status, 400);
    }

    #[test]
    fn a_channel_only_subscription_skips_the_push_fan_out() {
        // The behavioural core of S1: a subscription with no endpoint must be
        // registered (so the channel's `accepts_for` gate passes) but must
        // never enter the push queue.
        let _ = subscriptions().unsubscribe("CHANNELONLY");
        let r = subscribe("CHANNELONLY", &body_json("{}"), false, "");
        assert_eq!(r.status, 201);

        // Registered, and therefore visible to the channel delivery gate.
        assert!(
            subscriptions().accepts_for("CHANNELONLY", "1.2.3", &label("CAD")),
            "the subscription exists, so its channel may receive events"
        );

        // But it contributes no push target. Measured through the registry
        // rather than the queue: `matching` is what the fan-out iterates, and
        // the skip happens there.
        let target = subscriptions()
            .snapshot()
            .into_iter()
            .find(|s| s.subscriber == "CHANNELONLY")
            .expect("registered");
        assert!(
            target.endpoint.is_none(),
            "a channel-only subscription carries no endpoint, which is what \
             makes the fan-out skip it"
        );

        let _ = subscriptions().unsubscribe("CHANNELONLY");
    }

    #[test]
    fn unsubscribe_is_idempotent() {
        subscribe(
            "GONE",
            &body_json("{\"endpoint\":\"http://127.0.0.1:8124/ups\"}"),
            false,
            "",
        );
        assert_eq!(unsubscribe("GONE").status, 204);
        assert_eq!(unsubscribe("GONE").status, 204);
    }

    // -- Deletion Lock (PS3.18 11.10, AIW-I 4.86.4.1.2) ------------------

    #[test]
    fn deletionlock_true_registers_a_subscription_with_a_lock() {
        let r = subscribe(
            "LOCKED",
            &body_json("{\"endpoint\":\"http://127.0.0.1:8124/ups\"}"),
            false,
            "deletionlock=true",
        );
        assert_eq!(r.status, 201);
        let text = String::from_utf8(r.body).unwrap();
        assert!(
            text.contains("\"deletionlock\":true"),
            "the 201 body reports the lock: {text}"
        );
        assert!(
            subscriptions().newest_deletion_lock().is_some(),
            "the registry entry carries the lock instant"
        );
        subscriptions().unsubscribe("LOCKED");
    }

    #[test]
    fn an_absent_deletionlock_registers_without_one() {
        // The parameter is optional (PS3.18 11.10's `{?deletionlock}`
        // template), so its absence means "no lock" rather than an error.
        let r = subscribe(
            "UNLOCKED",
            &body_json("{\"endpoint\":\"http://127.0.0.1:8124/ups\"}"),
            false,
            "",
        );
        assert_eq!(r.status, 201);
        let text = String::from_utf8(r.body).unwrap();
        assert!(
            text.contains("\"deletionlock\":false"),
            "the body states the absence: {text}"
        );
        subscriptions().unsubscribe("UNLOCKED");

        // An explicit false is the same statement.
        let r = subscribe(
            "EXPLICIT",
            &body_json("{\"endpoint\":\"http://127.0.0.1:8124/ups\"}"),
            false,
            "deletionlock=false",
        );
        assert_eq!(r.status, 201);
        assert!(
            String::from_utf8(r.body)
                .unwrap()
                .contains("\"deletionlock\":false")
        );
        subscriptions().unsubscribe("EXPLICIT");
    }

    #[test]
    fn an_invalid_deletionlock_value_is_refused_not_treated_as_false() {
        // Silently dropping a lock the subscriber asked for would leave it
        // believing the workitem is protected when it is not.
        for query in [
            "deletionlock=maybe",
            "deletionlock=1",
            "deletionlock=yes",
            "deletionlock=",
        ] {
            let r = subscribe(
                "BADLOCK",
                &body_json("{\"endpoint\":\"http://127.0.0.1:8124/ups\"}"),
                false,
                query,
            );
            assert_eq!(r.status, 400, "query {query:?} must be refused");
            let text = String::from_utf8(r.body).unwrap();
            assert!(
                text.contains("deletionlock"),
                "the message names the parameter: {text}"
            );
            assert!(
                !subscriptions().unsubscribe("BADLOCK"),
                "the refused call may not register a subscriber"
            );
        }
    }

    #[test]
    fn the_deletion_lock_parses_for_all_three_subscription_resources() {
        // The same handler serves the worklist (34.5), the filtered
        // worklist (34.5.1) and a workitem-scoped subscription; the
        // `filtered` flag is the only difference the parser sees, so both
        // values of it must yield a lock - asserted rather than assumed.
        for filtered in [false, true] {
            let subscriber = if filtered { "LOCKF" } else { "LOCKP" };
            let r = subscribe(
                subscriber,
                &body_json("{\"endpoint\":\"http://127.0.0.1:8124/ups\"}"),
                filtered,
                "deletionlock=true",
            );
            assert_eq!(r.status, 201, "filtered={filtered}");
            assert!(
                String::from_utf8(r.body)
                    .unwrap()
                    .contains("\"deletionlock\":true"),
                "filtered={filtered} carries the lock"
            );
            subscriptions().unsubscribe(subscriber);
        }
    }

    #[test]
    fn deletionlock_is_matched_case_insensitively() {
        // Query-parameter keys and the true/false lexical forms are both
        // matched without regard to case, the convention this server already
        // uses for `limit`.
        let r = subscribe(
            "CASELOCK",
            &body_json("{\"endpoint\":\"http://127.0.0.1:8124/ups\"}"),
            false,
            "DeletionLock=TRUE",
        );
        assert_eq!(r.status, 201);
        assert!(
            String::from_utf8(r.body)
                .unwrap()
                .contains("\"deletionlock\":true")
        );
        subscriptions().unsubscribe("CASELOCK");
    }

    #[test]
    fn other_query_parameters_do_not_disturb_the_lock() {
        // The parameter is parsed out of the whole query string, so a
        // request carrying others alongside it must still read the lock.
        let r = subscribe(
            "WITHOTHERS",
            &body_json("{\"endpoint\":\"http://127.0.0.1:8124/ups\"}"),
            false,
            "limit=10&deletionlock=true&offset=0",
        );
        assert_eq!(r.status, 201);
        assert!(
            String::from_utf8(r.body)
                .unwrap()
                .contains("\"deletionlock\":true")
        );
        subscriptions().unsubscribe("WITHOTHERS");
    }

    #[test]
    fn unsubscribing_releases_the_lock() {
        // PS3.4 CC.2.3.1 Table CC.2.3-2: "AE Unsubscribes from Specific UPS:
        // Go to Not Subscribed" - the lock goes with the subscription.
        //
        // The assertion is scoped to THIS subscriber on purpose. The registry
        // is process-wide and the lib binary runs these tests in parallel,
        // while several of the tests in this file hold a lock for the length
        // of their body. A global "nobody holds a lock" assert therefore
        // observes a NEIGHBOUR's lock and fails depending on scheduling - it
        // did, at thumbnail acceptance, with the neighbour's `Some(1789991679)`.
        // What this test means is narrower and always true: after RELEASE
        // unsubscribes, its own registration (and so its lock) is gone, and
        // nobody else's was disturbed.
        let own = |subscriber: &str| {
            subscriptions()
                .snapshot()
                .into_iter()
                .filter(|s| s.subscriber == subscriber)
                .count()
        };

        subscribe(
            "RELEASE",
            &body_json("{\"endpoint\":\"http://127.0.0.1:8124/ups\"}"),
            false,
            "deletionlock=true",
        );
        assert_eq!(own("RELEASE"), 1, "registered once");
        assert!(
            subscriptions()
                .snapshot()
                .iter()
                .any(|s| s.subscriber == "RELEASE" && s.deletion_lock_at.is_some()),
            "and it registered WITH a lock"
        );

        assert!(subscriptions().unsubscribe("RELEASE"));
        assert_eq!(
            own("RELEASE"),
            0,
            "the registration went away, so its lock went with it"
        );
    }
}
