// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Audit codes and status values, bound to the curated standard reference.
//!
//! Every ATNA EventID, EventActionCode and ParticipantObjectIDTypeCode the
//! servers emit is named here ONCE, transcribed from
//! `standard/ps3.15/audit-message-model.json` (PS3.15 2026d A.5.3, RFC 3881 for
//! the inherited fields). The journal and the metrics surfaces reference these
//! constants instead of repeating literals, so a digit that is wrong in one
//! place cannot disagree with the same digit elsewhere.
//!
//! The tests below re-read the JSON and assert each constant against it, which
//! is what makes "sourced from `standard/`" a checked claim rather than a
//! comment.
#![forbid(unsafe_code)]

/// `EventID` values of PS3.15 A.5.3 (`DICOM Specific Audit Messages`).
pub mod event_id {
    /// A.5.3.2 `Audit Log Used` - a person or process reading an audit trail.
    pub const AUDIT_LOG_USED: &str = "110101";
    /// A.5.3.6 `DICOM Instances Accessed` - instances viewed/updated/deleted.
    pub const INSTANCES_ACCESSED: &str = "110103";
    /// A.5.3.7 `DICOM Instances Transferred` - transfer completion.
    pub const INSTANCES_TRANSFERRED: &str = "110104";
    /// A.5.3.10 `Query` - a query issued or received.
    pub const QUERY: &str = "110112";
    /// A.5.3.14 `Patient Record` - patient record touched, no instance named.
    pub const PATIENT_RECORD: &str = "110110";
    /// A.5.3.15 `Procedure Record` - a procedure record touched (MPPS).
    pub const PROCEDURE_RECORD: &str = "110111";
}

/// `EventActionCode` values (RFC 3881, constrained per message by PS3.15).
pub mod action {
    /// Create.
    pub const CREATE: &str = "C";
    /// Read.
    pub const READ: &str = "R";
    /// Update.
    pub const UPDATE: &str = "U";
    /// Delete.
    pub const DELETE: &str = "D";
    /// Execute.
    ///
    /// PS3.15 A.5 constrains this per message: `Query` (110112) and
    /// `User Authentication` (110114) mandate `E`, and `Audit Log Used`
    /// (110101) mandates `R`. A query recorded as `R` is therefore not
    /// conformant, even though the EventID itself would be right.
    pub const EXECUTE: &str = "E";
}

/// `ParticipantObjectIDTypeCode` values the journal uses.
pub mod object_id_type {
    /// RFC 3881 base code 2: `Patient Number`.
    pub const PATIENT_NUMBER: &str = "2";
    /// CID 404 `110180`: `Study Instance UID`.
    pub const STUDY_INSTANCE_UID: &str = "110180";
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The curated reference, embedded so the assertions run without a filesystem.
    const MODEL: &str = include_str!("../../standard/ps3.15/audit-message-model.json");

    /// A tiny JSON string-field reader.
    ///
    /// The module deliberately avoids pulling a JSON crate into the server for
    /// four tests: the reference is a flat array of objects with one key per
    /// field, so scanning for `"id": "NNNNNN"` and then for the next `"name"`
    /// is sufficient and does not depend on key order.
    fn event_field(id: &str, field: &str) -> Option<String> {
        let needle = format!("\"id\": \"{id}\"");
        let start = MODEL.find(&needle)?;
        let rest = &MODEL[start..];
        let end = rest.find('}').unwrap_or(rest.len());
        let object = &rest[..end];
        let key = format!("\"{field}\": ");
        let at = object.find(&key)? + key.len();
        let tail = &object[at..];
        let value = tail.strip_prefix('"')?;
        Some(value[..value.find('"')?].to_string())
    }

    /// Every event ID the journal emits exists in the reference, under the
    /// section the constant claims.
    #[test]
    fn every_event_id_is_the_standard_value() {
        for (id, name, section) in [
            (event_id::AUDIT_LOG_USED, "Audit Log Used", "A.5.3.2"),
            (
                event_id::INSTANCES_ACCESSED,
                "DICOM Instances Accessed",
                "A.5.3.6",
            ),
            (
                event_id::INSTANCES_TRANSFERRED,
                "DICOM Instances Transferred",
                "A.5.3.7",
            ),
            (event_id::QUERY, "Query", "A.5.3.10"),
            (event_id::PATIENT_RECORD, "Patient Record", "A.5.3.14"),
            (event_id::PROCEDURE_RECORD, "Procedure Record", "A.5.3.15"),
        ] {
            assert_eq!(
                event_field(id, "name").as_deref(),
                Some(name),
                "event {id} must be {name} in the standard"
            );
            assert_eq!(
                event_field(id, "section").as_deref(),
                Some(section),
                "event {id} must live at {section}"
            );
        }
    }

    /// The reference with all whitespace removed, so a value can be located
    /// without depending on how the JSON happens to be indented.
    fn compact() -> String {
        MODEL.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// Assert that `code` is paired with `meaning` somewhere in the reference.
    ///
    /// The comparison is on the whitespace-free form on BOTH sides, so the
    /// meaning may be written "Patient Number" here and stored as
    /// "PatientNumber" in the reference.
    fn assert_pair(compact: &str, code: &str, meaning: &str) {
        let meaning: String = meaning.chars().filter(|c| !c.is_whitespace()).collect();
        let pair = format!("\"code\":\"{code}\",\"meaning\":\"{meaning}\"");
        assert!(
            compact.contains(&pair),
            "the standard must pair code {code} with meaning {meaning}"
        );
    }

    /// The action codes are the RFC 3881 set the standard inherits.
    #[test]
    fn the_action_codes_are_the_standard_letters() {
        let compact = compact();
        for (code, meaning) in [
            (action::CREATE, "Create"),
            (action::READ, "Read"),
            (action::UPDATE, "Update"),
            (action::DELETE, "Delete"),
            (action::EXECUTE, "Execute"),
        ] {
            assert_pair(&compact, code, meaning);
        }
    }

    /// The object ID type codes are the RFC 3881 base 2 and CID 404 110180.
    #[test]
    fn the_object_id_type_codes_are_the_standard_values() {
        let compact = compact();
        assert_pair(&compact, object_id_type::PATIENT_NUMBER, "Patient Number");
        assert_pair(
            &compact,
            object_id_type::STUDY_INSTANCE_UID,
            "Study Instance UID",
        );
    }
}
