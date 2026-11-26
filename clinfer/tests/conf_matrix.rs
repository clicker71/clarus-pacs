// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The configuration is two layers, and both are read by these tests.
//!
//! `clinfer.conf` carries the server, the engine, the models directory and
//! the fallback; a model is one YAML file under that directory. The tests
//! therefore write a DIRECTORY - a config plus `models/*.yaml` - because a
//! config on its own no longer describes what the sidecar runs.
//!
//! The base-file refusals are pinned here (an unknown key, a bad listen
//! address), and the model-file refusals live in `models_conf.rs`: each layer
//! reports its own problems, so a test that mixed them would not say which
//! layer refused.

use clinfer::conf::{self, DefaultRule, ResultKind};
use clinfer::matcher::{self, Decision, StudyAttrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

// -- Scratch directories ------------------------------------------------

/// A fresh, unique directory for one test.
///
/// The uniqueness comes from a process-wide counter, never from a clock: the
/// tests in this binary run on parallel threads that share one pid, and two
/// clock readings inside the same tick would produce one directory for two
/// tests.
fn scratch_dir() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "clinfer-conf-matrix-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// The base configuration, with `extra` appended to the top level.
fn base_source(extra: &str) -> String {
    format!(
        "\
server:
  base_url: \"http://127.0.0.1:8024/dicomweb\"
  identity_user: \"clinfer\"
engine:
  cache_dir: \"./cache\"
{extra}"
    )
}

/// A model file that satisfies every tier: one `pr` output and a runtime
/// naming this build's own shell `true`.
///
/// The script is a FILE the test writes beside the model, because an enabled
/// model whose script does not exist is refused by the tier that checks it -
/// which is a test of that tier, not of the loader under test here.
const A_MODEL: &str = "\
enabled: true
output_capabilities:
  sop_classes:
    - 1.2.840.10008.5.1.4.1.1.11.1
runtime:
  script: ./script.py
outputs:
  - kind: pr
    profile_version: 1
";

/// Write a config and its models directory; `models` is `(file stem, body)`.
fn write_tree(source: &str, models: &[(&str, &str)]) -> PathBuf {
    let dir = scratch_dir();
    if !models.is_empty() {
        let models_dir = dir.join("models");
        std::fs::create_dir_all(&models_dir).expect("models directory");
        // The script every fixture names must EXIST: an enabled model with a
        // missing script is refused, and that refusal is pinned by its own
        // test rather than by every test in this file.
        std::fs::write(models_dir.join("script.py"), b"#!/usr/bin/env python3\n")
            .expect("write script");
        for (name, body) in models {
            std::fs::write(models_dir.join(format!("{name}.yaml")), body).expect("write model");
        }
    }
    let path = dir.join("clinfer.conf");
    std::fs::write(&path, source).expect("write config");
    path
}

/// Load a config with exactly one valid model.
fn load_one(source: &str) -> Result<conf::Conf, conf::ConfError> {
    conf::load(&write_tree(source, &[("m", A_MODEL)]))
}

// -- The base layer -----------------------------------------------------

#[test]
fn an_absent_models_dir_is_the_models_directory_beside_the_config() {
    // A deployment is a directory holding `clinfer.conf` and `models/`, so a
    // config that names no directory gets the one it lives next to. The
    // anchor is the CONFIG FILE, which is what makes the directory movable.
    let path = write_tree(&base_source(""), &[("m", A_MODEL)]);
    let conf = conf::load(&path).expect("parses");
    assert_eq!(
        conf.models_dir,
        path.parent().expect("parent").join("models")
    );
    assert_eq!(conf.models.len(), 1, "the model beside it is read");
}

#[test]
fn a_relative_models_dir_is_anchored_at_the_configuration_file() {
    let path = write_tree(&base_source("models_dir: ./site-models\n"), &[]);
    let dir = path.parent().expect("parent");
    std::fs::create_dir_all(dir.join("site-models")).expect("dir");
    std::fs::write(dir.join("site-models/m.yaml"), A_MODEL).expect("write model");
    std::fs::write(
        dir.join("site-models/script.py"),
        b"#!/usr/bin/env python3\n",
    )
    .expect("write script");
    let conf = conf::load(&path).expect("parses");
    assert_eq!(conf.models_dir, dir.join("site-models"));
    assert_eq!(conf.models.len(), 1);
}

#[test]
fn an_empty_models_directory_is_a_valid_deployment() {
    // A sidecar that serves nothing is a deployment with nothing to do, not
    // a broken one: refusing to start would make the first install fail.
    let conf = conf::load(&write_tree(&base_source(""), &[])).expect("parses");
    assert!(conf.models.is_empty());
    assert!(conf.rules.is_empty());
    assert_eq!(conf.workers(), 0);
}

#[test]
fn the_fallback_defaults_to_cancel_and_may_be_switched_off() {
    let default = load_one(&base_source("")).expect("parses");
    assert_eq!(default.default_rule, DefaultRule::Cancel);

    let off = load_one(&base_source("default_rule: off\n")).expect("parses");
    assert_eq!(off.default_rule, DefaultRule::Off);

    let err = load_one(&base_source("default_rule: maybe\n")).expect_err("unknown value");
    assert!(err.to_string().contains("cancel or off"), "{err}");
}

#[test]
fn the_retired_models_and_rules_blocks_are_refused_by_name() {
    // Accepting them would leave an administrator believing a model is
    // declared that nothing reads: the model layer is a directory now.
    for (key, body) in [
        ("models", "models:\n  m:\n    script: /bin/true\n"),
        (
            "rules",
            "rules:\n  - match:\n      modality: [DX]\n    model: m\n",
        ),
    ] {
        let err = load_one(&base_source(body)).expect_err(body);
        let text = err.to_string();
        assert!(text.contains(key), "the message names {key}: {text}");
        assert!(text.contains("unknown key"), "{text}");
    }
}

#[test]
fn an_unknown_base_key_is_refused_with_its_name() {
    let err = load_one(&base_source("  base_uri: \"http://x\"\n")).expect_err("unknown key");
    assert!(err.to_string().contains("base_uri"), "{err}");
}

#[test]
fn the_listen_address_must_be_host_and_port() {
    let source = base_source("").replace(
        "  identity_user: \"clinfer\"",
        "  identity_user: \"clinfer\"\n  listen: \"127.0.0.1\"",
    );
    let err = load_one(&source).expect_err("no port");
    assert!(err.to_string().contains("server.listen"), "{err}");
}

#[test]
fn the_origin_url_must_be_http() {
    let source = base_source("").replace("http://127.0.0.1:8024", "ftp://127.0.0.1:8024");
    let err = load_one(&source).expect_err("not http");
    assert!(err.to_string().contains("http://"), "{err}");
}

#[test]
fn the_identity_user_must_not_be_empty() {
    let source = base_source("").replace("identity_user: \"clinfer\"", "identity_user: \"\"");
    let err = load_one(&source).expect_err("empty identity");
    assert!(err.to_string().contains("identity_user"), "{err}");
}

#[test]
fn the_strict_identity_mode_needs_a_trusted_peer() {
    let source = base_source("").replace(
        "  identity_user: \"clinfer\"",
        "  identity_user: \"clinfer\"\n  require_proxy_identity: true",
    );
    let err = load_one(&source).expect_err("no trusted peer");
    assert!(err.to_string().contains("trusted_proxies"), "{err}");
}

#[test]
fn the_station_aet_must_not_be_empty() {
    // The title is the (0040,0001) query key and the (0040,4025) comparison
    // operand; an empty one would make every "is this mine?" compare two
    // absences, which reads as "mine".
    let source = base_source("").replace(
        "  identity_user: \"clinfer\"",
        "  identity_user: \"clinfer\"\n  station_aet: \"\"",
    );
    let err = load_one(&source).expect_err("empty station");
    assert!(err.to_string().contains("station_aet"), "{err}");
}

// -- engine.workers -----------------------------------------------------

/// The base with `workers` set and `count` distinct models.
fn with_workers(workers: Option<&str>, count: usize) -> Result<conf::Conf, conf::ConfError> {
    let key = workers.map_or(String::new(), |value| format!("  workers: {value}\n"));
    let source = format!(
        "\
server:
  base_url: \"http://127.0.0.1:8024/dicomweb\"
  identity_user: \"clinfer\"
engine:
  cache_dir: \"./cache\"
{key}"
    );
    let names: Vec<String> = (0..count).map(|index| format!("m{index}")).collect();
    let models: Vec<(&str, &str)> = names.iter().map(|name| (name.as_str(), A_MODEL)).collect();
    conf::load(&write_tree(&source, &models))
}

#[test]
fn the_workers_ceiling_defaults_to_the_models() {
    // Absent means "one per model", which is the only value that cannot be
    // wrong for a site that never thought about the question.
    let single = with_workers(None, 1).expect("parses");
    assert_eq!(single.engine.workers, None, "the key stays absent");
    assert_eq!(single.workers(), 1);

    let two = with_workers(None, 2).expect("parses");
    assert_eq!(two.workers(), 2);
}

#[test]
fn a_workers_value_above_the_model_count_is_capped_not_refused() {
    let conf = with_workers(Some("8"), 2).expect("8 is accepted");
    assert_eq!(conf.engine.workers, Some(8), "the configured value is kept");
    assert_eq!(conf.workers(), 2, "and the effective ceiling is the cap");
}

#[test]
fn a_workers_value_of_zero_is_refused_by_name() {
    let err = with_workers(Some("0"), 1).expect_err("0 is refused");
    let text = err.to_string();
    assert!(text.contains("engine.workers"), "the key is named: {text}");
    assert!(
        text.contains("at least 1") || text.contains("start at 1"),
        "and the bound is stated: {text}"
    );
}

#[test]
fn a_negative_workers_value_is_refused() {
    let err = with_workers(Some("-1"), 1).expect_err("negative is refused");
    assert!(err.to_string().contains("engine.workers"), "{err}");
}

// -- Model -> rule derivation -------------------------------------------

#[test]
fn every_output_of_a_model_becomes_its_own_rule() {
    // A detector that produces a presentation state AND a report is one
    // model and two rules: the chain delivers every matched object, which is
    // what the multi-output YAML entry exists for.
    let model = "\
enabled: true
output_capabilities:
  sop_classes:
    - 1.2.840.10008.5.1.4.1.1.11.1
    - 1.2.840.10008.5.1.4.1.1.88.11
runtime:
  script: ./script.py
outputs:
  - kind: pr
    series_description: Boxes
    profile_version: 2
  - kind: sr
    series_description: Report
";
    let conf = conf::load(&write_tree(&base_source(""), &[("det", model)])).expect("parses");
    assert_eq!(conf.rules.len(), 2, "one rule per output");
    assert_eq!(conf.rules[0].result.kind, ResultKind::Pr);
    assert_eq!(conf.rules[0].result.profile_version, 2);
    assert_eq!(
        conf.rules[0].result.series_description.as_deref(),
        Some("Boxes")
    );
    assert_eq!(conf.rules[1].result.kind, ResultKind::Sr);
    assert_eq!(conf.rules[1].result.profile_version, 1, "the default");
    assert!(conf.rules.iter().all(|rule| rule.model == "det"));
    assert_eq!(conf.rules[0].index, 0);
    assert_eq!(conf.rules[1].index, 1);
}

#[test]
fn a_disabled_model_is_not_registered_at_all() {
    // An example in the directory must not reach the worker: it may name a
    // script the site has not installed, and a model that dispatches to a
    // missing program is worse than one that is simply absent.
    let disabled = A_MODEL.replace("enabled: true", "enabled: false");
    let conf = conf::load(&write_tree(&base_source(""), &[("off", &disabled)])).expect("parses");
    assert!(conf.models.is_empty());
    assert!(conf.rules.is_empty());
    assert!(conf.model("off").is_none());
}

#[test]
fn a_model_id_is_its_file_stem() {
    let conf = conf::load(&write_tree(&base_source(""), &[("cxr10", A_MODEL)])).expect("parses");
    assert!(conf.model("cxr10").is_some());
    assert_eq!(conf.rules[0].model, "cxr10");
}

#[test]
fn files_are_read_in_sorted_order_not_directory_order() {
    // Directory enumeration order is not promised on any platform, and a
    // report whose lines move between runs cannot be diffed.
    let conf = conf::load(&write_tree(
        &base_source(""),
        &[("zzz", A_MODEL), ("aaa", A_MODEL), ("mmm", A_MODEL)],
    ))
    .expect("parses");
    let ids: Vec<&str> = conf.models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, ["aaa", "mmm", "zzz"]);
}

