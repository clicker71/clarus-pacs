// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The shape of one model file.
//!
//! The types here are what [`crate::models::parse`] produces and what the
//! Questionnaire serializer reads. They carry no behaviour beyond parsing,
//! because the decisions about a model (does its parameter list make sense,
//! can this build produce its outputs) belong to the validation tiers and are
//! reported together rather than at the first complaint.
//!
//! `parameters[].type` is the FHIR R4 `Questionnaire.item.type` vocabulary
//! verbatim: the card in `standard/hl7/fhir-r4/` is the list, the parser
//! accepts exactly it, and the serializer writes the same string back. An
//! answer type invented here would make the Questionnaire a different
//! document from the one the standard defines, which is the whole reason
//! the vocabulary was carded rather than typed from memory.

use crate::conf::MAX_RESULT_BYTES_CEILING;
use crate::value::{ConfError, Result, Section};
use crate::yaml::Yaml;
use std::path::PathBuf;

/// One model, as its file declares it.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    /// The file stem: the model's identifier here and in the Questionnaire.
    pub id: String,
    /// Whether the sidecar registers this model. Default true, so a file
    /// that says nothing good is a model that runs.
    pub enabled: bool,
    pub title: Option<String>,
    pub description: Option<String>,
    /// The system identifying information an AISPro consumer reads.
    pub provider: Option<Provider>,
    /// (0040,4018) Scheduled Workitem Code Sequence, as this model states
    /// which procedure it performs.
    pub procedure_code: Option<Code>,
    pub input_requirements: InputRequirements,
    pub output_capabilities: OutputCapabilities,
    pub parameters: Vec<Parameter>,
    /// The study filter. clinfer-only: no consumer of the declared
    /// capabilities has a use for it.
    pub r#match: crate::conf::Match,
    /// How to run the processor. clinfer-only. `None` is legal at parse time
    /// and is refused for an ENABLED model by the validation, because a
    /// disabled example may legitimately show the fields without a script
    /// that can run.
    pub runtime: Option<Runtime>,
    /// The objects a run produces. clinfer-only.
    pub outputs: Vec<Output>,
}

impl Model {
    /// The script, already resolved against the model file's directory.
    pub fn script(&self) -> Option<PathBuf> {
        let runtime = self.runtime.as_ref()?;
        Some(runtime.script.clone())
    }
}

/// System identifying information: name, vendor, version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    pub name: String,
    pub vendor: Option<String>,
    pub version: Option<String>,
}

/// A coded concept with a scheme, as the model declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Code {
    pub value: String,
    pub scheme: String,
    pub meaning: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InputRequirements {
    /// (0008,0061) values the model accepts, upper-cased.
    pub modalities: Vec<String>,
    /// (0028,0004) values the model accepts, upper-cased.
    pub photometric_interpretation: Vec<String>,
    /// The body regions, CODED so a consumer filters on a code rather than
    /// on display text. The target vocabulary is SNOMED CT (the FHIR
    /// bodySite binding) or RadLex.
    pub anatomy: Vec<Anatomy>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anatomy {
    /// The code system, e.g. `http://snomed.info/sct`.
    pub system: String,
    pub code: String,
    pub meaning: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OutputCapabilities {
    /// SOP Class UIDs (PS3.6) of the objects the model's outputs produce.
    pub sop_classes: Vec<String>,
}

/// One tunable the requester may set through (0074,1210).
#[derive(Debug, Clone, PartialEq)]
pub struct Parameter {
    /// The Concept Name Code Value: the key a script reads.
    pub code: String,
    /// The FHIR `Questionnaire.item.type` value.
    pub r#type: ParameterType,
    pub default: Option<Value>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// The answers a `choice` may take, keyed by the code the requester
    /// sends and carrying the display text a viewer shows.
    pub options: Vec<Choice>,
    /// FHIR `item.repeats`: true means a multi-select item whose default is
    /// allowed to be a list.
    pub repeats: bool,
    /// The human text a viewer shows for the item.
    pub text: Option<String>,
}

/// One admissible answer of a `choice` parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// The value the requester sends: the code of a CODED parameter, or the
    /// text of a TEXT one.
    pub value: String,
    /// The display text, when the answer is coded.
    pub display: Option<String>,
}

/// A parameter's default, typed by the parameter.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    /// A number with no declared width: the shape the YAML has, not the
    /// shape the parameter will take.
    Number(f64),
    Text(String),
    /// A list, legal only on a `repeats: true` parameter.
    List(Vec<Value>),
}

