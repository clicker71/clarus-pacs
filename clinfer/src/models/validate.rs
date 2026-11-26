// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! What a models directory must satisfy to be accepted.
//!
//! Two tiers, because the same directory serves two different purposes:
//!
//! - EVERY file is checked against the schema, including `enabled: false`
//!   examples. An example is documentation an operator copies, so a broken
//!   one teaches a broken model; it is read for exactly that reason, and a
//!   file that is not read is a file that rots.
//! - an ENABLED model is additionally checked against what the sidecar needs
//!   at runtime: a script that exists and a runtime block that names it. A
//!   disabled example may describe a script the site has not installed yet,
//!   which is what a one-line comment says to do; an enabled model with no
//!   script is a model that will never run and says nothing about it.
//!
//! Every problem in the directory is collected rather than thrown at the
//! first one: fixing a directory one error per run is how an operator
//! concludes the tool is lying.
//!
//! `default` consistency and the `repeats` / list rule live here too, because
//! they are about the paramETER LIST as a whole (bounds, options, duplicates)
//! rather than about one value, and a parser that refused them would report
//! the file's first complaint instead of all of them.

use super::schema::{Model, Parameter, ParameterType, Value};
use super::{Models, kind_produces, known_output_kind};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// One thing wrong with the models directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// The file, when the problem names one.
    pub file: Option<PathBuf>,
    /// The model, when the file is known.
    pub model: String,
    /// Where inside the model, e.g. `parameters[CLIP_LIMIT].default`.
    pub at: String,
    pub message: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(file) = &self.file {
            write!(f, "{}: ", file.display())?;
        }
        if self.at.is_empty() {
            writeln!(f, "model '{}': {}", self.model, self.message)
        } else {
            writeln!(f, "{}: {}", self.at, self.message)
        }
    }
}

/// Every problem found, in the order the files were read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diagnostics {
    pub problems: Vec<Problem>,
}

impl Diagnostics {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.problems.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.problems.len()
    }

    /// The whole answer as one error, for a caller that refuses to start.
    #[must_use]
    pub fn report(&self) -> String {
        let mut out = String::new();
        for (index, problem) in self.problems.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            out.push_str(&problem.to_string());
        }
        out
    }
}

/// Check the whole directory.
///
/// `dir` is the models directory, used to resolve a relative script: the
/// anchor is the FILE (`dir/<id>.yaml`), so the script a model names is
/// relative to the file that names it.
#[must_use]
pub fn check(dir: &Path, models: &Models) -> Diagnostics {
    let mut problems = Vec::new();
    for model in &models.models {
        let file = dir.join(format!("{}.{}", model.id, super::MODEL_EXTENSION));
        check_schema(model, &file, &mut problems);
        if model.enabled {
            check_enabled(model, &file, &mut problems);
        }
    }
    check_unique_procedure_codes(models, dir, &mut problems);
    Diagnostics { problems }
}

