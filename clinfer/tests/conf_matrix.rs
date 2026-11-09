// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The reference configuration must parse, and the refusals must refuse.
//!
//! The file these tests read is the one shipped to administrators
//! (`clinfer.conf.example.en`), not a copy kept in step by hand: a change to
//! the parser that the reference file no longer satisfies fails here
//! rather than in the field.

use clinfer::conf::{self, DefaultRule, ResultKind, SrTemplate};
use clinfer::matcher::{self, Decision, StudyAttrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

fn example_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("clinfer.conf.example.en")
}

fn example_source() -> String {
    std::fs::read_to_string(example_path()).expect("the reference configuration is present")
}

fn example_ru_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("clinfer.conf.example.ru")
}

/// The Russian reference file is the SAME configuration with translated
/// comments, so it must parse to the SAME values.
///
/// It is shipped to administrators but was never read by a test, which is how
/// a key could be documented in one file and mistyped in the other without
/// anything noticing. Comparing the PARSED configuration rather than the bytes
/// is the right comparison: the comments are supposed to differ (that is the
/// whole point of two files) and the settings are not.
#[test]
fn both_reference_files_parse_to_the_same_configuration() {
    let en = conf::load(&example_path()).expect("the English reference file parses");
    let ru = conf::load(&example_ru_path()).expect("the Russian reference file parses");

    assert_eq!(
        en.server.poll_interval_secs, ru.server.poll_interval_secs,
        "the poll interval must be the same in both reference files"
    );
    assert_eq!(
        en.server.station_aet, ru.server.station_aet,
        "the station AE Title is an identity, not a translation"
    );
    assert_eq!(
        en.server.base_url, ru.server.base_url,
        "the two files must point at the same kind of origin"
    );
    assert_eq!(
        en.models.len(),
        ru.models.len(),
        "the translated file must declare the same models"
    );
    // `engine.workers` is the one engine knob that changes concurrency rather
    // than a path, so a translated file that omits it runs a different sidecar
    // from the one the English file documents.
    assert_eq!(
        en.engine.workers, ru.engine.workers,
        "the engine worker ceiling must be the same in both reference files"
    );
    assert!(
        en.engine.workers.is_some_and(|workers| workers > 0),
        "the reference files document a real ceiling: {:?}",
        en.engine.workers
    );
}

/// The knob the fallback poll needs is documented, and its documented value is
/// the default.
///
/// An example file that omits the key teaches an operator nothing about it; one
/// that names a different value than the parser defaults to would make "copy
/// the example" silently change behaviour.
#[test]
fn the_reference_configurations_document_the_poll_interval() {
    for (path, label) in [
        (example_path(), "clinfer.conf.example.en"),
        (example_ru_path(), "clinfer.conf.example.ru"),
    ] {
        let source = std::fs::read_to_string(&path).expect("reference file present");
        let documented = source
            .lines()
            .find(|line| line.trim_start().starts_with("poll_interval_secs:"))
            .unwrap_or_else(|| panic!("{label} does not document poll_interval_secs"));
        assert!(
            documented.contains("poll_interval_secs: 2"),
            "{label} must show the default: {documented}"
        );
    }
}