// -- Matching ------------------------------------------------------------

#[test]
fn a_model_matches_the_study_it_declares() {
    let model = "\
enabled: true
output_capabilities:
  sop_classes:
    - 1.2.840.10008.5.1.4.1.1.11.1
match:
  modality:
    - DX
    - CR
  study_description_contains:
    - \"*chest*\"
    - \"*thorax*\"
runtime:
  script: ./script.py
outputs:
  - kind: pr
";
    let conf = conf::load(&write_tree(&base_source(""), &[("det", model)])).expect("parses");
    let chest = StudyAttrs {
        modalities: vec!["DX".into()],
        study_description: "CHEST PA".into(),
        ..StudyAttrs::default()
    };
    match matcher::decide(&conf, &chest) {
        Decision::Run(rule) => assert_eq!(rule.model, "det"),
        Decision::Fallback => panic!("the model should match a chest DX"),
    }

    // Another modality falls through even though the description matches.
    let ct = StudyAttrs {
        modalities: vec!["CT".into()],
        study_description: "CHEST PA".into(),
        ..StudyAttrs::default()
    };
    assert_eq!(matcher::decide(&conf, &ct), Decision::Fallback);
}

#[test]
fn a_model_that_declares_no_filter_accepts_every_study() {
    // An empty filter constrains nothing, which is what a model about its
    // OUTPUT rather than its INPUT means.
    let conf = conf::load(&write_tree(&base_source(""), &[("any", A_MODEL)])).expect("parses");
    let attrs = StudyAttrs {
        modalities: vec!["MR".into()],
        ..StudyAttrs::default()
    };
    assert!(matches!(matcher::decide(&conf, &attrs), Decision::Run(_)));
}

