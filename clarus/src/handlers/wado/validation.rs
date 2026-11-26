// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! UID validation for WADO-RS endpoints.
//!
//! Provides [`validate_dicom_uid`] for path-traversal-safe DICOM UID
//! validation and [`validate_wado_params`] for shared query parameter
//! validation.

#![forbid(unsafe_code)]

use super::error::WadoError;
use super::types::WadoQueryParams;

/// Maximum allowed length for a DICOM UID (64 chars per PS3.5).
pub const UID_MAX_LEN: usize = 64;

/// Validate common WADO-RS query parameters (annotation, charset, RetrieveAE).
///
/// Shared by `retrieve_study` and `retrieve_series` to eliminate copy-paste.
/// Returns `Ok(())` if all parameters are valid or absent.
pub fn validate_wado_params(query: &WadoQueryParams) -> Result<(), WadoError> {
    // R-05: annotation not supported
    if query.annotation.is_some() {
        return Err(WadoError::NotAcceptable(
            "Parameter 'annotation' is not supported.".into(),
        ));
    }
    // R-06: charset validation
    if let Some(ref cs) = query.charset
        && cs.to_lowercase() != "utf-8"
    {
        return Err(WadoError::NotAcceptable(format!(
            "Charset '{}' not supported; only utf-8.",
            cs
        )));
    }
    // R-10: RetrieveAE not supported
    if query.retrieve_ae.is_some() {
        return Err(WadoError::NotAcceptable(
            "RetrieveAE not supported. Use Clarus Bridge for DIMSE C-MOVE integration.".into(),
        ));
    }
    Ok(())
}

/// Validate a DICOM UID according to the invariants.
///
/// # Rules
/// - Only ASCII digits and dots: `^[0-9.]+$`
/// - Max 64 characters
/// - No consecutive dots `..` (blocks `../` traversal)
/// - No dot-slash `./` (blocks `./hidden` traversal)
/// - No null bytes `\x00`
pub fn validate_dicom_uid(uid: &str) -> Result<(), WadoError> {
    // Check 1: not empty
    if uid.is_empty() {
        return Err(WadoError::InvalidUid("(empty)".to_string()));
    }

    // Check 2: max length
    if uid.len() > UID_MAX_LEN {
        return Err(WadoError::InvalidUid(uid.to_string()));
    }

    // Check 3: null bytes
    if uid.contains('\x00') {
        return Err(WadoError::InvalidUid(uid.to_string()));
    }

    // Check 4: only digits and dots
    if !uid.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(WadoError::InvalidUid(uid.to_string()));
    }

    // Check 5: no consecutive dots (blocks ../, ..\ traversal)
    if uid.contains("..") {
        return Err(WadoError::InvalidUid(uid.to_string()));
    }

    // Check 6: no dot-slash (blocks ./hidden traversal)
    if uid.contains("./") {
        return Err(WadoError::InvalidUid(uid.to_string()));
    }

    // Check 7: no backslash (blocks Windows path traversal)
    if uid.contains('\\') || uid.contains(".\\") {
        return Err(WadoError::InvalidUid(uid.to_string()));
    }

    // Check 8: COMPONENT rules, not just the character set.
    //
    // PS3.5 9.1 (`standard/ps3.5/uid-encoding-rules.json`, UID-1/UID-2):
    // every component is "one or more digits", and its first digit must not be
    // zero unless the component is a single digit. The character checks above
    // admit `.1`, `1.` and `1.02`, which are all malformed but pass the lookup
    // - so an invalid UID became a 404 "not found" instead of the correct
    // 400-class InvalidUid.
    for component in uid.split('.') {
        if component.is_empty() {
            // Leading, trailing or doubled '.'. UID-1.
            return Err(WadoError::InvalidUid(uid.to_string()));
        }
        // UID-1 is already implied by the digits-only check above, but the
        // leading-zero rule is new: multi-digit components may not start '0'.
        if component.len() > 1 && component.starts_with('0') {
            return Err(WadoError::InvalidUid(uid.to_string()));
        }
    }

    Ok(())
}