/// A unique temp-file name for one `load_source` call.
///
/// The uniqueness comes from a process-wide counter, not from a clock reading.
/// A timestamp is not a uniqueness primitive: the tests in this binary run on
/// parallel threads that share one pid, two of them can read the clock inside
/// the same tick, and the resulting name collision makes one test parse (and
/// delete) another test's file - which surfaced as a test reporting a
/// DIFFERENT config's refusal text. `abi_seam.rs` already names its scratch
/// directories this way; this helper follows it.
fn temp_name() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!(
        "clinfer-conf-{}-{}.conf",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Write `source` to a temporary file and load it.
fn load_source(source: &str) -> Result<conf::Conf, conf::ConfError> {
    let path = std::env::temp_dir().join(temp_name());
    std::fs::write(&path, source).expect("write");
    let result = conf::load(&path);
    let _ = std::fs::remove_file(&path);
    result
}

// -- The reference file -------------------------------------------------

#[test]
fn the_reference_configuration_parses_whole() {
    let conf = conf::load(&example_path()).expect("the shipped example parses");

    assert_eq!(conf.server.base_url, "http://127.0.0.1:8024/dicomweb");
    assert_eq!(conf.server.identity_user, "clinfer");
    assert_eq!(conf.engine.cache_dir, PathBuf::from("./cache"));

    let model = conf.model("detect-pr").expect("the declared model");
    // Relative paths anchor at the directory of the configuration file
    // (the shipped-demo rule), so the expectation is built the same way.
    let expected_script = example_path()
        .parent()
        .expect("parent")
        .join("scripts/PR-openvino-detect.py");
    assert_eq!(model.script.as_deref(), Some(expected_script.as_path()));
    let expected_interpreter = example_path()
        .parent()
        .expect("parent")
        .join("venv/bin/python");
    assert_eq!(
        model.interpreter.as_deref(),
        Some(expected_interpreter.as_path())
    );
    assert_eq!(model.timeout_secs, 300);
    // The reference file leaves the result budget at the default, which is
    // what an existing deployment gets when it does not mention the key.
    assert_eq!(model.max_result_bytes, conf::DEFAULT_MAX_RESULT_BYTES);

    assert_eq!(conf.rules.len(), 1);
    let rule = &conf.rules[0];
    assert_eq!(rule.model, "detect-pr");
    assert_eq!(rule.result.kind, ResultKind::Pr);
    assert_eq!(rule.result.color.as_deref(), Some("#FF3B30"));
    assert_eq!(rule.result.profile_version, 1);
    assert_eq!(conf.default_rule, DefaultRule::Cancel);
}

#[test]
fn the_reference_file_pins_the_operational_defaults() {
    let conf = conf::load(&example_path()).expect("parses");
    // The reference file names the operational defaults explicitly, so a
    // reader sees the shipped values. They must not drift towards the
    // strict ones: wildcard listen and closed proxy list are refusals to
    // run; the loopback peer is listed because that is where the origin
    // pushes from.
    assert_eq!(conf.server.listen, "127.0.0.1:8124");
    assert_eq!(conf.server.trusted_proxies, vec!["127.0.0.1".to_string()]);
    assert!(!conf.server.require_proxy_identity);
}

#[test]
fn the_reference_rule_matches_a_chest_xray() {
    let conf = conf::load(&example_path()).expect("parses");
    // The reference rule constrains the description AND the code
    // meanings, so a study has to satisfy both to match it.
    let attrs = StudyAttrs {
        modalities: vec!["DX".into()],
        study_description: "CHEST PA".into(),
        code_meanings: vec!["Thorax".into()],
        ..StudyAttrs::default()
    };
    match matcher::decide(&conf, &attrs) {
        Decision::Run(rule) => assert_eq!(rule.model, "detect-pr"),
        Decision::Fallback => panic!("the reference rule should match a chest DX"),
    }

    // The same study under a modality the rule does not list falls
    // through even though every other condition holds.
    let attrs = StudyAttrs {
        modalities: vec!["CT".into()],
        study_description: "CHEST PA".into(),
        code_meanings: vec!["Thorax".into()],
        ..StudyAttrs::default()
    };
    assert_eq!(matcher::decide(&conf, &attrs), Decision::Fallback);
}

#[test]
fn the_reference_rule_is_not_triggered_by_a_different_body_part() {
    let conf = conf::load(&example_path()).expect("parses");
    // An abdominal study: neither suggested signal matches, so the rule
    // stays away even though the modality is one it names.
    let attrs = StudyAttrs {
        modalities: vec!["DX".into()],
        study_description: "ABDOMEN AP".into(),
        code_meanings: vec!["Abdomen".into()],
        ..StudyAttrs::default()
    };
    assert_eq!(matcher::decide(&conf, &attrs), Decision::Fallback);
}

#[test]
fn the_reference_rule_needs_any_one_of_its_signals() {
    let conf = conf::load(&example_path()).expect("parses");
    let base = StudyAttrs {
        modalities: vec!["DX".into()],
        ..StudyAttrs::default()
    };

    // The suggested body-part keys are alternatives, so each one alone is
    // enough: a device that fills in a description and leaves the coded
    // sequences empty is recognised, and so is one that does the reverse.
    let description_only = StudyAttrs {
        study_description: "CHEST PA".into(),
        ..base.clone()
    };
    match matcher::decide(&conf, &description_only) {
        Decision::Run(rule) => assert_eq!(rule.model, "detect-pr"),
        Decision::Fallback => panic!("the description alone should match"),
    }

    let code_only = StudyAttrs {
        code_meanings: vec!["Thorax".into()],
        ..base.clone()
    };
    match matcher::decide(&conf, &code_only) {
        Decision::Run(rule) => assert_eq!(rule.model, "detect-pr"),
        Decision::Fallback => panic!("the coded meaning alone should match"),
    }

    // Neither signal matches: no rule applies.
    let neither = StudyAttrs {
        study_description: "ABDOMEN AP".into(),
        code_meanings: vec!["Abdomen".into()],
        ..base.clone()
    };
    assert_eq!(matcher::decide(&conf, &neither), Decision::Fallback);

    // Both signals match and the modality does not: the modality is the
    // one condition that is not an alternative.
    let wrong_modality = StudyAttrs {
        modalities: vec!["CT".into()],
        study_description: "CHEST PA".into(),
        code_meanings: vec!["Thorax".into()],
        ..StudyAttrs::default()
    };
    assert_eq!(matcher::decide(&conf, &wrong_modality), Decision::Fallback);
}

// -- The refusals -------------------------------------------------------

/// A minimal configuration whose one model is an external processor,
/// with the result block supplied by the caller.
fn with_rule(result_body: &str) -> String {
    format!(
        "\
server:
  base_url: \"http://127.0.0.1:8024/dicomweb\"
  identity_user: \"clinfer\"
engine:
  cache_dir: \"./cache\"
models:
  m:
    script: \"/opt/clinfer/scripts/proc.py\"
rules:
  - match:
      modality: [DX]
    model: \"m\"
{result_body}
default_rule: cancel
"
    )
}

/// The fallback poll interval: absent -> 2, `0` -> disabled, negative -> a
/// refusal that NAMES the key.
///
/// `0` is the interesting case. It is a legal value here, unlike
/// `engine.workers`, because "do not poll" is a real deployment decision for an
/// origin that DOES deliver Event Reports; a knob whose off switch is missing
/// is a knob operators route around.
#[test]
fn the_poll_interval_defaults_to_two_and_accepts_zero() {
    let base = with_rule("    result:\n      kind: pr\n      color: \"#fff\"\n");

    let absent = load_source(&base).expect("the default configuration parses");
    assert_eq!(
        absent.server.poll_interval_secs, 2,
        "an absent key means the documented default"
    );

    let zero = load_source(&base.replace(
        "  identity_user: \"clinfer\"",
        "  identity_user: \"clinfer\"\n  poll_interval_secs: 0",
    ))
    .expect("0 is legal: it disables polling");
    assert_eq!(zero.server.poll_interval_secs, 0, "0 means off, not absent");

    let explicit = load_source(&base.replace(
        "  identity_user: \"clinfer\"",
        "  identity_user: \"clinfer\"\n  poll_interval_secs: 30",
    ))
    .expect("a plain interval parses");
    assert_eq!(explicit.server.poll_interval_secs, 30);
}

#[test]
fn a_negative_poll_interval_is_refused_by_name() {
    let source = with_rule("    result:\n      kind: pr\n      color: \"#fff\"\n").replace(
        "  identity_user: \"clinfer\"",
        "  identity_user: \"clinfer\"\n  poll_interval_secs: -1",
    );
    let err = load_source(&source).expect_err("a negative interval has no meaning");
    let text = err.to_string();
    assert!(
        text.contains("poll_interval_secs"),
        "the message names the key: {text}"
    );
}

/// An interval past the maximum is refused, not silently clamped.
///
/// An operator who wrote 86400 believes they configured something; clamping to
/// 3600 would honour a number they did not choose.
#[test]
fn a_poll_interval_above_the_maximum_is_refused() {
    let source = with_rule("    result:\n      kind: pr\n      color: \"#fff\"\n").replace(
        "  identity_user: \"clinfer\"",
        "  identity_user: \"clinfer\"\n  poll_interval_secs: 3601",
    );
    let err = load_source(&source).expect_err("3601 is past the maximum");
    assert!(err.to_string().contains("poll_interval_secs"), "{err}");
}

#[test]
fn an_unknown_key_is_refused_with_its_name() {
    let source = with_rule("    result:\n      kind: pr\n      color: \"#fff\"\n").replace(
        "  identity_user: \"clinfer\"",
        "  identity_user: \"clinfer\"\n  base_uri: \"http://x\"",
    );
    let err = load_source(&source).expect_err("unknown key");
    assert!(err.to_string().contains("base_uri"), "{err}");
    assert!(err.to_string().contains("unknown key"), "{err}");
}

#[test]
fn an_unknown_key_deep_in_a_rule_names_its_path() {
    let source =
        with_rule("    result:\n      kind: pr\n      color: \"#fff\"\n      cached: true\n");
    let err = load_source(&source).expect_err("unknown key");
    assert!(err.to_string().contains("rules[0].result.cached"), "{err}");
}

#[test]
fn an_unknown_key_in_a_model_names_its_path() {
    // A key the removed native engine used to read is now simply unknown:
    // accepting it would leave an administrator believing a declaration
    // exists that nothing reads.
    let source = with_rule("    result:\n      kind: pr\n      color: \"#fff\"\n").replace(
        "    script: \"/opt/clinfer/scripts/proc.py\"",
        "    script: \"/opt/clinfer/scripts/proc.py\"\n    device: \"GPU.0\"",
    );
    let err = load_source(&source).expect_err("unknown key");
    assert!(err.to_string().contains("device"), "{err}");
}

#[test]
fn a_model_without_a_script_is_refused() {
    // `script` is the whole declaration of a model: clinfer has no
    // engine, so a model without one is a model that produces nothing.
    let source = with_rule("    result:\n      kind: pr\n      color: \"#fff\"\n")
        .replace("    script: \"/opt/clinfer/scripts/proc.py\"\n", "");
    let err = load_source(&source).expect_err("missing script");
    assert!(err.to_string().contains("script"), "{err}");
}

#[test]
fn the_processor_timeout_is_bounded_by_name() {
    for (value, want) in [(4, false), (300, true), (1800, true), (1801, false)] {
        let source = with_rule("    result:\n      kind: pr\n      color: \"#fff\"\n").replace(
            "    script: \"/opt/clinfer/scripts/proc.py\"",
            &format!("    script: \"/opt/clinfer/scripts/proc.py\"\n    timeout_secs: {value}"),
        );
        match load_source(&source) {
            Ok(_) => assert!(want, "timeout_secs={value} should be refused"),
            Err(err) => assert!(!want, "timeout_secs={value} gave: {err}"),
        }
    }
}

// -- engine.workers ------------------------------------------------------

/// The reference configuration with `engine.workers` set, and `models`
/// replaced by `models_body` (indented under `models:`).
fn with_workers(workers: Option<&str>, extra_model: bool) -> String {
    let models = if extra_model {
        "models:\n  m:\n    script: \"/opt/clinfer/scripts/proc.py\"\n  \
         n:\n    script: \"/opt/clinfer/scripts/other.py\"\n"
    } else {
        "models:\n  m:\n    script: \"/opt/clinfer/scripts/proc.py\"\n"
    };
    let key = workers.map_or(String::new(), |value| format!("  workers: {value}\n"));
    format!(
        "\
server:
  base_url: \"http://127.0.0.1:8024/dicomweb\"
  identity_user: \"clinfer\"
engine:
  cache_dir: \"./cache\"
{key}{models}rules:
  - match:
      modality: [DX]
    model: \"m\"
    result:
      kind: pr
      color: \"#fff\"
default_rule: cancel
"
    )
}

#[test]
fn the_workers_ceiling_defaults_to_the_models_with_scripts() {
    // Absent means "one per model that declares a script", which is the
    // shipped default and the only value that cannot be wrong for a site
    // that never thought about the question.
    let single = load_source(&with_workers(None, false)).expect("parses");
    assert_eq!(single.engine.workers, None, "the key stays absent");
    assert_eq!(single.workers(), 1, "and resolves to the model count");

    let two = load_source(&with_workers(None, true)).expect("parses");
    assert_eq!(two.workers(), 2);
}

#[test]
fn a_workers_value_larger_than_the_model_count_is_capped_not_refused() {
    // A ceiling above the number of threads that exist is not an error, it
    // is a redundant setting. Refusing it would make a host's configuration
    // depend on how many models happen to be declared beside it.
    let conf = load_source(&with_workers(Some("8"), true)).expect("8 is accepted");
    assert_eq!(conf.engine.workers, Some(8), "the configured value is kept");
    assert_eq!(conf.workers(), 2, "and the effective ceiling is the cap");
}

#[test]
fn a_workers_value_of_one_keeps_the_models_serial() {
    // The one setting that reproduces the sidecar's pre-sprint behaviour
    // exactly, which is what a pilot deployment wants.
    let conf = load_source(&with_workers(Some("1"), true)).expect("parses");
    assert_eq!(
        conf.workers(),
        1,
        "one child at a time, whatever is declared"
    );
}

#[test]
fn a_workers_value_of_zero_is_refused_by_name() {
    // 0 is not "no limit": it would be a sidecar that claims work and never
    // runs it, which is worse than a refused start and invisible in the log.
    let err = load_source(&with_workers(Some("0"), false)).expect_err("0 is refused");
    let text = err.to_string();
    assert!(text.contains("engine.workers"), "the key is named: {text}");
    assert!(
        text.contains("at least 1") || text.contains("start at 1"),
        "and the bound is stated: {text}"
    );
    assert!(
        text.contains("model processes"),
        "and the message says what the key limits: {text}"
    );
}

#[test]
fn a_negative_workers_value_is_refused() {
    let err = load_source(&with_workers(Some("-1"), false)).expect_err("negative is refused");
    assert!(err.to_string().contains("engine.workers"), "{err}");
}

#[test]
fn the_reference_file_declares_the_workers_key() {
    // The shipped example names the operational defaults explicitly, so a
    // reader sees the value rather than inferring it from the prose.
    let conf = conf::load(&example_path()).expect("parses");
    assert!(
        conf.engine.workers.is_some(),
        "the reference file states engine.workers"
    );
    assert_eq!(
        conf.workers(),
        conf.models
            .iter()
            .filter(|model| model.script.is_some())
            .count()
            .min(conf.engine.workers.expect("present")),
        "and the effective value is the documented cap rule"
    );
}

#[test]
fn every_result_kind_has_an_encoder_and_loads() {
    // Every kind is written by this crate, and a processor may declare any
    // of them: the object is decided by its result.json, the rule's kind
    // only names the encoder. A kind added to the enum without an encoder
    // would be refused here by name.
    for kind in ["pr", "sr", "seg", "derived", "stl"] {
        let body = format!("    result:\n      kind: {kind}\n      color: \"#fff\"\n");
        let source = with_rule(&body);
        load_source(&source).unwrap_or_else(|err| panic!("'{kind}' must load, got: {err}"));
    }
}

#[test]
fn an_sr_block_parses_with_its_defaults_and_bounds() {
    let source = with_rule(
        "    result:\n      kind: sr\n      series_description: \"LLM Findings\"\n      \
         profile_version: 1\n      sr:\n        title: \"LLM Findings\"\n        template_id: \
         TID2000\n",
    );
    let conf = load_source(&source).expect("the sr block parses");
    let result = &conf.rules[0].result;
    assert_eq!(result.kind, ResultKind::Sr);
    assert_eq!(result.series_description.as_deref(), Some("LLM Findings"));
    let sr = result.sr.as_ref().expect("the sr block");
    assert_eq!(sr.title.as_deref(), Some("LLM Findings"));
    assert_eq!(sr.template_id, SrTemplate::Tid2000);

    // An sr result without a block is legal: the template and the title
    // both have defaults.
    let source = with_rule("    result:\n      kind: sr\n      profile_version: 1\n");
    let conf = load_source(&source).expect("an sr result without a block parses");
    assert_eq!(conf.rules[0].result.sr, Some(conf::SrResult::default()));
}

#[test]
fn an_sr_block_on_a_non_sr_result_is_refused() {
    let source = with_rule(
        "    result:\n      kind: pr\n      color: \"#fff\"\n      sr:\n        title: \"x\"\n",
    );
    let err = load_source(&source).expect_err("sr block on a pr result");
    assert!(err.to_string().contains("sr"), "{err}");
}

#[test]
fn an_unknown_model_is_refused() {
    let source = with_rule("    result:\n      kind: pr\n      color: \"#fff\"\n")
        .replace("model: \"m\"", "model: \"nowhere\"");
    let err = load_source(&source).expect_err("unknown model");
    assert!(err.to_string().contains("unknown model"), "{err}");
}

#[test]
fn an_unknown_default_rule_is_refused() {
    let source = with_rule("    result:\n      kind: pr\n      color: \"#fff\"\n")
        .replace("default_rule: cancel", "default_rule: maybe");
    let err = load_source(&source).expect_err("unknown value");
    assert!(err.to_string().contains("cancel or off"), "{err}");
}

#[test]
fn a_rule_without_a_modality_is_refused() {
    let source = with_rule("    result:\n      kind: pr\n      color: \"#fff\"\n").replace(
        "      modality: [DX]\n",
        "      study_description_contains: [\"*chest*\"]\n",
    );
    let err = load_source(&source).expect_err("modality required");
    assert!(err.to_string().contains("modality"), "{err}");
}

#[test]
fn malformed_yaml_reports_the_line() {
    let source = format!("{}\n  base_url: \"x\n", example_source());
    let err = load_source(&source).expect_err("unterminated quote");
    assert!(err.to_string().contains("unterminated"), "{err}");
}

// -- The shipped documentation ------------------------------------------

/// The example prompt file named by the reference configuration must
/// exist, so a copied config does not fail start-up on a missing path,
/// and must carry the disclaimer the model is asked to repeat.
#[test]
fn the_shipped_prompt_names_the_disclaimer_and_the_frames() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("llm_prompt.example.txt");
    let text = std::fs::read_to_string(&path).expect("the example prompt ships");
    assert!(
        text.contains("not a diagnosis"),
        "the disclaimer the model repeats"
    );
    assert!(
        text.contains("EACH frame"),
        "the per-frame instruction the report shape depends on"
    );
}

