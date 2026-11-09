// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Rule matching.
//!
//! Matching is a function of the study attributes and the rule list, with
//! no server, no file and no clock involved, so the table in the reference
//! configuration can be replayed in a test exactly as it runs in the field.
//!
//! The comparison is its own, not the archive's search matcher: that one
//! trims `%` for SQL `LIKE`, and a rule author writes `*chest*`. Lower-case
//! on both sides, runs of spaces collapsed, `*` stands for any run of
//! characters, and the first matching rule wins.
//!
//! Modality is the one mandatory condition. Every other key a rule may
//! name - description, coded meaning, tag binding - is alternative evidence
//! of the SAME fact, "this is the study type the model was trained for", so
//! they are OR-ed: a rule naming three of them matches a study that satisfies
//! any one. A device that writes a clean StudyDescription and an empty
//! coded meaning, and one that does the reverse, are both recognised by the
//! same rule.
//!
//! BodyPartExamined (0018,0015) is NOT one of these study-level signals: it
//! is a SERIES-level attribute (PS3.3 General Series Module C.7.3.1) and an
//! origin does not return it on the study query, so a rule that names it is
//! recognised at the study level by code/modality alone and the body-part
//! constraint is applied per series by the worker's series filter.

use crate::conf::{Conf, Rule, TagBinding};

/// The attributes a rule may look at. Every field is read from the study
/// the sidecar already has in hand; matching issues no requests.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StudyAttrs {
    /// (0008,0061) ModalitiesInStudy, upper-cased, one entry per value.
    ///
    /// NOT (0008,0060) Modality: 2026c defines that as a SERIES-level
    /// matching attribute, and the study-level key is this multi-valued
    /// one (PS3.18 Table 10.6.1-5). A study of one modality carries a
    /// one-element list, so the single-valued case is not a special case.
    pub modalities: Vec<String>,
    /// (0008,1030). Frequently absent.
    pub study_description: String,
    /// (0018,0015) from a series row, joined; NOT a study-level signal.
    ///
    /// BodyPartExamined is a SERIES attribute (PS3.3 C.7.3.1), so a rule
    /// that names it is applied by the worker's series filter, not by
    /// [`matches_rule`]. The field is kept for the series-side readers.
    pub body_part: String,
    /// (0008,0104) values found inside the coded sequences, joined.
    pub code_meanings: Vec<String>,
    /// Values bound to a tag by the site's own rule, keyed by the
    /// canonical eight-digit form.
    pub tags: Vec<(String, String)>,
    /// (0040,4018) Scheduled Workitem Code Sequence Code Value, upper-cased.
    ///
    /// Read from the WORKITEM, not from the study: the workitem carries its
    /// own intent, so a rule that constrains only the code needs no study
    /// fetch at all. `None` means the workitem declared no code, which no
    /// code-constrained rule can match.
    pub code: Option<String>,
}

/// The outcome of matching one study.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision<'a> {
    /// The first rule that matched.
    Run(&'a Rule),
    /// No rule matched; the configured fallback applies.
    Fallback,
}

/// Find the first rule whose conditions all hold.
#[must_use]
pub fn decide<'a>(conf: &'a Conf, attrs: &StudyAttrs) -> Decision<'a> {
    match conf.rules.iter().find(|rule| matches_rule(rule, attrs)) {
        Some(rule) => Decision::Run(rule),
        None => Decision::Fallback,
    }
}

/// Every rule that recognises the study, in file order.
///
/// The chain semantics: each matching rule delivers its own result, so a
/// study can carry a detector's overlay AND an endpoint's report. The
/// order is the file's, which is what an administrator reads when deciding
/// which result is produced first.
///
/// An empty vector is the same fact [`Decision::Fallback`] reports; the
/// caller then applies `default_rule`.
#[must_use]
pub fn matching_rules<'a>(conf: &'a Conf, attrs: &StudyAttrs) -> Vec<&'a Rule> {
    conf.rules
        .iter()
        .filter(|rule| matches_rule(rule, attrs))
        .collect()
}