impl Value {
    /// The value as text, when it is text.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The value as a number, when it is one.
    #[must_use]
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Self::Number(number) => Some(*number),
            _ => None,
        }
    }

    /// The value as a boolean, when it is one.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(flag) => Some(*flag),
            _ => None,
        }
    }
}

/// The FHIR R4 `Questionnaire.item.type` values this sidecar accepts.///
/// The card in `standard/hl7/fhir-r4/questionnaire-item-type-valueset.json`
/// is the source. The three hierarchy roots (`group`, `display`,
/// `question`) are NOT answer types and are refused for a parameter: a
/// parameter is an answer, and an item that captures no answer cannot carry
/// a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterType {
    Boolean,
    Decimal,
    Integer,
    Date,
    DateTime,
    Time,
    String,
    Text,
    Url,
    Choice,
    OpenChoice,
    Attachment,
    Reference,
    Quantity,
}

impl ParameterType {
    /// Every answer type, in the order the value set lists them.
    pub const ALL: [ParameterType; 14] = [
        Self::Boolean,
        Self::Decimal,
        Self::Integer,
        Self::Date,
        Self::DateTime,
        Self::Time,
        Self::String,
        Self::Text,
        Self::Url,
        Self::Choice,
        Self::OpenChoice,
        Self::Attachment,
        Self::Reference,
        Self::Quantity,
    ];

    /// The name the FHIR value set gives the type, spaced as the standard
    /// spells it (`open-choice`, `dateTime`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Boolean => "boolean",
            Self::Decimal => "decimal",
            Self::Integer => "integer",
            Self::Date => "date",
            Self::DateTime => "dateTime",
            Self::Time => "time",
            Self::String => "string",
            Self::Text => "text",
            Self::Url => "url",
            Self::Choice => "choice",
            Self::OpenChoice => "open-choice",
            Self::Attachment => "attachment",
            Self::Reference => "reference",
            Self::Quantity => "quantity",
        }
    }

    /// The type a name denotes, from the value set. The comparison is exact:
    /// the codes are case-sensitive, so `Decimal` is not `decimal`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }

    /// Whether a default of this shape is the right kind of answer.
    ///
    /// The QUESTION types are answerable; the three root types are not, so
    /// they are not in [`Self::ALL`] at all. `choice` and `open-choice`
    /// answers are coded, which is a string here because the code is what
    /// the requester sends and the display text is the option's, not the
    /// default's.
    #[must_use]
    pub fn accepts(self, value: &Value) -> bool {
        match (self, value) {
            (Self::Boolean, Value::Bool(_)) => true,
            (Self::Integer, Value::Number(number)) => number.is_finite() && number.fract() == 0.0,
            (Self::Decimal | Self::Quantity, Value::Number(number)) => number.is_finite(),
            (Self::String | Self::Text | Self::Url, Value::Text(_)) => true,
            (Self::Date | Self::DateTime | Self::Time, Value::Text(_)) => true,
            (Self::Choice | Self::OpenChoice, Value::Text(_)) => true,
            (Self::Attachment | Self::Reference, Value::Text(_)) => true,
            _ => false,
        }
    }

    /// How the type is spelled in a refusal.
    #[must_use]
    pub fn expected(self) -> &'static str {
        match self {
            Self::Boolean => "a boolean",
            Self::Integer => "a whole number",
            Self::Decimal | Self::Quantity => "a number",
            Self::Choice | Self::OpenChoice => "the value of one of its options",
            _ => "a string",
        }
    }
}

/// How to run the model's script.
#[derive(Debug, Clone, PartialEq)]
pub struct Runtime {
    /// Absolute, or resolved against the model file's directory.
    pub script: PathBuf,
    /// The interpreter, `None` for the default.
    pub interpreter: Option<PathBuf>,
    pub timeout_secs: u32,
    pub max_result_bytes: u64,
}

/// One object a run produces.
#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    /// Encoder kind: the name of a [`ResultKind`].
    pub kind: String,
    pub series_description: Option<String>,
    pub profile_version: u32,
}