// -- Matching table -----------------------------------------------------

/// A configuration with an ordered pair of processor rules, to prove
/// first-match.
const TWO_RULES: &str = "\
server:
  base_url: \"http://127.0.0.1:8024/dicomweb\"
  identity_user: \"clinfer\"
engine:
  cache_dir: \"./cache\"
models:
  narrow:
    script: \"/opt/clinfer/scripts/narrow.py\"
  broad:
    script: \"/opt/clinfer/scripts/broad.py\"
rules:
  - match:
      modality: [DX]
      study_description_contains: [\"*pneumo*\"]
    model: \"narrow\"
    result:
      kind: pr
      color: \"#FF0000\"
  - match:
      modality: [DX, CR]
    model: \"broad\"
    result:
      kind: pr
      color: \"#00FF00\"
default_rule: off
";

#[test]
fn the_first_matching_rule_wins() {
    let conf = load_source(TWO_RULES).expect("parses");
    let specific = StudyAttrs {
        modalities: vec!["DX".into()],
        study_description: "CHEST PNEUMONIA".into(),
        ..StudyAttrs::default()
    };
    match matcher::decide(&conf, &specific) {
        Decision::Run(rule) => {
            assert_eq!(rule.model, "narrow");
            assert_eq!(rule.index, 0);
        }
        Decision::Fallback => panic!("the specific rule should match"),
    }

    let general = StudyAttrs {
        modalities: vec!["CR".into()],
        study_description: "CHEST".into(),
        ..StudyAttrs::default()
    };
    match matcher::decide(&conf, &general) {
        Decision::Run(rule) => {
            assert_eq!(rule.model, "broad");
            assert_eq!(rule.index, 1);
        }
        Decision::Fallback => panic!("the broad rule should match"),
    }
}

#[test]
fn no_match_takes_the_configured_fallback() {
    let conf = load_source(TWO_RULES).expect("parses");
    assert_eq!(conf.default_rule, DefaultRule::Off);
    let attrs = StudyAttrs {
        modalities: vec!["MR".into()],
        ..StudyAttrs::default()
    };
    assert_eq!(matcher::decide(&conf, &attrs), Decision::Fallback);
}

#[test]
fn the_default_fallback_is_cancel() {
    let conf = load_source(&TWO_RULES.replace("default_rule: off", "")).expect("parses");
    assert_eq!(conf.default_rule, DefaultRule::Cancel);
}

#[test]
fn optional_filters_are_independent() {
    let conf = load_source(TWO_RULES).expect("parses");
    // The first rule names only the study description; a modality it does
    // not list still refuses, but a missing description does not.
    let attrs = StudyAttrs {
        modalities: vec!["DX".into()],
        study_description: String::new(),
        body_part: "CHEST".into(),
        ..StudyAttrs::default()
    };
    match matcher::decide(&conf, &attrs) {
        Decision::Run(rule) => assert_eq!(rule.model, "broad"),
        Decision::Fallback => panic!("the broad rule matches on modality alone"),
    }
}

#[test]
fn a_tag_binding_matches_a_private_tag() {
    let source = TWO_RULES.replace(
        "      study_description_contains: [\"*pneumo*\"]",
        "      tag: { 00211109: [\"*грудн*\", \"*chest*\"] }",
    );
    assert_ne!(source, TWO_RULES, "the tag binding was not substituted");
    let conf = load_source(&source).expect("parses");
    let attrs = StudyAttrs {
        modalities: vec!["DX".into()],
        tags: vec![("00211109".into(), "ГРУДНАЯ КЛЕТКА".into())],
        ..StudyAttrs::default()
    };
    match matcher::decide(&conf, &attrs) {
        Decision::Run(rule) => assert_eq!(rule.model, "narrow"),
        Decision::Fallback => panic!("the tag binding should match"),
    }

    // The same study without the tag falls through to the broad rule.
    let attrs = StudyAttrs {
        modalities: vec!["DX".into()],
        ..StudyAttrs::default()
    };
    match matcher::decide(&conf, &attrs) {
        Decision::Run(rule) => assert_eq!(rule.model, "broad"),
        Decision::Fallback => panic!("the broad rule should match"),
    }
}

#[test]
fn a_quoted_tag_key_binds_the_same_tag_as_a_bare_one() {
    // YAML 1.2 7.3.1: `"00211109"` and `00211109` are the same key. Before
    // the quotes were removed at the key layer, the quoted spelling reached
    // the tag reader as ten characters and was refused, so an operator who
    // quoted a numeric key - which is what a YAML linter suggests for an
    // all-digit key - got a configuration that would not load.
    let quoted = TWO_RULES.replace(
        "      study_description_contains: [\"*pneumo*\"]",
        "      tag: { \"00211109\": [\"*chest*\"] }",
    );
    assert_ne!(quoted, TWO_RULES, "the tag binding was not substituted");
    let conf = load_source(&quoted).expect("a quoted tag key parses");

    let attrs = StudyAttrs {
        modalities: vec!["DX".into()],
        tags: vec![("00211109".into(), "CHEST".into())],
        ..StudyAttrs::default()
    };
    match matcher::decide(&conf, &attrs) {
        Decision::Run(rule) => assert_eq!(rule.model, "narrow"),
        Decision::Fallback => panic!("the quoted tag binding should match"),
    }

    // And it is the SAME binding, not merely a working one: the bare
    // spelling must reach the identical rule.
    let bare = TWO_RULES.replace(
        "      study_description_contains: [\"*pneumo*\"]",
        "      tag: { 00211109: [\"*chest*\"] }",
    );
    let bare_conf = load_source(&bare).expect("the bare spelling parses");
    match (
        matcher::decide(&conf, &attrs),
        matcher::decide(&bare_conf, &attrs),
    ) {
        (Decision::Run(quoted_rule), Decision::Run(bare_rule)) => assert_eq!(
            quoted_rule.model, bare_rule.model,
            "both spellings select the same rule"
        ),
        other => panic!("both spellings should run a rule, got {other:?}"),
    }
}

#[test]
fn a_tag_binding_with_a_malformed_tag_is_refused() {
    let source = TWO_RULES.replace(
        "      study_description_contains: [\"*pneumo*\"]",
        "      tag: { 0021110: [\"*chest*\"] }",
    );
    assert_ne!(source, TWO_RULES, "the tag binding was not substituted");
    let err = load_source(&source).expect_err("malformed tag");
    assert!(err.to_string().contains("8 hex digits"), "{err}");
}
