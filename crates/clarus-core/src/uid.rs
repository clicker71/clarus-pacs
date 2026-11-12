// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Validation of a DICOM UID that is about to become a path component.
//!
//! PS3.5 9.1: a UID is digits and dots, at most 64 characters. The alphabet is
//! what makes a UID safe as a file stem: a value drawn from `[0-9.]` cannot
//! encode a separator, a drive letter, a percent-escape or a `..` component, so
//! it cannot address a file outside the directory it is joined onto. The
//! standard puts NO length bound on an individual component - the 32-bit range
//! belongs to one ENCODING of a component, not to the syntax - and real vendor
//! UIDs carry components of 15, 20 and 38 digits.
//!
//! The refusal here is deliberate and distinct from
//! [`crate::commitment::CommitmentStore::path_for`], which SANITISES by
//! replacing foreign characters with `_`. A sanitiser answers "write it
//! somewhere safe"; a caller that has just received an operator's UID needs
//! "tell me this is not a UID", because a rewritten UID addresses a different
//! object than the one the operator named. The two policies are not
//! interchangeable and must not be merged.

/// Why a string cannot be used as a UID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UidError {
    /// The value was empty.
    Empty,
    /// The value carried a character outside `[0-9.]`, or a component was
    /// empty (PS3.5 9.1).
    Malformed,
}

impl std::fmt::Display for UidError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UidError::Empty => write!(f, "UID is empty"),
            UidError::Malformed => {
                write!(f, "UID must be digits and dots, with no empty component")
            }
        }
    }
}

impl std::error::Error for UidError {}

/// Check that `uid` is a DICOM UID suitable as a path component.
///
/// Rejects `.`, `..`, an embedded separator, a leading or trailing dot, two
/// consecutive dots and an empty component. It also rejects a multi-digit
/// component that starts with `0` (PS3.5 9.1, rule UID-2: "The first digit of
/// each component shall not be zero unless the component is a single digit");
/// a lone `0` component stays legal. It does NOT bound a component's digit
/// count: PS3.5 9.1 does not (UID-4 is the only length rule, and it is the
/// 64-character ceiling enforced below), and our own
/// `1.2.826.0.1.3680043.8.498` branch produces 38-digit components that this
/// function must accept.
pub fn validate_path_uid(uid: &str) -> Result<(), UidError> {
    if uid.is_empty() {
        return Err(UidError::Empty);
    }
    if uid.len() > 64 {
        // PS3.5 9.1: a UID is at most 64 characters.
        return Err(UidError::Malformed);
    }
    // The walk has to know two things: whether a component is empty (a leading
    // dot, a trailing dot or two dots in a row all leave a gap) and whether the
    // current component opened with a `0` that a later digit would make
    // non-conforming. The digit count itself carries no rule.
    let mut component_len = 0usize;
    let mut leading_zero = false;
    for byte in uid.bytes() {
        match byte {
            b'.' => {
                if component_len == 0 {
                    // Leading dot, or two dots in a row.
                    return Err(UidError::Malformed);
                }
                component_len = 0;
                leading_zero = false;
            }
            b'0' if component_len == 0 => {
                leading_zero = true;
                component_len += 1;
            }
            b'0'..=b'9' => {
                if leading_zero {
                    // PS3.5 9.1 (UID-2): a multi-digit component may not start
                    // with '0'. A single '0' component is legal.
                    return Err(UidError::Malformed);
                }
                component_len += 1;
            }
            _ => return Err(UidError::Malformed),
        }
    }
    if component_len == 0 {
        // Trailing dot.
        return Err(UidError::Malformed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_uid_is_accepted() {
        assert!(validate_path_uid("1.2.840.10008.5.1.4.1.1.2").is_ok());
        assert!(validate_path_uid("1").is_ok());
        assert!(validate_path_uid("123456789.1").is_ok());
    }

    /// PS3.5 9.1 bounds the UID, not a component. Measured against
    /// `D:\criterion`: 1612 UIDs carry a component longer than nine digits, and
    /// this branch of our own prefix produces a 38-digit one. Rejecting them
    /// made admin delete and restore answer 400 for real studies.
    #[test]
    fn a_component_longer_than_nine_digits_is_accepted() {
        // Our own 1.2.826.0.1.3680043.8.498 branch: 38 digits in the last
        // component.
        assert!(
            validate_path_uid("1.2.826.0.1.3680043.8.498.43960911271980867431420083030440798847")
                .is_ok(),
            "the 38-digit component is legal"
        );
        // Vendor shape with a 15-digit and a 14-digit component.
        assert!(
            validate_path_uid("1.3.6.1.4.1.25403.246788402477788.12876.20260812120435.1").is_ok(),
            "15- and 14-digit components are legal"
        );
        assert!(
            validate_path_uid("1234567890").is_ok(),
            "ten digits, one component"
        );
        assert!(validate_path_uid("1.1234567890123456789012345678901234567890").is_ok());
    }

    #[test]
    fn path_escapes_are_refused() {
        for bad in [
            "..",
            ".",
            "../index",
            "../../../etc/passwd",
            "..\\index",
            "1.2/3",
            "1.2 3",
            "1.2\x003",
            "/absolute",
            ".1.2",
            "1.2.",
            "1..2",
            "",
            "1.2\\3",
            "1.2%2F3",
        ] {
            assert!(
                validate_path_uid(bad).is_err(),
                "{bad:?} must not be usable as a path component"
            );
        }
    }

    /// The only length rule left is the UID's own ceiling (PS3.5 9.1).
    #[test]
    fn an_over_long_uid_is_refused() {
        let long = std::iter::repeat_n("1234567", 10)
            .collect::<Vec<_>>()
            .join(".");
        assert!(validate_path_uid(&long).is_err());
        // Exactly 64 characters is still legal.
        let at_limit = "1".repeat(64);
        assert!(
            validate_path_uid(&at_limit).is_ok(),
            "64 chars is the ceiling"
        );
        let over_limit = "1".repeat(65);
        assert!(validate_path_uid(&over_limit).is_err());
    }

    /// PS3.5 9.1: "The first digit of each component shall not be zero unless
    /// the component is a single digit." A multi-digit component with a leading
    /// zero is not a UID, so it must not be usable as a path component either.
    #[test]
    fn a_leading_zero_in_a_multi_digit_component_is_refused() {
        assert!(
            validate_path_uid("1.02").is_err(),
            "UID-2: a multi-digit component may not start with '0'"
        );
        assert!(
            validate_path_uid("01.2").is_err(),
            "UID-2 applies to the first component too"
        );
        // A single-digit `0` component is legal, anywhere in the UID.
        assert!(validate_path_uid("0").is_ok(), "a lone zero is legal");
        assert!(
            validate_path_uid("1.2.3.0.4").is_ok(),
            "interior zero is legal"
        );
        assert!(
            validate_path_uid("1.2.0.4").is_ok(),
            "interior zero is legal"
        );
    }
}