// -- Readers ------------------------------------------------------------

/// Read the `provider:` block.
pub fn parse_provider(value: &Yaml, at: &str) -> Result<Provider> {
    let mut section = Section::wrap(value, at)?;
    let provider = Provider {
        name: section.require_str("name")?.trim().to_string(),
        vendor: section.opt_str("vendor")?.map(str::to_string),
        version: section.opt_str("version")?.map(str::to_string),
    };
    section.finish("a provider block")?;
    if provider.name.is_empty() {
        return Err(ConfError::new(
            format!("{at}.name"),
            "must not be empty; it is how the model is named to a consumer",
        ));
    }
    Ok(provider)
}

/// Read a `{value, scheme, meaning}` triple.
pub fn parse_code(value: &Yaml, at: &str) -> Result<Code> {
    let mut section = Section::wrap(value, at)?;
    let code = Code {
        value: section.require_str("value")?.trim().to_string(),
        scheme: section.require_str("scheme")?.trim().to_string(),
        meaning: section.opt_str("meaning")?.map(str::to_string),
    };
    section.finish("a code block")?;
    if code.value.is_empty() {
        return Err(ConfError::new(
            format!("{at}.value"),
            "must not be empty; it is the Code Value a requester matches",
        ));
    }
    if code.scheme.is_empty() {
        return Err(ConfError::new(
            format!("{at}.scheme"),
            "must not be empty; a Code Value with no designator names nothing",
        ));
    }
    Ok(code)
}

/// Read the `parameters:` list.
pub fn parse_parameters(root: &mut Section<'_>) -> Result<Vec<Parameter>> {
    let items = root.opt_section_list("parameters")?;
    let mut out = Vec::with_capacity(items.len());
    for mut item in items {
        let at = item.at().to_string();
        let code = item.require_str("code")?.trim().to_string();
        if code.is_empty() {
            return Err(ConfError::new(
                format!("{at}.code"),
                "must not be empty; it is the Concept Name Code Value a script reads",
            ));
        }
        let text = item.opt_str("text")?.map(str::to_string);
        let type_name = item.require_str("type")?.trim().to_string();
        let r#type = ParameterType::parse(&type_name).ok_or_else(|| {
            ConfError::new(
                format!("{at}.type"),
                format!(
                    "'{type_name}' is not a Questionnaire item type; expected one of {}",
                    ParameterType::ALL
                        .iter()
                        .map(|kind| kind.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
        })?;
        let repeats = item.opt_bool("repeats")?.unwrap_or(false);
        let default = match item.take("default")? {
            Some(value) => Some(parse_value(value, &format!("{at}.default"))?),
            None => None,
        };
        let min = item.opt_float("min")?;
        let max = item.opt_float("max")?;
        let options = match item.take("options")? {
            Some(value) => parse_options(value, &format!("{at}.options"))?,
            None => Vec::new(),
        };
        item.finish("a parameter")?;
        out.push(Parameter {
            code,
            r#type,
            default,
            min,
            max,
            options,
            repeats,
            text,
        });
    }
    Ok(out)
}

/// One default value, kept in the shape the file has.
fn parse_value(value: &Yaml, at: &str) -> Result<Value> {
    match value {
        Yaml::Bool(flag) => Ok(Value::Bool(*flag)),
        Yaml::Int(number) => Ok(Value::Number(*number as f64)),
        Yaml::Float(number) => Ok(Value::Number(*number)),
        Yaml::Str(text) => Ok(Value::Text(text.clone())),
        Yaml::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                out.push(parse_value(item, &format!("{at}[{index}]"))?);
            }
            Ok(Value::List(out))
        }
        Yaml::Map(_) => Err(ConfError::new(
            at,
            "a default is a scalar or a list of scalars, not a mapping",
        )),
    }
}