/// Whether `rule` recognises `attrs`.
///
/// The CODE is checked first, because it comes from the workitem and needs no
/// study fetch: a rule constrained by code alone answers without the study
/// ever being read. It is not required - an empty list constrains nothing,
/// which is what every rule meant before the key existed.
///
/// The modality must then match: a rule that does not name one of the study's
/// modalities never applies to it. The comparison is a set intersection,
/// because a study may carry several modalities (0008,0061). Beyond that
/// the rule is a list of alternative signals, and any one of them that
/// matches is enough. A study that declares no modality at all satisfies
/// no modality-constrained rule - an unknown modality is not a match.
#[must_use]
pub fn matches_rule(rule: &Rule, attrs: &StudyAttrs) -> bool {
    let m = &rule.r#match;

    // The code, when the rule names one. The comparison is case-insensitive
    // and exact: a code is a machine key, so `cxr-ai` and `CXR-AI` are the
    // same code while `CXR-AI-2` is a different (unmatched) one.
    if !m.code.is_empty()
        && !m.code.iter().any(|code| {
            attrs
                .code
                .as_deref()
                .is_some_and(|have| have.eq_ignore_ascii_case(code))
        })
    {
        return false;
    }

    if !m.modality.is_empty()
        && !m.modality.iter().any(|modality| {
            attrs
                .modalities
                .iter()
                .any(|study| study.eq_ignore_ascii_case(modality))
        })
    {
        return false;
    }

    // No optional key named at all: the code and the modality alone identify
    // the task, which is what a site with a single purpose-built protocol
    // wants. This is also the CATCH-ALL shape - a rule that constrains
    // nothing matches every workitem.
    if m.study_description_contains.is_empty()
        && m.code_meaning_contains.is_empty()
        && m.tags.is_empty()
    {
        return true;
    }

    any_pattern(&m.study_description_contains, &attrs.study_description)
        || any_pattern_any(&m.code_meaning_contains, &attrs.code_meanings)
        || m.tags
            .iter()
            .any(|binding| binding_matches(binding, &attrs.tags))
}

/// A pattern list matches when any of its patterns matches the value.
///
/// An empty list is false here rather than true: these keys are OR-ed, so
/// "no patterns" means this signal says nothing, and a signal that says
/// nothing must not by itself satisfy the group.
fn any_pattern(patterns: &[String], value: &str) -> bool {
    patterns.iter().any(|pattern| glob(pattern, value))
}

fn any_pattern_any(patterns: &[String], values: &[String]) -> bool {
    values
        .iter()
        .any(|value| patterns.iter().any(|pattern| glob(pattern, value)))
}

/// A tag binding matches when the study carries the tag and its value
/// matches one of the patterns.
fn binding_matches(binding: &TagBinding, tags: &[(String, String)]) -> bool {
    tags.iter()
        .filter(|(tag, _)| tag.eq_ignore_ascii_case(&binding.tag))
        .any(|(_, value)| binding.patterns.iter().any(|p| glob(p, value)))
}