#[test]
fn a_malformed_model_file_reports_its_own_path() {
    // The refusal names the model FILE, not the base config: an operator
    // editing a model must not be sent to the file they did not touch.
    let err = conf::load(&write_tree(&base_source(""), &[("broken", "outputs: [\n")]))
        .expect_err("a malformed file is refused");
    assert!(err.to_string().contains("broken.yaml"), "{err}");
}

#[test]
fn an_unlistable_models_path_is_an_error_not_an_empty_list() {
    // `NotFound` is a deployment with no models; a path that exists but is
    // not a directory is a filesystem fault, and reading it as "no models"
    // would silently stop serving models the site installed.
    let path = write_tree(&base_source("models_dir: ./models-file\n"), &[]);
    let dir = path.parent().expect("parent");
    std::fs::write(dir.join("models-file"), b"not a directory").expect("write file");
    let err = conf::load(&path).expect_err("a file where a directory belongs");
    assert!(err.to_string().contains("models-file"), "{err}");
}

#[test]
fn the_models_dir_resolves_to_an_absolute_path() {
    // A relative default is anchored at the config file, so a packaged
    // deployment works from any working directory.
    let conf = load_one(&base_source("")).expect("parses");
    assert!(
        conf.models_dir.is_absolute(),
        "{:?} is not absolute",
        conf.models_dir
    );
}