/// The admissible answers of a `choice`.
fn parse_options(value: &Yaml, at: &str) -> Result<Vec<Choice>> {
    let Some(items) = value.as_list() else {
        return Err(ConfError::type_error(at, "a list", value));
    };
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let item_at = format!("{at}[{index}]");
        // A bare scalar is accepted as the option's value: a parameter whose
        // answers are plain words (`clahe`, `lanczos`) has no display text
        // that differs from the value, and demanding a block for it would be
        // ceremony around the common case.
        if let Yaml::Str(text) = item {
            out.push(Choice {
                value: text.trim().to_string(),
                display: None,
            });
            continue;
        }
        let mut section = Section::wrap(item, &item_at)?;
        let option = Choice {
            value: section.require_str("value")?.trim().to_string(),
            display: section.opt_str("display")?.map(str::to_string),
        };
        section.finish("an option")?;
        out.push(option);
    }
    Ok(out)
}

/// Read the `outputs:` list.
pub fn parse_outputs(root: &mut Section<'_>) -> Result<Vec<Output>> {
    let items = root.opt_section_list("outputs")?;
    let mut out = Vec::with_capacity(items.len());
    for mut item in items {
        let kind = item.require_str("kind")?.trim().to_ascii_lowercase();
        let series_description = item.opt_str("series_description")?.map(str::to_string);
        let profile_version = match item.opt_bounded_int("profile_version", 1, 1_000_000)? {
            Some(value) => u32::try_from(value).map_err(|_| {
                ConfError::new(
                    format!("{}.profile_version", item.at()),
                    format!("{value} does not fit a version count"),
                )
            })?,
            None => 1,
        };
        item.finish("an output")?;
        out.push(Output {
            kind,
            series_description,
            profile_version,
        });
    }
    Ok(out)
}

/// The size ceiling a `runtime.max_result_bytes` is checked against, so the
/// model layer and the base configuration cannot disagree about it.
pub const RESULT_BYTES_CEILING: u64 = MAX_RESULT_BYTES_CEILING;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conf::ResultKind;

    #[test]
    fn every_value_set_answer_type_round_trips() {
        for kind in ParameterType::ALL {
            assert_eq!(ParameterType::parse(kind.as_str()), Some(kind));
        }
        // The three hierarchy roots are not answer types: a parameter
        // carrying one captures no answer.
        for root in ["group", "display", "question"] {
            assert_eq!(ParameterType::parse(root), None, "{root} is not an answer");
        }
        // The spelling is the value set's, not the Rust variant's.
        assert_eq!(ParameterType::OpenChoice.as_str(), "open-choice");
        assert_eq!(ParameterType::DateTime.as_str(), "dateTime");
    }

    #[test]
    fn the_answer_type_names_are_case_sensitive() {
        // A code-typed value is compared exactly: `Decimal` would be a
        // different concept to a FHIR consumer.
        assert_eq!(ParameterType::parse("Decimal"), None);
        assert_eq!(
            ParameterType::parse("dateTime"),
            Some(ParameterType::DateTime)
        );
        assert_eq!(ParameterType::parse("datetime"), None);
    }

    #[test]
    fn a_default_is_accepted_by_the_type_it_is_declared_for() {
        assert!(ParameterType::Boolean.accepts(&Value::Bool(true)));
        assert!(!ParameterType::Boolean.accepts(&Value::Number(1.0)));
        assert!(ParameterType::Integer.accepts(&Value::Number(8.0)));
        assert!(
            !ParameterType::Integer.accepts(&Value::Number(8.5)),
            "a fractional default is not an integer answer"
        );
        assert!(ParameterType::Decimal.accepts(&Value::Number(2.5)));
        assert!(ParameterType::Text.accepts(&Value::Text("x".into())));
        assert!(!ParameterType::Text.accepts(&Value::Number(8.0)));
    }

    #[test]
    fn the_sop_classes_follow_the_result_matrix() {
        assert_eq!(
            crate::models::sop_classes(ResultKind::Pr),
            ["1.2.840.10008.5.1.4.1.1.11.1"]
        );
        assert_eq!(
            crate::models::sop_classes(ResultKind::Rtstruct),
            ["1.2.840.10008.5.1.4.1.1.481.3"]
        );
        assert!(
            crate::models::kind_produces(ResultKind::Sr, "1.2.840.10008.5.1.4.1.1.88.22"),
            "an enhanced SR is what a measurements run writes"
        );
        assert!(
            !crate::models::kind_produces(ResultKind::Pr, "1.2.840.10008.5.1.4.1.1.66.4"),
            "a presentation state never produces a segmentation"
        );
    }
}