// -- Unit tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_standard_uid() {
        assert!(validate_dicom_uid("1.2.840.10008.5.1.4.1.1.2").is_ok());
        assert!(validate_dicom_uid("1.2.840.113619.2.5.1762383155.1317.1160492719").is_ok());
        assert!(validate_dicom_uid("1").is_ok());
        // A single-digit component may be ZERO (PS3.5 9.1 UID-2), and a
        // multi-digit component may not start with one.
        assert!(
            validate_dicom_uid("1.0.2").is_ok(),
            "single-digit zero is legal"
        );
        assert!(validate_dicom_uid("0").is_ok(), "a lone zero is legal");
    }

    /// Component structure is checked, not just the character set.
    ///
    /// PS3.5 9.1 UID-1/UID-2 (`standard/ps3.5/uid-encoding-rules.json`): each
    /// component is one or more digits, and its first digit is not zero unless
    /// it is a single-digit component. `.1`, `1.` and `1.02` passed the
    /// digits-and-dots check, so an invalid UID reached lookup and answered
    /// "not found" instead of being refused.
    #[test]
    fn test_uid_component_structure() {
        // Empty components: leading, trailing, doubled.
        assert!(validate_dicom_uid(".1").is_err(), "leading empty component");
        assert!(
            validate_dicom_uid("1.").is_err(),
            "trailing empty component"
        );
        assert!(validate_dicom_uid("1..2").is_err(), "doubled dot");
        assert!(validate_dicom_uid(".").is_err());
        // Leading zero in a MULTI-digit component.
        assert!(validate_dicom_uid("1.02").is_err(), "leading zero in '02'");
        assert!(validate_dicom_uid("007").is_err(), "leading zeros");
        assert!(validate_dicom_uid("1.2.03.4").is_err());
        // The single-digit-zero form stays valid, so the fix is not a blanket
        // ban on zeros inside a UID.
        assert!(validate_dicom_uid("1.2.0.4").is_ok());
        // A 64-char UID is still accepted; 65 is not. Built from exact pieces
        // so the length is arithmetic, not hand-counted.
        let ok64 = format!("1.{}1", "2".repeat(61));
        assert_eq!(ok64.len(), 64, "fixture is exactly 64");
        assert!(validate_dicom_uid(&ok64).is_ok());
        assert!(validate_dicom_uid(&format!("{ok64}5")).is_err());
        // Path separators are still refused.
        assert!(validate_dicom_uid("1.2/3").is_err());
        assert!(validate_dicom_uid("1.2\\3").is_err());
    }

    #[test]
    fn test_empty_uid() {
        assert!(validate_dicom_uid("").is_err());
    }

    #[test]
    fn test_null_byte() {
        assert!(validate_dicom_uid("1.2.840\x00.test").is_err());
    }

    #[test]
    fn test_double_dot_path_traversal() {
        assert!(validate_dicom_uid("../../etc/passwd").is_err());
        assert!(validate_dicom_uid("1.2..840").is_err());
    }

    #[test]
    fn test_dot_slash_path_traversal() {
        assert!(validate_dicom_uid("./hidden").is_err());
        assert!(validate_dicom_uid("1.2.840./hidden").is_err());
    }

    #[test]
    fn test_backslash_path_traversal() {
        assert!(validate_dicom_uid(r"..\windows\system32").is_err());
        assert!(validate_dicom_uid(r".\hidden").is_err());
    }

    #[test]
    fn test_non_digit_non_dot_chars() {
        assert!(validate_dicom_uid("abc.def").is_err());
        assert!(validate_dicom_uid("1.2;840").is_err());
        assert!(validate_dicom_uid("1.2 840").is_err());
        assert!(validate_dicom_uid("1/2/840").is_err());
    }

    #[test]
    fn test_max_length() {
        let valid_64 = "1".repeat(64);
        assert!(validate_dicom_uid(&valid_64).is_ok());

        let too_long = "1".repeat(65);
        assert!(validate_dicom_uid(&too_long).is_err());
    }
}
