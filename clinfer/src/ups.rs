// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The UPS lifecycle, as a decision function.
//!
//! Everything here is a pure function of what the sidecar knows: a
//! workitem, the configuration, and whether an encoder is built. Nothing
//! performs I/O, so the whole lifecycle is testable without a server, and
//! the worker that applies these decisions stays a thin loop.
//!
//! The state names are the standard's own (PS3.18 11, Unified Procedure
//! Step State): SCHEDULED, IN PROGRESS, COMPLETED, CANCELED.

use crate::conf::{Conf, DefaultRule};
use crate::matcher::{self, StudyAttrs};

/// The states a workitem moves through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Scheduled,
    InProgress,
    Completed,
    Canceled,
}

impl State {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "SCHEDULED",
            Self::InProgress => "IN PROGRESS",
            Self::Completed => "COMPLETED",
            Self::Canceled => "CANCELED",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_uppercase().as_str() {
            "SCHEDULED" => Some(Self::Scheduled),
            "IN PROGRESS" => Some(Self::InProgress),
            "COMPLETED" => Some(Self::Completed),
            "CANCELED" | "CANCELLED" => Some(Self::Canceled),
            _ => None,
        }
    }
}

/// Why a workitem will not produce a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The study matches at least one rule and this build can produce
    /// every matched result.
    ///
    /// ALL matching rules, in file order: the chain is firewall-like, so
    /// each matching rule delivers its own result rather than the first
    /// match winning. A single-rule deployment sees a one-element list and
    /// behaves exactly as before.
    Infer { rule_indices: Vec<usize> },
    /// No rule matched and the configured fallback is to cancel.
    NoRuleCancel,
    /// No rule matched and the configured fallback is to leave it alone.
    NoRuleOff,
    /// A rule matched, but this build has no encoder for its result kind.
    ///
    /// Refusing here is the honest alternative to a silent success: the
    /// workitem is closed with a reason an operator can read, rather than
    /// left to look like an inference that produced nothing.
    EncoderNotBuilt { rule_index: usize, kind: String },
}

/// What the worker should do with a workitem it has just claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Leave the workitem claimed and run every matching rule's result
    /// encoder, in this order.
    Infer { rule_indices: Vec<usize> },
    /// Cancel it with this reason.
    Cancel { reason: String },
    /// Do nothing: the workitem stays as it is.
    Ignore,
}

/// Decide what happens to a claimed workitem.
///
/// The reason strings are operator-facing: they name the configured
/// fallback, or the missing encoder and the kind it was asked for. A
/// generic message would leave an operator with no way to tell a
/// misconfigured rule from a missing build.
///
/// The FIRST rule with a missing encoder decides the whole outcome: the
/// chain delivers every matched result or nothing, so a study whose
/// detector can be encoded and whose report cannot must not be half
/// delivered. The refusal names that rule.
#[must_use]
pub fn decide(conf: &Conf, attrs: &StudyAttrs) -> Outcome {
    let matched = matcher::matching_rules(conf, attrs);
    if matched.is_empty() {
        return match conf.default_rule {
            DefaultRule::Cancel => Outcome::NoRuleCancel,
            DefaultRule::Off => Outcome::NoRuleOff,
        };
    }
    for rule in &matched {
        if !rule.result.kind.encoder_built() {
            return Outcome::EncoderNotBuilt {
                rule_index: rule.index,
                kind: rule.result.kind.as_str().to_string(),
            };
        }
    }
    Outcome::Infer {
        rule_indices: matched.iter().map(|rule| rule.index).collect(),
    }
}

