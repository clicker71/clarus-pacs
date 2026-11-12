// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Query parameter types for UPS-RS endpoints.

#![forbid(unsafe_code)]

/// UPS-RS search query parameters (PS3.18 Section 11.4).
/// Field names use DICOM tag hex strings to match DICOMweb conventions.
/// (de-Serialized; parsed natively from form-urlencoded.)
#[derive(Debug)]
#[allow(non_snake_case)]
pub struct UpsQuery {
    /// Study Instance UID of the input study (0020,000D).
    ///
    /// Named `study_uid` and not `workitem_uid`, which is what it used to be
    /// called. The key is (0020,000D) - the STUDY - and the store compares it
    /// against `WorkItemRecord.input_study_uid` (the 0040,4021 Input
    /// Information Sequence), never against the workitem's own UID
    /// (0008,0018), which is the value in the `/workitems/{uid}` path. Under
    /// the old name a search for a study matched nothing, because a workitem
    /// UID equals a study UID only by coincidence.
    pub study_uid: Option<String>,

    /// Accession Number (0040,A370 > 0008,0050) - a REQUIRED matching key
    /// (PS3.4 Table CC.2.5-3).
    ///
    /// The standard writes the key as a path through the Referenced Request
    /// Sequence; this server stores one scalar per workitem, so the sequence
    /// segment is carried in the parameter NAME (see `parse_form_query`) and
    /// the value is compared directly. A requester that sends the shorter
    /// `00080050` spelling reaches the same field.
    pub accession_number: Option<String>,

    /// Procedure Step Label (0074,1204) - a standard matching key
    /// (PS3.4 Table CC.2.5-3).
    pub step_label: Option<String>,

    /// Scheduled Station Name Code Sequence > Code Value (0040,4025).
    ///
    /// The station key of AIW-I 4.81.4.1.2.1 Table 4.81.4.1.2.1-3, whose Code
    /// Value IS the performer AE Title - so a station-oriented search and the
    /// AE Title a performer claims are the same fact. This is the ONLY
    /// carrier: (0040,0001) Scheduled Station AE Title has no row in PS3.4
    /// Table CC.2.5-3, and carrying the station in a vendor key as well would
    /// state one fact twice with two ways to get it wrong.
    pub scheduled_station_code_value: Option<String>,

    /// (0040,4018) Scheduled Workitem Code Sequence > Code Value.
    ///
    /// A REQUIRED matching key (PS3.4 Table CC.2.5-3, listed as a Sequence:
    /// the key reaches the Code Value through the sequence, which is how a
    /// requestor states it). The record stores the whole code item, so the
    /// filter compares the stored Code Value.
    pub scheduled_workitem_code_value: Option<String>,

    /// (0040,A370) Referenced Request Sequence > (0040,1001) Requested
    /// Procedure ID - a REQUIRED matching key (PS3.4 Table CC.2.5-3, R).
    ///
    /// The standard writes the key as a path through the Referenced Request
    /// Sequence; the sequence segment lives in the parameter NAME on this
    /// server, whose workitem carries one scalar.
    pub requested_procedure_id: Option<String>,

    /// Patient ID (0010,0020).
    pub patient_id: Option<String>,

    /// Scheduled Procedure Step Start DateTime (0040,0405) - a REQUIRED
    /// matching key (PS3.4 Table CC.2.5-3, matchType "Single Value or
    /// Range").
    ///
    /// A DT value, so a query states either one instant or a range
    /// (`20260917-20260918`), which is what the standard's matchType allows.
    pub sps_start_datetime: Option<String>,

    /// Scheduled Procedure Step Priority (0074,1200) - a REQUIRED matching
    /// key (PS3.4 Table CC.2.5-3, matchType "Single Value").
    pub sps_priority: Option<String>,

    /// Patient's Name (0010,0010) - a REQUIRED matching key (PS3.4 Table
    /// CC.2.5-3).
    ///
    /// A PN value; the comparison is the literal stored string, the same
    /// convention the sibling LO/SH keys use on this server.
    pub patient_name: Option<String>,

    /// (0008,0060) Modality - a VENDOR matching key.
    ///
    /// The UPS IOD defines no Modality, so this is not a standard UPS matching
    /// key; it exists because the DIMSE MWL service requires Modality as a
    /// Return key Type 1 and the bridge maps a modality query onto it. The
    /// render carries the vendor-extended attribute, so querying it is
    /// consistent: an absent or empty parameter matches everything.
    pub modality: Option<String>,

    /// Procedure Step State (0074,1000): SCHEDULED | IN PROGRESS | COMPLETED |
    /// CANCELED.
    pub workflow_state: Option<String>,

    /// Maximum results to return. `None` = server default (`[ups]
    /// default_limit`, 100).
    pub limit: Option<u64>,

    /// Zero-based offset. Default: 0.
    pub offset: Option<u64>,
}

fn default_offset() -> Option<u64> {
    Some(0)
}

impl Default for UpsQuery {
    fn default() -> Self {
        Self {
            study_uid: None,
            accession_number: None,
            step_label: None,
            scheduled_station_code_value: None,
            scheduled_workitem_code_value: None,
            requested_procedure_id: None,
            patient_id: None,
            sps_start_datetime: None,
            sps_priority: None,
            patient_name: None,
            modality: None,
            workflow_state: None,
            limit: None,
            offset: default_offset(),
        }
    }
}