/// Case-insensitive match with `*` standing for any run of characters,
/// spaces collapsed on both sides.
///
/// The collapse is done here rather than at parse time because the value
/// comes from the image, where a device may pad a description to a fixed
/// field width. Comparing `"Chest  PA"` with `"*chest pa*"` without it
/// would depend on how many spaces the device happened to send.
#[must_use]
pub fn glob(pattern: &str, value: &str) -> bool {
    let pattern = normalize(pattern);
    let value = normalize(value);
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();

    // Iterative wildcard match: `star` remembers the last `*`, `retry`
    // the value position to resume from. Backtracking is bounded by the
    // value length, and no recursion means a pathological pattern cannot
    // exhaust the stack.
    let (mut p, mut v) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut retry = 0usize;
    while v < value.len() {
        if p < pattern.len() && pattern[p] == value[v] {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            retry = v;
            p += 1;
        } else if let Some(star_pos) = star {
            p = star_pos + 1;
            retry += 1;
            v = retry;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// Lower-case and collapse whitespace runs to a single space, trimmed.
fn normalize(text: &str) -> String {
    let lowered = text.to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut in_space = false;
    for c in lowered.chars() {
        if c.is_whitespace() {
            in_space = true;
            continue;
        }
        if in_space && !out.is_empty() {
            out.push(' ');
        }
        in_space = false;
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_star_matches_any_run_including_none() {
        assert!(glob("*chest*", "chest pa"));
        assert!(glob("*chest*", "PA CHEST AP"));
        assert!(glob("chest*", "CHEST"));
        assert!(glob("*thorax", "pa thorax"));
        assert!(glob("*", "anything"));
        assert!(!glob("*thorax*", "chest pa"));
        assert!(!glob("chest", "chest pa"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(glob("*CHEST*", "chest"));
        assert!(glob("*chest*", "CHEST"));
    }

    #[test]
    fn whitespace_runs_are_collapsed() {
        assert!(glob("*chest pa*", "CHEST   PA"));
        assert!(glob("*chest*pa*", "chest  pa"));
        assert!(glob(" *chest* ", " chest "));
    }

    #[test]
    fn a_pattern_without_a_star_is_an_exact_comparison() {
        assert!(glob("dx", "DX"));
        assert!(!glob("dx", "DX pa"));
    }

    #[test]
    fn a_trailing_star_may_match_nothing() {
        assert!(glob("ab*", "ab"));
        assert!(glob("*ab*", "ab"));
        assert!(!glob("ab*c", "ab"));
    }

    #[test]
    fn backtracking_finds_a_late_match() {
        // Without backtracking the first `*` would consume "a" and fail.
        assert!(glob("*a*ab", "aaab"));
        assert!(glob("*bc", "abc"));
        assert!(!glob("*bc", "abd"));
    }

    // -- Optional keys are alternatives ---------------------------------

    use crate::conf::{LabelProfile, Match, ResultKind, RuleResult};

    /// A rule naming all three study-level optional signals at once, so the
    /// tests vary which one the study actually satisfies. `body_part_contains`
    /// is deliberately absent: it is not a study-level signal (PS3.3 C.7.3.1)
    /// and is applied per series by the worker's series filter.
    fn rule_with_all_signals() -> Rule {
        Rule {
            r#match: Match {
                modality: vec!["DX".into()],
                code: Vec::new(),
                study_description_contains: vec!["*chest*".into()],
                code_meaning_contains: vec!["*thorax*".into()],
                body_part_contains: Vec::new(),
                tags: vec![TagBinding {
                    tag: "00211109".into(),
                    patterns: vec!["*chest*".into()],
                }],
            },
            model: "m".into(),
            result: RuleResult {
                kind: ResultKind::Pr,
                color: Some("#fff".into()),
                profile_version: 1,
                series_description: None,
                label: LabelProfile::default(),
                sr: None,
                measurements: None,
            },
            index: 0,
        }
    }

    /// The same shape with no optional signal named.
    fn rule_with_modality_only() -> Rule {
        Rule {
            r#match: Match {
                modality: vec!["DX".into(), "CR".into()],
                code: Vec::new(),
                study_description_contains: Vec::new(),
                code_meaning_contains: Vec::new(),
                body_part_contains: Vec::new(),
                tags: Vec::new(),
            },
            model: "m".into(),
            result: RuleResult {
                kind: ResultKind::Pr,
                color: Some("#fff".into()),
                profile_version: 1,
                series_description: None,
                label: LabelProfile::default(),
                sr: None,
                measurements: None,
            },
            index: 0,
        }
    }

    #[test]
    fn any_one_named_signal_is_enough() {
        let rule = rule_with_all_signals();
        let base = StudyAttrs {
            modalities: vec!["DX".into()],
            ..StudyAttrs::default()
        };

        // Each signal alone, with every other one absent.
        let only_description = StudyAttrs {
            study_description: "CHEST PA".into(),
            ..base.clone()
        };
        assert!(matches_rule(&rule, &only_description));

        let only_code = StudyAttrs {
            code_meanings: vec!["Thorax".into()],
            ..base.clone()
        };
        assert!(matches_rule(&rule, &only_code));

        let only_tag = StudyAttrs {
            tags: vec![("00211109".into(), "CHEST".into())],
            ..base.clone()
        };
        assert!(matches_rule(&rule, &only_tag));
    }

    #[test]
    fn body_part_alone_does_not_decide_the_study_match() {
        // BodyPartExamined is a series attribute: a rule that names the body
        // part still matches at the study level by code/modality, and a study
        // that carries only the body part must not be selected by it here.
        let mut rule = rule_with_all_signals();
        rule.r#match.study_description_contains = Vec::new();
        rule.r#match.code_meaning_contains = Vec::new();
        rule.r#match.tags = Vec::new();
        rule.r#match.body_part_contains = vec!["*chest*".into()];
        let attrs = StudyAttrs {
            modalities: vec!["DX".into()],
            body_part: "CHEST".into(),
            ..StudyAttrs::default()
        };
        // The match now rests on the catch-all (no study-level key is named),
        // not on the body part; the body-part constraint is the series filter's.
        assert!(matches_rule(&rule, &attrs));
        let other = StudyAttrs {
            modalities: vec!["DX".into()],
            body_part: "ABDOMEN".into(),
            ..StudyAttrs::default()
        };
        assert!(matches_rule(&rule, &other));
    }

    #[test]
    fn a_study_that_satisfies_no_signal_does_not_match() {
        let rule = rule_with_all_signals();
        let attrs = StudyAttrs {
            modalities: vec!["DX".into()],
            study_description: "ABDOMEN AP".into(),
            body_part: "ABDOMEN".into(),
            code_meanings: vec!["Abdomen".into()],
            tags: vec![("00211109".into(), "ABDOMEN".into())],
            code: None,
        };
        assert!(!matches_rule(&rule, &attrs));
    }

    #[test]
    fn modality_overrides_every_signal() {
        let rule = rule_with_all_signals();
        // Every optional signal matches and the rule still refuses: the
        // modality is the one condition that is not an alternative.
        let attrs = StudyAttrs {
            modalities: vec!["CT".into()],
            study_description: "CHEST PA".into(),
            body_part: "CHEST".into(),
            code_meanings: vec!["Thorax".into()],
            tags: vec![("00211109".into(), "CHEST".into())],
            code: None,
        };
        assert!(!matches_rule(&rule, &attrs));
    }

    #[test]
    fn a_rule_naming_only_a_modality_needs_nothing_else() {
        let rule = rule_with_modality_only();
        // A single-protocol site leaves every optional key out, and a
        // study with nothing else filled in is the one it wants.
        assert!(matches_rule(
            &rule,
            &StudyAttrs {
                modalities: vec!["CR".into()],
                ..StudyAttrs::default()
            }
        ));
        // An optional key the rule does not name cannot rescue a study
        // whose modality is wrong.
        assert!(!matches_rule(
            &rule,
            &StudyAttrs {
                modalities: vec!["MR".into()],
                study_description: "CHEST".into(),
                ..StudyAttrs::default()
            }
        ));
    }

    #[test]
    fn a_modality_matches_any_of_the_study_modalities() {
        // A study carries a SET of modalities (0008,0061). A rule naming
        // one of them applies, whichever position it holds in the study's
        // list.
        let rule = rule_with_modality_only();
        for study in [
            vec!["DX".to_string()],
            vec!["CT".to_string(), "DX".to_string()],
            vec!["DX".to_string(), "CR".to_string(), "MR".to_string()],
        ] {
            assert!(
                matches_rule(
                    &rule,
                    &StudyAttrs {
                        modalities: study.clone(),
                        ..StudyAttrs::default()
                    }
                ),
                "a rule naming DX must match a study declaring {study:?}"
            );
        }
        // A study whose declared set shares nothing with the rule does not
        // match, however many entries it has.
        assert!(!matches_rule(
            &rule,
            &StudyAttrs {
                modalities: vec!["CT".into(), "MR".into()],
                ..StudyAttrs::default()
            }
        ));
    }

    // -- The workitem code is the first key ------------------------------

    /// A rule constrained by CODE alone: no modality, no optional key.
    fn rule_with_code_only(code: &[&str]) -> Rule {
        Rule {
            r#match: Match {
                modality: Vec::new(),
                code: code.iter().map(|c| (*c).into()).collect(),
                study_description_contains: Vec::new(),
                code_meaning_contains: Vec::new(),
                body_part_contains: Vec::new(),
                tags: Vec::new(),
            },
            model: "m".into(),
            result: RuleResult {
                kind: ResultKind::Pr,
                color: Some("#fff".into()),
                profile_version: 1,
                series_description: None,
                label: LabelProfile::default(),
                sr: None,
                measurements: None,
            },
            index: 0,
        }
    }

    #[test]
    fn a_code_constrained_rule_matches_the_workitem_code_alone() {
        let rule = rule_with_code_only(&["CXR-AI"]);
        // No study attributes at all: the code came from the workitem, so the
        // study is neither needed nor consulted.
        assert!(matches_rule(
            &rule,
            &StudyAttrs {
                code: Some("CXR-AI".into()),
                ..StudyAttrs::default()
            }
        ));
        // The comparison is case-insensitive, because a code is a machine key.
        assert!(matches_rule(
            &rule,
            &StudyAttrs {
                code: Some("cxr-ai".into()),
                ..StudyAttrs::default()
            }
        ));
        // Any one of several listed codes is enough.
        let multi = rule_with_code_only(&["CXR-AI", "GENERIC-AI"]);
        assert!(matches_rule(
            &multi,
            &StudyAttrs {
                code: Some("GENERIC-AI".into()),
                ..StudyAttrs::default()
            }
        ));
    }

    #[test]
    fn a_code_constrained_rule_refuses_other_codes() {
        let rule = rule_with_code_only(&["CXR-AI"]);
        // A different code.
        assert!(!matches_rule(
            &rule,
            &StudyAttrs {
                code: Some("CT-organ-seg".into()),
                ..StudyAttrs::default()
            }
        ));
        // A code that merely STARTS like the wanted one: the match is exact.
        assert!(!matches_rule(
            &rule,
            &StudyAttrs {
                code: Some("CXR-AI-2".into()),
                ..StudyAttrs::default()
            }
        ));
        // No code on the workitem at all.
        assert!(!matches_rule(&rule, &StudyAttrs::default()));
    }

    /// The backwards-compatibility guarantee, stated as a test: a rule that
    /// names no code behaves exactly as it did before the key existed.
    #[test]
    fn an_empty_code_list_constrains_nothing() {
        let rule = rule_with_modality_only();
        assert!(rule.r#match.code.is_empty());
        // A workitem with a code, a foreign code, or no code at all: the rule
        // does not care, because it names none.
        for code in [Some("ANY".to_string()), None] {
            assert!(
                matches_rule(
                    &rule,
                    &StudyAttrs {
                        modalities: vec!["DX".into()],
                        code,
                        ..StudyAttrs::default()
                    }
                ),
                "a rule with no code key ignores the workitem's code"
            );
        }
    }

    /// The code and the modality together narrow the match: both must hold.
    #[test]
    fn code_and_modality_both_have_to_hold() {
        let mut rule = rule_with_code_only(&["CXR-AI"]);
        rule.r#match.modality = vec!["DX".into()];

        // Both right.
        assert!(matches_rule(
            &rule,
            &StudyAttrs {
                modalities: vec!["DX".into()],
                code: Some("CXR-AI".into()),
                ..StudyAttrs::default()
            }
        ));
        // Right code, wrong modality.
        assert!(!matches_rule(
            &rule,
            &StudyAttrs {
                modalities: vec!["CT".into()],
                code: Some("CXR-AI".into()),
                ..StudyAttrs::default()
            }
        ));
        // Right modality, wrong code.
        assert!(!matches_rule(
            &rule,
            &StudyAttrs {
                modalities: vec!["DX".into()],
                code: Some("GENERIC-AI".into()),
                ..StudyAttrs::default()
            }
        ));
    }

    /// The catch-all: a rule that constrains nothing matches every workitem,
    /// coded or not. This is the documented fallback shape.
    #[test]
    fn a_rule_constraining_nothing_matches_every_workitem() {
        let rule = rule_with_code_only(&[]);
        assert!(matches_rule(&rule, &StudyAttrs::default()));
        assert!(matches_rule(
            &rule,
            &StudyAttrs {
                code: Some("ANY".into()),
                ..StudyAttrs::default()
            }
        ));
    }

    #[test]
    fn a_study_declaring_no_modality_matches_no_modality_constrained_rule() {
        // An unknown modality is not a match: a rule that names modalities
        // cannot be satisfied by a study that declares none.
        let rule = rule_with_modality_only();
        assert!(!matches_rule(
            &rule,
            &StudyAttrs {
                modalities: Vec::new(),
                study_description: "CHEST PA".into(),
                ..StudyAttrs::default()
            }
        ));
        // And a rule that names no modality is unaffected by the absence.
        let unconstrained = Rule {
            r#match: Match {
                modality: Vec::new(),
                code: Vec::new(),
                study_description_contains: vec!["*chest*".into()],
                code_meaning_contains: Vec::new(),
                body_part_contains: Vec::new(),
                tags: Vec::new(),
            },
            ..rule_with_modality_only()
        };
        assert!(matches_rule(
            &unconstrained,
            &StudyAttrs {
                modalities: Vec::new(),
                study_description: "CHEST PA".into(),
                ..StudyAttrs::default()
            }
        ));
    }

    #[test]
    fn modality_comparison_ignores_case_on_both_sides() {
        // The origin may spell a modality in either case; the rule file may
        // too. Both are normalised before comparison.
        let rule = rule_with_modality_only();
        assert!(matches_rule(
            &rule,
            &StudyAttrs {
                modalities: vec!["dx".into()],
                ..StudyAttrs::default()
            }
        ));
    }
}
