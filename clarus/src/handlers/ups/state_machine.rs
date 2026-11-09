// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! UPS workflow state machine (PS3.4 Annex CC, PS3.18 Section 11.7).
//!
//! The state set and the transition table are PS3.4 Table CC.1.1-1 and
//! Table CC.1.1-2 verbatim. There are four states - SCHEDULED, IN PROGRESS,
//! COMPLETED, CANCELED - and SUSPENDED is not one of them (the same set
//! appears as the Procedure Step State enumerated values in PS3.3 C.30.1).

#![forbid(unsafe_code)]

/// The four states of PS3.4 Table CC.1.1-1.
pub const VALID_STATES: &[&str] = &["SCHEDULED", "IN PROGRESS", "COMPLETED", "CANCELED"];

/// The states a Change UPS State request may name (PS3.18 11.7.1.4).
///
/// SCHEDULED is legal on the wire only as the state a workitem is created in;
/// a request that asks for it is refused with the same code as any other
/// illegal transition (Table CC.1.1-2 answers C303H for every column).
pub const CHANGE_STATES: &[&str] = &["IN PROGRESS", "COMPLETED", "CANCELED"];

/// Validate a workflow state transition per PS3.4 Table CC.1.1-2.
///
/// The table is read as: a Change State request naming `to` against a
/// workitem currently in `from`, by the holder of the lock where one is
/// required. The rows that matter here are the ones the SCP answers
/// SUCCESS to:
///
/// - SCHEDULED -> IN PROGRESS (the claim, with the TUID the SCU generated)
/// - IN PROGRESS -> COMPLETED
/// - IN PROGRESS -> CANCELED
/// - COMPLETED -> COMPLETED and CANCELED -> CANCELED (warnings B306H/B304H,
///   which are successes)
///
/// Everything else is a failure the caller maps to 400. Equal transitions to
/// SCHEDULED and IN PROGRESS are failures (C303H/C302H), not successes - the
/// distinction between the two terminal equal cases and these is why the
/// table is spelled out rather than reduced to "different states only".
pub fn is_valid_transition(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("SCHEDULED", "IN PROGRESS")
            | ("IN PROGRESS", "COMPLETED")
            | ("IN PROGRESS", "CANCELED")
            | ("COMPLETED", "COMPLETED")
            | ("CANCELED", "CANCELED")
    )
}

/// Validate that a workflow state string is one of the four known values.
pub fn is_valid_state(state: &str) -> bool {
    VALID_STATES.contains(&state)
}

/// Whether the state is one a workitem can no longer leave.
///
/// PS3.18 11.6.2 refuses an Update on a workitem in a final state, and the
/// retention sweep in `ups_retention` uses the same predicate.
pub fn is_final_state(state: &str) -> bool {
    matches!(state, "COMPLETED" | "CANCELED")
}

// -- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_states_are_the_ones_the_standard_names() {
        assert_eq!(
            VALID_STATES,
            &["SCHEDULED", "IN PROGRESS", "COMPLETED", "CANCELED"]
        );
        assert!(is_valid_state("IN PROGRESS"));
        assert!(!is_valid_state("SUSPENDED"));
        assert!(!is_valid_state("IN_PROGRESS"));
        assert!(!is_valid_state(""));
    }

    #[test]
    fn the_success_rows_of_table_cc_1_1_2_are_the_only_ones() {
        // Claim, and the two ways out of IN PROGRESS.
        assert!(is_valid_transition("SCHEDULED", "IN PROGRESS"));
        assert!(is_valid_transition("IN PROGRESS", "COMPLETED"));
        assert!(is_valid_transition("IN PROGRESS", "CANCELED"));
        // Equal terminal transitions are warnings, which are successes.
        assert!(is_valid_transition("COMPLETED", "COMPLETED"));
        assert!(is_valid_transition("CANCELED", "CANCELED"));
    }

    #[test]
    fn equal_transitions_outside_the_terminal_states_are_refused() {
        // C302H for IN PROGRESS and C303H for SCHEDULED.
        assert!(!is_valid_transition("IN PROGRESS", "IN PROGRESS"));
        assert!(!is_valid_transition("SCHEDULED", "SCHEDULED"));
    }

    #[test]
    fn state_may_not_be_set_without_a_claim() {
        // CC.1.1 Note: a SCHEDULED instance is not set to COMPLETED or
        // CANCELED without first being set to IN PROGRESS.
        assert!(!is_valid_transition("SCHEDULED", "COMPLETED"));
        assert!(!is_valid_transition("SCHEDULED", "CANCELED"));
    }

    #[test]
    fn final_states_are_terminal() {
        assert!(!is_valid_transition("COMPLETED", "IN PROGRESS"));
        assert!(!is_valid_transition("CANCELED", "IN PROGRESS"));
        assert!(!is_valid_transition("COMPLETED", "CANCELED"));
        assert!(!is_valid_transition("CANCELED", "COMPLETED"));
    }

    #[test]
    fn in_progress_cannot_go_back_to_scheduled() {
        assert!(!is_valid_transition("IN PROGRESS", "SCHEDULED"));
    }

    #[test]
    fn final_state_predicate_matches_the_table() {
        assert!(is_final_state("COMPLETED"));
        assert!(is_final_state("CANCELED"));
        assert!(!is_final_state("SCHEDULED"));
        assert!(!is_final_state("IN PROGRESS"));
    }
}