/// Parse a form-urlencoded UPS search query string into an [`UpsQuery`]
/// (native percent-decoder; replaces `serde_urlencoded::from_str`).
///
/// Each matching key is accepted in its hex-tag form and in its PS3.6 keyword
/// form (PS3.18 8.3.4 defines the key by keyword). A key that is neither is
/// refused: see the `_` arm below for why silence is the wrong answer here.
pub fn parse_form_query(query_str: &str) -> Result<UpsQuery, String> {
    let mut q = UpsQuery::default();
    // Fields already assigned by a previous parameter, so a repeated matching
    // attribute can be refused rather than silently replaced.
    //
    // PS3.18 8.3.4.1: `normal-match = 1*("&" attribute "=" value)` - the
    // grammar has no production for one attribute twice, and DICOM matching is
    // conjunctive, so two values for one attribute would have to hold at once.
    // The old last-wins rule discarded the first constraint without saying so,
    // and the caller could not tell a filtered answer from an unfiltered one.
    // Aliases count as the same field: `PatientID` and `00100020` name one
    // attribute, so sending both is the same repetition.
    let mut seen: Vec<&'static str> = Vec::new();
    for pair in query_str.split('&') {
        if pair.is_empty() {
            continue;
        }
        // Decode the pair BEFORE splitting it, for the same reason the
        // subscribe parser does: `=` is a reserved delimiter inside a query
        // value (RFC 3986 2.2), so a conformant user agent encodes the
        // separator and a matching key arrives as
        // `ReferencedRequestSequence.StudyInstanceUID%3D<uid>`. Splitting the
        // RAW pair found no `=` at all, took the whole thing as the KEY, and
        // refused it as a misspelled parameter - a correct search answered 400
        // while the un-encoded spelling returned rows.
        //
        // Decoding first is also why the key may carry an encoded `.`
        // (`%2E`): the key is matched after this decode, so it sees the
        // literal spelling. The value therefore gets NO second decode.
        let pair = crate::handlers::json_common::form_decode_query_str(pair);
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair.as_str(), ""),
        };
        let key = k.to_string();
        let val = v.to_string();
        // Assign one matching-key field, refusing a second assignment.
        //
        // `field` is the canonical attribute name, not the spelling the caller
        // used, so the two spellings of one attribute collide as they should.
        macro_rules! set_match {
            ($field:literal, $target:expr) => {{
                if seen.contains(&$field) {
                    return Err(format!(
                        "Repeated matching attribute '{key}': query parameter '{}' was already \
                         given (PS3.18 8.3.4.1 defines one value per attribute)",
                        $field
                    ));
                }
                seen.push($field);
                $target = Some(val);
            }};
        }
        // PS3.18 8.3.4 (and 11.10.1.2, which cross-references it) defines the
        // matching keys as attribute KEYWORDS, e.g. `PatientID` or
        // `ReferencedRequestSequence.AccessionNumber` - that is what a
        // conformant user agent sends. This server's own vocabulary is the hex
        // tag, so EVERY key accepts both spellings and routes them to the same
        // field. A keyword that fell through to the `_` arm would silently
        // return the UNFILTERED worklist, which is a privacy leak rather than a
        // visible error; the hex form is kept because it is what this
        // deployment's own tests and the DICOMweb conformance statement use.
        //
        // Spelling is the standard's exact PS3.6 keyword (case-sensitive): the
        // standard fixes the case, and matching it exactly means a wrong-case
        // key is a miss rather than a second, accidentally-working spelling.
        //
        // When one request carries two spellings of ONE fact, the request is
        // refused (see `set_match` above) rather than resolved by position.
        match key.as_str() {
            "0020000D" | "StudyInstanceUID" => set_match!("StudyInstanceUID", q.study_uid),
            // PS3.4 Table CC.2.5-3 writes both study and accession as a path
            // through the Referenced Request Sequence; the sequence segment
            // lives in the parameter NAME on this server. The two forms name
            // ONE fact - the study the workitem operates on - so `study_uid`
            // is the whole statement and the sequence form is an alias rather
            // than a second field.
            "0040A370.0020000D" | "ReferencedRequestSequence.StudyInstanceUID" => {
                set_match!("StudyInstanceUID", q.study_uid);
            }
            // PS3.4 Table CC.2.5-3 writes the key as a path through the
            // Referenced Request Sequence; the sequence segment lives in the
            // parameter NAME on this server, whose workitem carries one
            // accession number. Both the standard path form and the bare
            // attribute tag are accepted, because they name one fact and a
            // caller sending either means the same search.
            "0040A370.00080050"
            | "00080050"
            | "AccessionNumber"
            | "ReferencedRequestSequence.AccessionNumber" => {
                set_match!("AccessionNumber", q.accession_number);
            }
            "00741204" | "ProcedureStepLabel" => {
                set_match!("ProcedureStepLabel", q.step_label);
            }
            // (0040,4025) travels as a Sequence in the standard, so a client
            // that spells the key the way dcm4chee does writes the path form
            // `00404025.00080100`; the bare sequence tag names the same fact
            // and is accepted beside it. PS3.18 8.3.4 allows a tag path in a
            // matching key, so both are the standard's own spellings.
            "00404025" | "ScheduledStationNameCodeSequence.CodeValue" | "00404025.00080100" => {
                set_match!(
                    "ScheduledStationNameCodeSequence.CodeValue",
                    q.scheduled_station_code_value
                );
            }
            // (0040,4018) Scheduled Workitem Code Sequence > (0008,0100) Code
            // Value - a REQUIRED matching key (PS3.4 Table CC.2.5-3).
            "00404018.00080100" | "ScheduledWorkitemCodeSequence.CodeValue" => {
                set_match!(
                    "ScheduledWorkitemCodeSequence.CodeValue",
                    q.scheduled_workitem_code_value
                );
            }
            // (0040,A370) > (0040,1001) - a REQUIRED matching key. The
            // sequence-qualified keyword is the standard's spelling and the
            // dotted tag is the same path written in hex (PS3.18 8.3.4).
            "0040A370.00401001" | "ReferencedRequestSequence.RequestedProcedureID" => {
                set_match!("RequestedProcedureID", q.requested_procedure_id);
            }
            "00100020" | "PatientID" => set_match!("PatientID", q.patient_id),
            // (0040,0405), (0074,1200) and (0010,0010) are REQUIRED matching
            // keys in PS3.4 Table CC.2.5-3 and had no branch at all, so every
            // search that used one was refused as unknown. A DT key accepts
            // the standard's range form as well as one instant, which is what
            // its "Single Value or Range" matchType states.
            "00404005" | "ScheduledProcedureStepStartDateTime" => {
                set_match!("ScheduledProcedureStepStartDateTime", q.sps_start_datetime);
            }
            "00741200" | "ScheduledProcedureStepPriority" => {
                set_match!("ScheduledProcedureStepPriority", q.sps_priority);
            }
            "00100010" | "PatientName" => set_match!("PatientName", q.patient_name),
            // Modality is a documented vendor extension, not a UPS attribute
            // (PS3.4 Table CC.2.5-3 defines none), so only this server and its
            // clients know the keyword - it is spelled after the DICOM attribute
            // it mirrors.
            "00080060" | "Modality" => set_match!("Modality", q.modality),
            "00741000" | "ProcedureStepState" => {
                set_match!("ProcedureStepState", q.workflow_state);
            }
            // PS3.18 8.3.4 Table 8.3.4-1: limit/offset are uint, and the
            // grammar is closed - a value that is not an unsigned integer is
            // an invalid query parameter (400, PS3.18 8.5), not a silently
            // ignored one.
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
            // PS3.18 Table 8.3.4-1 lists the search query parameters and marks
            // every one of them M (mandatory) for the Origin Server, with the
            // text "Search Transactions shall support these parameters". These
            // four are the control parameters, not matching keys (8.3.4.2 to
            // 8.3.4.6 are their own sections), so the exhaustive-key argument
            // below does not reach them: refusing one is a conformance defect,
            // not a safety measure.
            //
            // Their VALUE grammar is closed all the same: `fuzzymatching` and
            // the two *valuematching parameters take `true` or `false` and
            // `includefield` takes one or more attributes or `all`. Accepting
            // `fuzzymatching=banana` or an empty `includefield=` would answer a
            // search whose stated semantics the server did not apply, so both
            // are refused as malformed (PS3.18 8.5).
            "fuzzymatching" | "emptyvaluematching" | "multiplevaluematching" => {
                if val != "true" && val != "false" {
                    return Err(format!(
                        "'{key}' must be true or false (got \"{val}\"): PS3.18 Table 8.3.4-1"
                    ));
                }
            }
            "includefield" => {
                if val.is_empty() {
                    return Err(
                        "'includefield' needs at least one attribute or 'all': PS3.18 Table 8.3.4-1"
                            .to_string(),
                    );
                }
            }
            // PS3.18 8.3.4 defines the matching keys exhaustively: a key that
            // is neither a PS3.6 keyword nor an 8HEXDIG tag is not a filter this
            // server can honour, and DROPPING it would answer with the
            // UNFILTERED worklist - a requester that misspells PatientID would
            // receive every patient's workitems and have no way to tell.
            // dcm4chee-arc refuses the same request with 400 "Invalid Query
            // Parameter", so the loud answer is also the interoperable one.
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

/// Encode an [`UpsQuery`] back to a form-urlencoded query string (native
/// percent-encoder; replaces `serde_urlencoded::to_string`).
///
/// Spaces travel as `%20`, not `+`: PS3.18 8.3.1 lists `+` in `qp-special`, so
/// a `+` in a query value means a literal plus and a space must be
/// percent-encoded. Encoding a space as `+` would decode back as `+` on any
/// conformant reader, so the encoder and `parse_form_query` would disagree.
pub fn form_urlencode_query(q: &UpsQuery) -> String {
    let mut parts: Vec<String> = Vec::new();
    let push = |parts: &mut Vec<String>, key: &str, val: &str| {
        parts.push(format!(
            "{}={}",
            crate::handlers::json_common::form_encode_query_str(key),
            crate::handlers::json_common::form_encode_query_str(val)
        ));
    };
    if let Some(v) = &q.study_uid {
        push(&mut parts, "0020000D", v);
    }
    if let Some(v) = &q.accession_number {
        push(&mut parts, "0040A370.00080050", v);
    }
    if let Some(v) = &q.step_label {
        push(&mut parts, "00741204", v);
    }
    if let Some(v) = &q.scheduled_station_code_value {
        push(&mut parts, "00404025", v);
    }
    if let Some(v) = &q.scheduled_workitem_code_value {
        push(&mut parts, "00404018.00080100", v);
    }
    if let Some(v) = &q.requested_procedure_id {
        push(&mut parts, "0040A370.00401001", v);
    }
    if let Some(v) = &q.patient_id {
        push(&mut parts, "00100020", v);
    }
    if let Some(v) = &q.sps_start_datetime {
        push(&mut parts, "00404005", v);
    }
    if let Some(v) = &q.sps_priority {
        push(&mut parts, "00741200", v);
    }
    if let Some(v) = &q.patient_name {
        push(&mut parts, "00100010", v);
    }
    if let Some(v) = &q.modality {
        push(&mut parts, "00080060", v);
    }
    if let Some(v) = &q.workflow_state {
        push(&mut parts, "00741000", v);
    }
    if let Some(v) = &q.limit {
        push(&mut parts, "limit", &v.to_string());
    }
    if let Some(v) = &q.offset {
        push(&mut parts, "offset", &v.to_string());
    }
    parts.join("&")
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::handlers::json_common::{form_decode_str, form_encode_str};

    // byte-parity with serde_urlencoded (captured 19.08 - the native
    // form encode/decode must reproduce `url::form_urlencoded` exactly).
    #[test]
    fn form_encode_matches_serde_urlencoded() {
        // (input, expected serde_urlencoded byte)
        let cases = [
            ("1.2.826.0.1.9", "1.2.826.0.1.9"),
            ("CT1", "CT1"),
            ("SMIRNOV^IVAN", "SMIRNOV%5EIVAN"),
            ("DR SMITH", "DR+SMITH"),
            ("a+b&c=d%", "a%2Bb%26c%3Dd%25"),
            ("IN PROGRESS", "IN+PROGRESS"),
            ("100", "100"),
            (
                "\u{043a}\u{043b}\u{044e}\u{0447} \u{0437}\u{043d}\u{0430}\u{0447}\u{0435}\u{043d}\u{0438}\u{0435}",
                "%D0%BA%D0%BB%D1%8E%D1%87+%D0%B7%D0%BD%D0%B0%D1%87%D0%B5%D0%BD%D0%B8%D0%B5",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(form_encode_str(input), expected, "input {:?}", input);
        }
    }

    #[test]
    fn form_decode_matches_serde_urlencoded() {
        let cases = [
            ("1.2.826.0.1.9", "1.2.826.0.1.9"),
            ("CT1", "CT1"),
            ("SMIRNOV%5EIVAN", "SMIRNOV^IVAN"),
            ("DR+Smith", "DR Smith"),
            ("a%2Bb%26c%3Dd%25", "a+b&c=d%"),
            ("IN+PROGRESS", "IN PROGRESS"),
            (
                "key%D0%BA%D0%BB%D1%8E%D1%87",
                "key\u{043a}\u{043b}\u{044e}\u{0447}",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(form_decode_str(input), expected, "input {:?}", input);
        }
    }

    #[test]
    fn parse_form_query_matches_serde_urlencoded() {
        // (query_str, expected UpsQuery)
        let cases = [
            (
                "0020000D=1.2.826.0.1.9&00741204=CAD&limit=5",
                UpsQuery {
                    study_uid: Some("1.2.826.0.1.9".into()),
                    accession_number: None,
                    step_label: Some("CAD".into()),
                    scheduled_station_code_value: None,
                    scheduled_workitem_code_value: None,
                    requested_procedure_id: None,
                    patient_id: None,
                    sps_start_datetime: None,
                    sps_priority: None,
                    patient_name: None,
                    modality: None,
                    workflow_state: None,
                    limit: Some(5),
                    offset: Some(0),
                },
            ),
            (
                "00741000=IN%20PROGRESS&offset=10",
                UpsQuery {
                    study_uid: None,
                    accession_number: None,
                    step_label: None,
                    scheduled_station_code_value: None,
                    scheduled_workitem_code_value: None,
                    requested_procedure_id: None,
                    patient_id: None,
                    sps_start_datetime: None,
                    sps_priority: None,
                    patient_name: None,
                    modality: None,
                    workflow_state: Some("IN PROGRESS".into()),
                    limit: None,
                    offset: Some(10),
                },
            ),
            // The station key parses in BOTH standard spellings: the bare
            // sequence tag and the dotted path dcm4chee writes.
            (
                "00404025=CT1",
                UpsQuery {
                    scheduled_station_code_value: Some("CT1".into()),
                    ..UpsQuery::default()
                },
            ),
            (
                "00404025.00080100=CT1",
                UpsQuery {
                    scheduled_station_code_value: Some("CT1".into()),
                    ..UpsQuery::default()
                },
            ),
            // (0040,4018) Scheduled Workitem Code Sequence > Code Value.
            (
                "00404018.00080100=CT-ORGAN-SEG",
                UpsQuery {
                    scheduled_workitem_code_value: Some("CT-ORGAN-SEG".into()),
                    ..UpsQuery::default()
                },
            ),
            // (0008,0060) Modality - the vendor matching key the bridge sends
            // for an MWL Modality query (MWL_TO_UPS_PARAM).
            (
                "00080060=CT",
                UpsQuery {
                    modality: Some("CT".into()),
                    ..UpsQuery::default()
                },
            ),
            // The Accession Number key parses in BOTH spellings: the standard's
            // sequence-qualified path and the bare attribute tag.
            (
                "0040A370.00080050=ACC-1",
                UpsQuery {
                    accession_number: Some("ACC-1".into()),
                    ..UpsQuery::default()
                },
            ),
            (
                "00080050=ACC-1",
                UpsQuery {
                    accession_number: Some("ACC-1".into()),
                    ..UpsQuery::default()
                },
            ),
        ];
        for (qs, expected) in cases {
            let got = parse_form_query(qs).expect("every key in this table is a matching key");
            assert_eq!(got.study_uid, expected.study_uid);
            assert_eq!(got.accession_number, expected.accession_number);
            assert_eq!(got.step_label, expected.step_label);
            assert_eq!(
                got.scheduled_station_code_value,
                expected.scheduled_station_code_value
            );
            assert_eq!(
                got.scheduled_workitem_code_value,
                expected.scheduled_workitem_code_value
            );
            assert_eq!(got.patient_id, expected.patient_id);
            assert_eq!(got.modality, expected.modality);
            assert_eq!(got.workflow_state, expected.workflow_state);
            assert_eq!(got.limit, expected.limit);
            assert_eq!(got.offset, expected.offset);
        }

        // A space travels as %20. The form-urlencoded `+` escape is NOT used:
        // PS3.18 8.3.1 makes `+` a literal `qp-special` character, so `+`
        // means plus and decoding it to a space would corrupt any value that
        // carries one.
        let plus = parse_form_query("00741000=IN+PROGRESS").expect("a literal plus is legal");
        assert_eq!(
            plus.workflow_state.as_deref(),
            Some("IN+PROGRESS"),
            "a raw + is a literal plus, not a space (PS3.18 8.3.1)"
        );
    }

    /// G1: a repeated matching attribute is refused, never resolved by
    /// position.
    ///
    /// PS3.18 8.3.4.1 defines a match as a SEQUENCE of `attribute=value`
    /// pairs and has no production for one attribute twice; DICOM matching is
    /// conjunctive, so two values for one attribute could never both hold. The
    /// old last-wins rule silently dropped the first constraint, and the
    /// caller could not tell a filtered answer from an unfiltered one.
    #[test]
    fn a_repeated_matching_attribute_is_refused() {
        let err = parse_form_query("PatientID=PT-1&PatientID=PT-2")
            .expect_err("a repeated matching attribute must be refused");
        assert!(
            err.contains("PatientID") && err.contains("Repeated"),
            "the message names the attribute: {err}"
        );

        // An ALIAS is the same attribute: the hex tag and the keyword name one
        // fact, so sending both is the same repetition.
        let alias = parse_form_query("00100020=PT-1&PatientID=PT-2")
            .expect_err("the hex tag and the keyword are one attribute");
        assert!(alias.contains("Repeated"), "alias refused: {alias}");

        // The same holds for the two study spellings, which used to be
        // documented as resolving last-wins.
        let study =
            parse_form_query("0020000D=1.2.3&ReferencedRequestSequence.StudyInstanceUID=1.2.4")
                .expect_err("the two study spellings are one attribute");
        assert!(study.contains("Repeated"), "study refused: {study}");

        // Distinct attributes in one request are still fine.
        let ok = parse_form_query("PatientID=PT-1&PatientName=SMITH^JOHN")
            .expect("two different attributes are two constraints");
        assert_eq!(ok.patient_id.as_deref(), Some("PT-1"));
        assert_eq!(ok.patient_name.as_deref(), Some("SMITH^JOHN"));
    }

    /// G2: a recognized search control with an out-of-grammar value is
    /// refused (PS3.18 Table 8.3.4-1 closes every one of them).
    #[test]
    fn search_controls_are_validated() {
        for bad in [
            "fuzzymatching=banana",
            "emptyvaluematching=yes",
            "multiplevaluematching=1",
            "includefield=",
        ] {
            let err = parse_form_query(bad).expect_err("out of grammar");
            assert!(
                err.contains("8.3.4-1"),
                "the message cites the table for {bad}: {err}"
            );
        }
        // The legal values still parse.
        for good in [
            "fuzzymatching=true",
            "fuzzymatching=false",
            "includefield=all",
            "includefield=PatientID",
            "emptyvaluematching=false",
            "multiplevaluematching=true",
        ] {
            parse_form_query(good).unwrap_or_else(|e| panic!("{good} must parse: {e}"));
        }
    }

    /// G3: a literal plus in a value survives, and a value with a space
    /// round-trips through the encoder as %20.
    #[test]
    fn a_literal_plus_survives_and_spaces_round_trip() {
        let plus = parse_form_query("PatientID=A+B").expect("a literal plus is legal");
        assert_eq!(
            plus.patient_id.as_deref(),
            Some("A+B"),
            "PS3.18 8.3.1 lists + in qp-special, so it means a plus"
        );

        // A space travels as %20 in both directions.
        let spaced = parse_form_query("PatientID=A%20B").expect("percent-encoded space");
        assert_eq!(spaced.patient_id.as_deref(), Some("A B"));

        let encoded = form_urlencode_query(&UpsQuery {
            patient_id: Some("A B".into()),
            ..UpsQuery::default()
        });
        assert_eq!(
            encoded, "00100020=A%20B&offset=0",
            "a space is %20, never +"
        );
        assert_eq!(
            parse_form_query(&encoded).expect("own output").patient_id,
            Some("A B".to_string())
        );

        // And a literal plus round-trips as %2B.
        let encoded_plus = form_urlencode_query(&UpsQuery {
            patient_id: Some("A+B".into()),
            ..UpsQuery::default()
        });
        assert_eq!(encoded_plus, "00100020=A%2BB&offset=0");
        assert_eq!(
            parse_form_query(&encoded_plus)
                .expect("own output")
                .patient_id,
            Some("A+B".to_string())
        );
    }

    #[test]
    fn form_urlencode_query_matches_serde_urlencoded_key_order() {
        // Fields are emitted in struct declaration order
        // (0020000D, 0040A370.00080050, 00741204, 00404025,
        // 00404018.00080100, 00100020, 00080060, 00741000, limit, offset).
        let q = UpsQuery {
            study_uid: Some("1.2.826.0.1.9".into()),
            accession_number: None,
            step_label: Some("CAD".into()),
            scheduled_station_code_value: None,
            scheduled_workitem_code_value: None,
            requested_procedure_id: None,
            patient_id: Some("PT-1".into()),
            sps_start_datetime: None,
            sps_priority: None,
            patient_name: None,
            modality: Some("CT".into()),
            workflow_state: Some("SCHEDULED".into()),
            limit: Some(100),
            offset: Some(0),
        };
        let s = form_urlencode_query(&q);
        assert_eq!(
            s,
            "0020000D=1.2.826.0.1.9&00741204=CAD&00100020=PT-1&00080060=CT&00741000=SCHEDULED&limit=100&offset=0"
        );

        // The Accession Number round-trips in the standard's qualified form.
        let acc = UpsQuery {
            accession_number: Some("ACC-1".into()),
            ..UpsQuery::default()
        };
        assert_eq!(
            form_urlencode_query(&acc),
            "0040A370.00080050=ACC-1&offset=0"
        );

        // The station key round-trips in its standard form. The rest of the
        // struct defaults and `offset` is always emitted, so the string
        // carries the trailing offset=0.
        let code = UpsQuery {
            scheduled_station_code_value: Some("CT1".into()),
            ..UpsQuery::default()
        };
        assert_eq!(form_urlencode_query(&code), "00404025=CT1&offset=0");

        // And so does the workitem code, in the path form that names the Code
        // Value inside the sequence (PS3.4 Table CC.2.5-3 types the key as a
        // Sequence, so the value is reached through a path).
        let work = UpsQuery {
            scheduled_workitem_code_value: Some("CT-ORGAN-SEG".into()),
            ..UpsQuery::default()
        };
        assert_eq!(
            form_urlencode_query(&work),
            "00404018.00080100=CT-ORGAN-SEG&offset=0"
        );
    }

    #[test]
    fn form_roundtrip_preserves_fields() {
        // encode -> decode must preserve all fields (internal reroute path).
        let q = UpsQuery {
            study_uid: Some("1.2.826.0.1.9".into()),
            accession_number: Some("ACC-1".into()),
            step_label: Some("CAD".into()),
            scheduled_station_code_value: Some("CT1".into()),
            scheduled_workitem_code_value: Some("CT-ORGAN-SEG".into()),
            requested_procedure_id: Some("RP-1".into()),
            patient_id: Some("SMIRNOV^IVAN".into()),
            sps_start_datetime: None,
            sps_priority: None,
            patient_name: None,
            modality: Some("CT".into()),
            workflow_state: Some("IN PROGRESS".into()),
            limit: Some(5),
            offset: Some(10),
        };
        let s = form_urlencode_query(&q);
        let got = parse_form_query(&s).expect("the encoder emits matching keys only");
        assert_eq!(got.study_uid, q.study_uid);
        assert_eq!(got.accession_number, q.accession_number);
        assert_eq!(got.step_label, q.step_label);
        assert_eq!(
            got.scheduled_station_code_value,
            q.scheduled_station_code_value
        );
        assert_eq!(
            got.scheduled_workitem_code_value,
            q.scheduled_workitem_code_value
        );
        assert_eq!(got.patient_id, q.patient_id);
        assert_eq!(got.modality, q.modality);
        assert_eq!(got.workflow_state, q.workflow_state);
        assert_eq!(got.limit, q.limit);
        assert_eq!(got.offset, q.offset);
    }

    /// The search parser decodes the pair BEFORE splitting it, so an encoded
    /// `=` separator reaches the same field as a raw one.
    ///
    /// This is the T2 half of the subscribe percent-decode defect: the
    /// subscribe parser split the raw value and answered 400 to
    /// `ReferencedRequestSequence.StudyInstanceUID%3D<uid>`. The search parser
    /// never had that bug because it decodes both halves first, and this pins
    /// that so a later "tidy-up" cannot reintroduce the asymmetry.
    #[test]
    fn parse_form_query_accepts_an_encoded_separator() {
        let raw = parse_form_query("ReferencedRequestSequence.StudyInstanceUID=1.2.826.0.1.9")
            .expect("raw separator");
        let encoded =
            parse_form_query("ReferencedRequestSequence.StudyInstanceUID%3D1.2.826.0.1.9")
                .expect("encoded separator");
        assert_eq!(encoded.study_uid, raw.study_uid);
        assert_eq!(encoded.study_uid.as_deref(), Some("1.2.826.0.1.9"));

        // A key whose dot is encoded resolves like the literal spelling: the
        // decode happens once, on the whole pair, before the key is matched.
        let dotted = parse_form_query("ReferencedRequestSequence%2EStudyInstanceUID=1.2.826.0.1.9")
            .expect("encoded dot");
        assert_eq!(dotted.study_uid, raw.study_uid);
    }

    /// PS3.4 Table CC.2.5-3 carries the study through Referenced Request
    /// Sequence, and the requester (Weasis, following the nroduit rule) sends
    /// the qualified path. Both names state ONE fact - the study the workitem
    /// operates on - so they parse into the same field. Before this the
    /// qualified key was unknown and silently ignored, and the search then
    /// returned every workitem instead of the caller's one.
    #[test]
    fn parse_form_query_accepts_the_referenced_request_study_alias() {
        let q = parse_form_query("0040A370.0020000D=1.2.826.0.1.9").expect("alias key");
        assert_eq!(q.study_uid.as_deref(), Some("1.2.826.0.1.9"));

        // The bare form still works and reaches the SAME field.
        let bare = parse_form_query("0020000D=1.2.826.0.1.9").expect("bare key");
        assert_eq!(bare.study_uid, q.study_uid);

        // The two search keys of the nroduit rule travel together:
        // PatientID + the sequence-qualified accession + the sequence study.
        let combined = parse_form_query(
            "00100020=PID1&0040A370.00080050=ACC1&0040A370.0020000D=1.2.826.0.1.9",
        )
        .expect("the nroduit key set");
        assert_eq!(combined.patient_id.as_deref(), Some("PID1"));
        assert_eq!(combined.accession_number.as_deref(), Some("ACC1"));
        assert_eq!(combined.study_uid.as_deref(), Some("1.2.826.0.1.9"));

        // The alias is INPUT-only: the encoder writes the bare tag, and the
        // decode of that output lands on the same field (one fact, one form
        // on the wire this server produces).
        let encoded = form_urlencode_query(&q);
        assert!(
            encoded.starts_with("0020000D="),
            "the server emits the bare study key; got {encoded}"
        );
        assert_eq!(
            parse_form_query(&encoded).expect("own output").study_uid,
            q.study_uid
        );
    }

    /// B: the three REQUIRED matching keys that had no branch at all.
    ///
    /// PS3.4 Table CC.2.5-3 marks ScheduledProcedureStepStartDateTime
    /// (0040,0405), ScheduledProcedureStepPriority (0074,1200) and Patient's
    /// Name (0010,0010) as matching keys (R). Before this, every one of them
    /// fell through to the unknown-key arm and the request was refused as a
    /// misspelled parameter - so a conformant search using any of the three
    /// could not be answered at all.
    #[test]
    fn parse_form_query_accepts_the_three_missing_matching_keys() {
        let q = parse_form_query(
            "ScheduledProcedureStepStartDateTime=20260917&\
             ScheduledProcedureStepPriority=HIGH&PatientName=SMITH^JOHN",
        )
        .expect("all three are spelled as PS3.6 defines them");
        assert_eq!(q.sps_start_datetime.as_deref(), Some("20260917"));
        assert_eq!(q.sps_priority.as_deref(), Some("HIGH"));
        assert_eq!(q.patient_name.as_deref(), Some("SMITH^JOHN"));

        // The hex spelling of each writes the same field.
        let hex = parse_form_query("00404005=20260917-20260918&00741200=LOW&00100010=DOE^JANE")
            .expect("hex tags are the other accepted spelling");
        assert_eq!(
            hex.sps_start_datetime.as_deref(),
            Some("20260917-20260918"),
            "the range form survives parsing; the store expands it"
        );
        assert_eq!(hex.sps_priority.as_deref(), Some("LOW"));
        assert_eq!(hex.patient_name.as_deref(), Some("DOE^JANE"));

        // L1: (0040,4005) is the tag, so `00404005` is the hex spelling. The
        // transposed `00400405` parses as group 0040 element 0405 only by
        // accident of this server's own history; the registry's tag is the
        // authority and the renderers already emit `00404005`, so a search
        // written from the spec must reach the same field.
        let spec = parse_form_query("00404005=20260917")
            .expect("the registry tag is the hex spelling a client sends");
        assert_eq!(spec.sps_start_datetime.as_deref(), Some("20260917"));
        let wrong = parse_form_query("00400405=20260917")
            .expect_err("00400405 is not a PS3.6 tag for this attribute");
        assert!(
            wrong.contains("00400405"),
            "the refusal names the offending key: {wrong}"
        );
    }

    /// B: an unknown key is still an error, not a silently wider search.
    #[test]
    fn parse_form_query_still_refuses_an_unknown_key() {
        let err = parse_form_query("PatientName=SMITH^JOHN&NotAKey=x")
            .expect_err("an unknown parameter must be refused");
        assert!(
            err.contains("NotAKey"),
            "the message must name the offending key: {err}"
        );
    }

    /// Every matching key is accepted in its PS3.6 KEYWORD spelling, which is
    /// what PS3.18 8.3.4 defines and what a conformant user agent sends - not
    /// only in this server's hex-tag spelling.
    ///
    /// Before this a keyword key fell through the match's `_` arm and set
    /// nothing, so the search returned the WHOLE worklist: a silent privacy
    /// leak rather than an error a caller could notice.
    ///
    /// One key per attribute: a request naming the same attribute twice is
    /// refused (see `a_repeated_matching_attribute_is_refused`), so this checks
    /// the keyword spelling against its hex twin on SEPARATE requests.
    #[test]
    fn parse_form_query_accepts_the_keyword_spellings() {
        let keyword = parse_form_query(
            "PatientID=PT-1&StudyInstanceUID=1.2.3&AccessionNumber=ACC-1&\
             ProcedureStepLabel=CAD&\
             ScheduledStationNameCodeSequence.CodeValue=STATION01&\
             ProcedureStepState=SCHEDULED&Modality=CT&\
             ScheduledWorkitemCodeSequence.CodeValue=CT-ORGAN-SEG&\
             ReferencedRequestSequence.RequestedProcedureID=RP-1",
        )
        .expect("every key here is spelled as PS3.6 defines it");
        assert_eq!(keyword.patient_id.as_deref(), Some("PT-1"));
        assert_eq!(keyword.study_uid.as_deref(), Some("1.2.3"));
        assert_eq!(keyword.accession_number.as_deref(), Some("ACC-1"));
        assert_eq!(keyword.step_label.as_deref(), Some("CAD"));
        assert_eq!(keyword.workflow_state.as_deref(), Some("SCHEDULED"));
        assert_eq!(keyword.modality.as_deref(), Some("CT"));
        assert_eq!(keyword.requested_procedure_id.as_deref(), Some("RP-1"));
        assert_eq!(
            keyword.scheduled_station_code_value.as_deref(),
            Some("STATION01")
        );
        assert_eq!(
            keyword.scheduled_workitem_code_value.as_deref(),
            Some("CT-ORGAN-SEG")
        );

        // The sequence-qualified spellings reach the same fields as the bare
        // ones.
        let qualified = parse_form_query(
            "ReferencedRequestSequence.StudyInstanceUID=1.2.4&\
             ReferencedRequestSequence.AccessionNumber=ACC-2",
        )
        .expect("the sequence-qualified spellings");
        assert_eq!(qualified.study_uid.as_deref(), Some("1.2.4"));
        assert_eq!(qualified.accession_number.as_deref(), Some("ACC-2"));

        // The hex twin of each key parses to the SAME query as the
        // sequence-qualified spelling above (1.2.4 / ACC-2).
        let hex = parse_form_query(
            "00100020=PT-1&0040A370.0020000D=1.2.4&0040A370.00080050=ACC-2&\
             00741204=CAD&00404025.00080100=STATION01&00741000=SCHEDULED&\
             00080060=CT&00404018.00080100=CT-ORGAN-SEG",
        )
        .expect("the hex twin of the same keys");
        assert_eq!(hex.patient_id, keyword.patient_id);
        assert_eq!(hex.study_uid, qualified.study_uid);
        assert_eq!(hex.accession_number, qualified.accession_number);
        assert_eq!(hex.step_label, keyword.step_label);
        assert_eq!(hex.workflow_state, keyword.workflow_state);
        assert_eq!(hex.modality, keyword.modality);
        assert_eq!(
            hex.scheduled_station_code_value,
            keyword.scheduled_station_code_value
        );
        assert_eq!(
            hex.scheduled_workitem_code_value,
            keyword.scheduled_workitem_code_value
        );
    }

    /// RED FACT. A key that is neither a PS3.6 keyword nor a tag is REFUSED,
    /// and a keyword spelled in the wrong case is refused with it.
    ///
    /// PS3.18 8.3.4 fixes the vocabulary AND the spelling of the matching keys,
    /// so anything outside it is a malformed request. The alternative - what
    /// this parser used to do - is to drop the key and answer with the whole
    /// worklist, which turns a typo into a silent PHI disclosure: the caller
    /// cannot tell a broad match from an ignored key. dcm4chee-arc answers the
    /// same request 400 "Invalid Query Parameter".
    #[test]
    fn an_unknown_or_misspelled_key_is_refused() {
        // Keyword case is part of the spelling: PS3.6 writes `PatientID`, and
        // `patientid` is not it. The error names the offending key so a client
        // can see which of its parameters the server did not understand.
        let err = parse_form_query("patientid=PT-1").expect_err("wrong case");
        assert!(
            err.contains("patientid"),
            "the message names the key it refused: {err}"
        );
        assert!(err.contains("8.3.4"), "and cites the convention: {err}");

        // An unknown name of any shape is refused the same way.
        for key in [
            "Foo",
            "0010002",
            "referencedrequestsequence.StuDyInstanceUID",
        ] {
            let err = parse_form_query(&format!("{key}=x"))
                .expect_err(&format!("unknown key {key} must be refused"));
            assert!(err.contains(key), "the message names {key}: {err}");
        }

        // The refusal is per-key, not per-query: a request that carries a good
        // key beside a bad one is still malformed, so the bad key cannot ride
        // along with a filter and widen it unnoticed. `Bogus` stands in for the
        // misspelling; the mandatory control parameters are NOT bad keys (see
        // the_search_control_parameters_of_table_8_3_4_1_are_accepted).
        let err = parse_form_query("PatientID=PT-1&Bogus=x")
            .expect_err("an unknown attribute key cannot ride along with a filter");
        assert!(err.contains("Bogus"), "names the bad key: {err}");
    }

    /// The control parameters PS3.18 Table 8.3.4-1 marks M for the Origin
    /// Server. Its prose is "Search Transactions shall support these
    /// parameters", so a 400 on any of them is a conformance defect; each is
    /// accepted here, and accepted while a real filter is present so the
    /// parameter cannot swallow the filter beside it.
    #[test]
    fn the_search_control_parameters_of_table_8_3_4_1_are_accepted() {
        for key in [
            "fuzzymatching=false",
            "fuzzymatching=true",
            "includefield=all",
            "includefield=00080018",
            "emptyvaluematching=false",
            "multiplevaluematching=false",
        ] {
            let q = parse_form_query(&format!("PatientID=PT-1&{key}"))
                .unwrap_or_else(|e| panic!("{key} is mandatory for the origin server: {e}"));
            assert_eq!(
                q.patient_id.as_deref(),
                Some("PT-1"),
                "{key} must not displace the filter beside it"
            );
        }

        // `includefield=all` is answered by construction: the render returns
        // every attribute the store holds, so accepting it changes nothing.
        let q = parse_form_query("includefield=all").expect("accepted alone");
        assert!(q.patient_id.is_none(), "no filter was given");
    }

    /// RED FACT. The station key is accepted in the three spellings PS3.18
    /// 8.3.4 and dcm4chee between them use, and the vendor (0040,0001) - which
    /// PS3.4 Table CC.2.5-3 gives no row - is REFUSED.
    ///
    /// The station travels in exactly one place now: the Code Value of
    /// (0040,4025). Keeping a second spelling would mean two ways to state one
    /// fact and two ways for a client to get it wrong, so the vendor key is
    /// gone rather than aliased.
    #[test]
    fn the_station_key_accepts_the_standard_spellings_only() {
        for key in [
            "00404025",
            "00404025.00080100",
            "ScheduledStationNameCodeSequence.CodeValue",
        ] {
            let q = parse_form_query(&format!("{key}=CT1"))
                .unwrap_or_else(|e| panic!("{key} is a standard spelling: {e}"));
            assert_eq!(
                q.scheduled_station_code_value.as_deref(),
                Some("CT1"),
                "{key} reaches the station field"
            );
        }

        // The vendor AE-title key has no row in the matching table, so a search
        // that sends it is malformed rather than quietly answered with the
        // whole worklist.
        let err = parse_form_query("00400001=CT1").expect_err("vendor station key");
        assert!(err.contains("00400001"), "names the removed key: {err}");
        let err = parse_form_query("ScheduledStationAETitle=CT1").expect_err("vendor keyword");
        assert!(
            err.contains("ScheduledStationAETitle"),
            "names the removed keyword: {err}"
        );
    }

    /// RED FACT. (0040,4018) Scheduled Workitem Code Sequence > Code Value is a
    /// REQUIRED matching key (PS3.4 Table CC.2.5-3), so both spellings of it
    /// reach the query field rather than being dropped.
    #[test]
    fn the_scheduled_workitem_code_key_is_accepted_in_both_spellings() {
        for key in [
            "00404018.00080100",
            "ScheduledWorkitemCodeSequence.CodeValue",
        ] {
            let q = parse_form_query(&format!("{key}=CT-ORGAN-SEG"))
                .unwrap_or_else(|e| panic!("{key} is a standard spelling: {e}"));
            assert_eq!(
                q.scheduled_workitem_code_value.as_deref(),
                Some("CT-ORGAN-SEG"),
                "{key} reaches the workitem code field"
            );
        }

        // The bare sequence tag is NOT the key: PS3.4 types it as a Sequence,
        // so the key has to name the Code Value inside it.
        assert!(
            parse_form_query("00404018=CT-ORGAN-SEG").is_err(),
            "the bare sequence tag is not a matching key"
        );
    }

    /// RED FACT. (0040,A370) > (0040,1001) Requested Procedure ID is a REQUIRED
    /// matching key, accepted in the standard keyword path and its hex twin.
    #[test]
    fn the_requested_procedure_id_key_is_accepted_in_both_spellings() {
        for key in [
            "0040A370.00401001",
            "ReferencedRequestSequence.RequestedProcedureID",
        ] {
            let q = parse_form_query(&format!("{key}=RP-1"))
                .unwrap_or_else(|e| panic!("{key} is a standard spelling: {e}"));
            assert_eq!(
                q.requested_procedure_id.as_deref(),
                Some("RP-1"),
                "{key} reaches the requested-procedure field"
            );
        }

        // The bare nested tag is a DIFFERENT attribute (the performed step's),
        // so it is not accepted here.
        assert!(
            parse_form_query("00401001=RP-1").is_err(),
            "the bare (0040,1001) is not this key"
        );
    }

    /// RED FACT. `limit` and `offset` are paging controls rather than DICOM
    /// matching keys (PS3.18 8.3.4 covers the keys; 11.4.1 adds the paging
    /// parameters) and they keep working now that the `_` arm refuses.
    #[test]
    fn the_paging_keys_are_not_swept_up_by_the_refusal() {
        let q = parse_form_query("limit=25&offset=50").expect("paging keys are known");
        assert_eq!(q.limit, Some(25));
        assert_eq!(q.offset, Some(50));

        // Beside a matching key, which is how a client actually sends them.
        let q = parse_form_query("PatientID=PT-1&limit=25&offset=50").expect("mixed query");
        assert_eq!(q.patient_id.as_deref(), Some("PT-1"));
        assert_eq!(q.limit, Some(25));
        assert_eq!(q.offset, Some(50));

        // A malformed paging VALUE is not a malformed key, but it is still an
        // invalid query parameter: PS3.18 8.3.4 Table 8.3.4-1 types limit and
        // offset as uint, so a non-numeric value is a 400 rather than a
        // silent fallback to the server default.
        let err = parse_form_query("limit=not-a-number").expect_err("invalid uint is refused");
        assert!(
            err.contains("limit must be a non-negative integer"),
            "unexpected message: {err}"
        );
    }

    /// An empty query string carries no key to refuse.
    #[test]
    fn the_empty_query_is_not_an_error() {
        assert!(parse_form_query("").is_ok());
    }
}