/// The tier every file goes through.
fn check_schema(model: &Model, file: &Path, problems: &mut Vec<Problem>) {
    let mut report = |at: String, message: String| {
        problems.push(Problem {
            file: Some(file.to_path_buf()),
            model: model.id.clone(),
            at,
            message,
        });
    };

    // outputs: an empty list is a model that produces nothing, and a kind
    // that no encoder implements is a file written against another build.
    if model.outputs.is_empty() {
        report(
            "outputs".to_string(),
            "a model produces at least one object; the list is empty".to_string(),
        );
    }
    let mut kinds = Vec::new();
    for (index, output) in model.outputs.iter().enumerate() {
        match known_output_kind(&output.kind) {
            Some(kind) => kinds.push(kind),
            None => report(
                format!("outputs[{index}].kind"),
                format!(
                    "'{}' is not a result kind this build encodes; expected one of {}",
                    output.kind,
                    crate::conf::ResultKind::ALL
                        .iter()
                        .map(|kind| kind.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
        }
    }
    // The SOP classes are the cross-tool half of the same fact: a class the
    // declared kinds do not produce is a capability the model claims and
    // cannot deliver.
    for (index, uid) in model.output_capabilities.sop_classes.iter().enumerate() {
        let produced = kinds.iter().any(|kind| kind_produces(*kind, uid));
        if !produced {
            report(
                format!("output_capabilities.sop_classes[{index}]"),
                format!(
                    "{uid} is not a SOP class the declared outputs produce; \
                     each entry must be the class of an outputs[].kind"
                ),
            );
        }
    }

    for (index, parameter) in model.parameters.iter().enumerate() {
        check_parameter(parameter, index, &mut report);
    }
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for (index, parameter) in model.parameters.iter().enumerate() {
        if let Some(first) = seen.insert(parameter.code.as_str(), index) {
            report(
                format!("parameters[{index}].code"),
                format!(
                    "duplicate parameter code '{}'; it is already declared at parameters[{first}] \
                     and a requester cannot tell the two apart",
                    parameter.code
                ),
            );
        }
    }
}

fn check_parameter(parameter: &Parameter, index: usize, report: &mut impl FnMut(String, String)) {
    let at = format!("parameters[{index}]");
    let named = format!("{at}[{}]", parameter.code);
    match (parameter.min, parameter.max) {
        (Some(min), Some(max)) if min > max => report(
            format!("{named}.min"),
            format!("min {min} is greater than max {max}; no value can satisfy both"),
        ),
        (Some(min), None) if !min.is_finite() => report(
            format!("{named}.min"),
            format!("{min} is not a finite bound"),
        ),
        (Some(min), _) if !min.is_finite() => report(
            format!("{named}.min"),
            format!("{min} is not a finite bound"),
        ),
        (_, Some(max)) if !max.is_finite() => report(
            format!("{named}.max"),
            format!("{max} is not a finite bound"),
        ),
        _ => {}
    }

    if parameter.r#type == ParameterType::Choice {
        if parameter.options.is_empty() {
            report(
                format!("{named}.options"),
                "a choice parameter lists the answers it accepts; the list is empty".to_string(),
            );
        } else if parameter
            .options
            .iter()
            .any(|option| option.value.is_empty())
        {
            report(
                format!("{named}.options"),
                "an option's value is empty; a requester cannot send it".to_string(),
            );
        }
    }
    if parameter.r#type != ParameterType::Choice && !parameter.options.is_empty() {
        report(
            format!("{named}.options"),
            format!(
                "options belong to a 'choice' parameter; this one is '{}'",
                parameter.r#type.as_str()
            ),
        );
    }

    let Some(default) = &parameter.default else {
        return;
    };
    // The list rule: a multi-select item takes a list, a single-valued one
    // does not. FHIR spells the first `repeats: true`.
    if let Value::List(items) = default {
        if !parameter.repeats {
            report(
                format!("{named}.default"),
                "a list default needs repeats: true; a single-valued item has one answer"
                    .to_string(),
            );
            return;
        }
        if items.is_empty() {
            report(
                format!("{named}.default"),
                "an empty list is not a default; omit the key instead".to_string(),
            );
            return;
        }
        for (item_index, item) in items.iter().enumerate() {
            check_value(
                parameter,
                item,
                &format!("{named}.default[{item_index}]"),
                report,
            );
        }
        return;
    }
    check_value(parameter, default, &format!("{named}.default"), report);
}

/// One answer checked against the parameter that declares it.
fn check_value(
    parameter: &Parameter,
    value: &Value,
    at: &str,
    report: &mut impl FnMut(String, String),
) {
    if !parameter.r#type.accepts(value) {
        report(
            at.to_string(),
            format!(
                "expected {}, found {}",
                parameter.r#type.expected(),
                describe(value)
            ),
        );
        return;
    }
    if let Some(number) = value.as_number() {
        if let Some(min) = parameter.min
            && number < min
        {
            report(
                at.to_string(),
                format!("{number} is below the minimum {min}"),
            );
            return;
        }
        if let Some(max) = parameter.max
            && number > max
        {
            report(
                at.to_string(),
                format!("{number} is above the maximum {max}"),
            );
            return;
        }
    }
    if !parameter.options.is_empty()
        && let Some(text) = value.as_text()
        && !parameter.options.iter().any(|option| option.value == text)
    {
        report(
            at.to_string(),
            format!(
                "'{text}' is not one of the declared options ({})",
                parameter
                    .options
                    .iter()
                    .map(|option| option.value.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }
}

fn describe(value: &Value) -> String {
    match value {
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Text(text) => format!("\"{text}\""),
        Value::List(items) => format!("a list of {} value(s)", items.len()),
    }
}

/// The tier only an ENABLED model goes through.
fn check_enabled(model: &Model, file: &Path, problems: &mut Vec<Problem>) {
    let mut report = |at: String, message: String| {
        problems.push(Problem {
            file: Some(file.to_path_buf()),
            model: model.id.clone(),
            at,
            message,
        });
    };
    let Some(runtime) = &model.runtime else {
        report(
            "runtime".to_string(),
            "an enabled model declares runtime.script; without it nothing can run".to_string(),
        );
        return;
    };
    // The script is resolved against the model FILE's directory, and it is
    // resolved here as well as by the sidecar: a start that discovers a
    // missing script one workitem at a time is a start that should have
    // refused.
    let dir = file.parent().unwrap_or(Path::new("."));
    let script = if runtime.script.is_absolute() {
        runtime.script.clone()
    } else {
        dir.join(&runtime.script)
    };
    if !script.is_file() {
        report(
            "runtime.script".to_string(),
            format!("{} is not a file", script.display()),
        );
    }
}

/// One procedure, one model.
///
/// A duplicate (0040,4018) across enabled files is not a schema error - each
/// file is a well-formed model - but two models claiming to perform the same
/// procedure would both match the same workitem and both store a result,
/// which is what an operator reading the model list has no way to know.
fn check_unique_procedure_codes(models: &Models, dir: &Path, problems: &mut Vec<Problem>) {
    let mut seen: HashMap<(&str, &str), &str> = HashMap::new();
    for model in models.enabled() {
        let Some(code) = &model.procedure_code else {
            continue;
        };
        let key = (code.value.as_str(), code.scheme.as_str());
        if let Some(first) = seen.get(&key) {
            problems.push(Problem {
                file: Some(dir.join(format!("{}.{}", model.id, super::MODEL_EXTENSION))),
                model: model.id.clone(),
                at: "procedure_code".to_string(),
                message: format!(
                    "procedure {} ({}) is already performed by model '{first}'; \
                     two enabled models for one procedure both claim the same workitem",
                    code.value, code.scheme
                ),
            });
            continue;
        }
        seen.insert(key, model.id.as_str());
    }
}