/// Turn an outcome into the action the worker performs.
#[must_use]
pub fn action(outcome: Outcome) -> Action {
    match outcome {
        Outcome::Infer { rule_indices } => Action::Infer { rule_indices },
        Outcome::NoRuleCancel => Action::Cancel {
            reason: "no rule matched this study (default_rule: cancel)".to_string(),
        },
        Outcome::NoRuleOff => Action::Ignore,
        Outcome::EncoderNotBuilt { rule_index, kind } => Action::Cancel {
            reason: format!("'{kind}' encoder not built (rules[{rule_index}])"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The reference configuration, which has one `pr` rule on DX/CR.
    fn reference() -> Conf {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("clinfer.conf.example.en");
        crate::conf::load(&path).expect("the shipped example parses")
    }

    fn matching_study() -> StudyAttrs {
        StudyAttrs {
            modalities: vec!["DX".into()],
            study_description: "CHEST PA".into(),
            ..StudyAttrs::default()
        }
    }

    #[test]
    fn a_study_the_rule_recognises_is_handed_to_inference() {
        let conf = reference();
        assert_eq!(
            decide(&conf, &matching_study()),
            Outcome::Infer {
                rule_indices: vec![0]
            }
        );
        assert_eq!(
            action(Outcome::Infer {
                rule_indices: vec![0]
            }),
            Action::Infer {
                rule_indices: vec![0]
            }
        );
    }

    #[test]
    fn every_matching_rule_is_handed_to_inference_in_file_order() {
        // The chain semantics: a second identical rule is a second
        // delivery, not a duplicate the first match suppresses.
        let mut conf = reference();
        let second = conf.rules[0].clone();
        conf.rules.push(crate::conf::Rule { index: 1, ..second });
        assert_eq!(
            decide(&conf, &matching_study()),
            Outcome::Infer {
                rule_indices: vec![0, 1]
            }
        );
    }

    #[test]
    fn every_matching_rule_reaches_inference_when_all_are_encodable() {
        // The chain semantics: a second identical rule is a second
        // delivery, not a duplicate the first match suppresses. Both are
        // encodable, so the whole chain is handed over - a chain with an
        // unencodable member is covered by the guard test below.
        let mut conf = reference();
        let second = conf.rules[0].clone();
        conf.rules.push(crate::conf::Rule { index: 1, ..second });
        conf.rules[1].result.kind = crate::conf::ResultKind::Sr;
        assert_eq!(
            decide(&conf, &matching_study()),
            Outcome::Infer {
                rule_indices: vec![0, 1]
            }
        );
    }

    #[test]
    fn no_match_takes_the_configured_fallback() {
        let conf = reference();
        let attrs = StudyAttrs {
            modalities: vec!["MR".into()],
            ..StudyAttrs::default()
        };
        assert_eq!(decide(&conf, &attrs), Outcome::NoRuleCancel);
        match action(Outcome::NoRuleCancel) {
            Action::Cancel { reason } => assert!(reason.contains("no rule matched"), "{reason}"),
            other => panic!("expected a cancel, got {other:?}"),
        }
    }

    #[test]
    fn the_off_fallback_leaves_the_workitem_alone() {
        let mut conf = reference();
        conf.default_rule = DefaultRule::Off;
        let attrs = StudyAttrs {
            modalities: vec!["MR".into()],
            ..StudyAttrs::default()
        };
        assert_eq!(decide(&conf, &attrs), Outcome::NoRuleOff);
        assert_eq!(action(Outcome::NoRuleOff), Action::Ignore);
    }

    #[test]
    fn no_rule_reaches_the_encoder_guard_because_every_kind_has_one() {
        // The guard exists for a future build that declares a kind before
        // it writes the encoder. Today every kind in the enum is written
        // by this crate, so no configuration reaches it - and this test is
        // what says so out loud. A new kind added without an encoder turns
        // `encoder_built` false and lands here as a failure, which is the
        // signal to either write the encoder or expect the cancel.
        for kind in [
            crate::conf::ResultKind::Pr,
            crate::conf::ResultKind::Sr,
            crate::conf::ResultKind::Seg,
            crate::conf::ResultKind::Stl,
            crate::conf::ResultKind::Derived,
        ] {
            assert!(kind.encoder_built(), "{kind} has an encoder");
            let mut conf = reference();
            conf.rules[0].result.kind = kind;
            assert!(
                matches!(decide(&conf, &matching_study()), Outcome::Infer { .. }),
                "{kind} must reach the pipeline, not the guard"
            );
        }
    }

    #[test]
    fn an_unencodable_kind_cancels_with_the_kind_named() {
        // The outcome is constructed rather than reached, because no
        // `ResultKind` can produce it while every kind has an encoder. What
        // is being pinned is the LIFECYCLE's answer: a rule this build
        // cannot encode cancels with the kind and the rule index named,
        // rather than being claimed and failing later.
        match action(Outcome::EncoderNotBuilt {
            rule_index: 0,
            kind: "seg".to_string(),
        }) {
            Action::Cancel { reason } => {
                assert!(reason.contains("encoder not built"), "{reason}");
                assert!(reason.contains("seg"), "{reason}");
                assert!(reason.contains("rules[0]"), "{reason}");
            }
            other => panic!("expected a cancel, got {other:?}"),
        }
    }

    #[test]
    fn states_round_trip_through_their_wire_spelling() {
        for state in [
            State::Scheduled,
            State::InProgress,
            State::Completed,
            State::Canceled,
        ] {
            assert_eq!(State::parse(state.as_str()), Some(state));
        }
        // The spelling this code writes is the one the origin compares.
        assert_eq!(State::InProgress.as_str(), "IN PROGRESS");
        // A tolerant reader, for an origin that spells it the other way.
        assert_eq!(State::parse("cancelled"), Some(State::Canceled));
        assert_eq!(State::parse(" in progress "), Some(State::InProgress));
        assert_eq!(State::parse("DONE"), None);
    }
}
