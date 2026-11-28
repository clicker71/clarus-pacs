// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The models as FHIR R4 `Questionnaire` resources.
//!
//! This is the standard half of the model layer, and it is what `/processors`
//! answers with. A consumer reads the model's declared capabilities as a
//! Questionnaire: `title` and `description`, the procedure as `code`, the
//! body regions an input requires in a `bodySite` extension, and each
//! parameter as one `item` whose `type`, `repeats`, `answerOption` and
//! `initial` are the YAML fields under the standard's own names.
//!
//! # What is NOT in the document
//!
//! `match` and `runtime` are clinfer-only and never appear. That is a rule
//! rather than an omission: the served document is the model's declaration
//! to the WORLD, and a workdir path or a Python interpreter in it is
//! deployment detail leaking into a capability statement. `outputs` is
//! different: `series_description` is capability - the "description of the
//! result content" a consumer asks for - and IS served as the result-content
//! extension, while the rest of an output stays internal.
//!
//! # Written by hand
//!
//! The JSON is built with the crate's own string writer, like `json.rs`
//! writes DICOM JSON and `yaml.rs` parses the model files. A serializer
//! dependency for one fixed document shape would be a dependency for the
//! shape that is most stable here.
//!
//! The one value that is NOT a plain string on the wire is a `decimal`
//! default: FHIR carries it as a JSON number, so the writer has to be able to
//! emit a number rather than a quoted one, which is why [`Value`] keeps its
//! shape instead of collapsing to text at parse time.

use crate::models::Models;
use crate::models::schema::{Choice, Model, Parameter, ParameterType, Value};

/// The file names of the extension StructureDefinitions, resolved against the
/// configured extension base URL (`fhir.extension_base` in the configuration;
/// default [`crate::conf::DEFAULT_EXTENSION_BASE`]). The extension `url` SHALL
/// be an absolute URL (the canonical URL of its StructureDefinition), never a
/// URN; a site hosts the definitions where its deployment can reach them, so
/// the base is a deployment setting rather than a constant.
const ANATOMY_DEFINITION: &str = "clinfer-input-anatomy.json";
const OUTPUT_DEFINITION: &str = "clinfer-output-sop-classes.json";
const RESULT_CONTENT_DEFINITION: &str = "clinfer-output-result-content.json";
const PROVIDER_DEFINITION: &str = "clinfer-provider.json";

/// The absolute canonical URL of one extension definition.
fn extension_url(base: &str, file: &str) -> String {
    format!("{base}/{file}")
}

/// The FHIR `Coding.system` for a model's DICOM coding scheme designator.
///
/// `Coding.system` is a `uri`: a bare DICOM scheme (`99CLARUS`, `SRT`) has no
/// URI form and must be mapped. A scheme that is already a URI
/// (`http://snomed.info/sct`) passes through unchanged; a private DICOM
/// scheme becomes a URN under this deployment's namespace, which a consumer
/// can round-trip back to the scheme.
fn coding_system(scheme: &str) -> String {
    if scheme.contains("://") || scheme.starts_with("urn:") {
        scheme.to_string()
    } else {
        format!("urn:clarus:scheme:{}", scheme.to_ascii_lowercase())
    }
}

/// One enabled model as a Questionnaire.
#[must_use]
pub fn questionnaire(model: &Model, extension_base: &str) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str("{\"resourceType\":\"Questionnaire\",\"id\":");
    push_string(&mut out, &model.id);
    out.push_str(",\"status\":\"active\",\"title\":");
    // A model with no `title` is named by its identifier: a document with an
    // empty title is a document a picker shows as blank.
    push_string(&mut out, model.title.as_deref().unwrap_or(&model.id));
    // `description` is a Questionnaire field of its own (markdown), so the
    // model's own text goes there and a consumer needs no extension to read
    // it.
    if let Some(description) = &model.description {
        out.push_str(",\"description\":");
        push_string(&mut out, description);
    }
    if let Some(code) = &model.procedure_code {
        out.push_str(",\"code\":[");
        push_codeable(
            &mut out,
            &coding_system(&code.scheme),
            &code.value,
            code.meaning.as_deref(),
        );
        out.push(']');
    }
    push_extensions(&mut out, model, extension_base);
    out.push_str(",\"item\":[");
    for (index, parameter) in model.parameters.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_item(&mut out, parameter);
    }
    out.push_str("]}");
    out
}

/// The extensions the standard has no field for.
fn push_extensions(out: &mut String, model: &Model, extension_base: &str) {
    let mut extensions: Vec<String> = Vec::new();

    // The provider block is a COMPLEX extension: one parent URL with child
    // extensions (name, vendor, version), each a `valueString`. A JSON object
    // under `valueString` would not be valid FHIR - a valueString is a string
    // primitive, not an object.
    if let Some(provider) = &model.provider {
        let provider_url = extension_url(extension_base, PROVIDER_DEFINITION);
        let mut value = String::from("{\"url\":");
        push_string(&mut value, &provider_url);
        value.push_str(",\"extension\":[{\"url\":\"name\",\"valueString\":");
        push_string(&mut value, &provider.name);
        value.push('}');
        if let Some(vendor) = &provider.vendor {
            value.push_str(",{\"url\":\"vendor\",\"valueString\":");
            push_string(&mut value, vendor);
            value.push('}');
        }
        if let Some(version) = &provider.version {
            value.push_str(",{\"url\":\"version\",\"valueString\":");
            push_string(&mut value, version);
            value.push('}');
        }
        value.push_str("]}");
        extensions.push(value);
    }
    // The anatomy is a CodeableConcept list, which a single standard
    // extension value cannot carry: one extension per region, each with the
    // standard `valueCodeableConcept`.
    for anatomy in &model.input_requirements.anatomy {
        let anatomy_url = extension_url(extension_base, ANATOMY_DEFINITION);
        let mut value = String::from("{\"url\":");
        push_string(&mut value, &anatomy_url);
        value.push_str(",\"valueCodeableConcept\":");
        let mut codeable = String::new();
        push_codeable(
            &mut codeable,
            &anatomy.system,
            &anatomy.code,
            anatomy.meaning.as_deref(),
        );
        value.push_str(&codeable);
        value.push('}');
        extensions.push(value);
    }
    // The SOP classes are one list of codes, so the standard Extension
    // shape is an extension per class (each carrying `valueUri`), not a
    // JSON blob: a blob would be a private vocabulary inside a document that
    // claims to be FHIR.
    for uid in &model.output_capabilities.sop_classes {
        let output_url = extension_url(extension_base, OUTPUT_DEFINITION);
        let mut value = String::from("{\"url\":");
        push_string(&mut value, &output_url);
        value.push_str(",\"valueUri\":");
        push_string(&mut value, uid);
        value.push('}');
        extensions.push(value);
    }
    // The result-content description is what an AISPro consumer reads as the
    // "description of the result content": one extension per output, each
    // carrying the series description as `valueString`. `kind` is not served:
    // it names an encoder, and a single valueString carries one value, so the
    // description alone is the smallest retrievable shape.
    for output in &model.outputs {
        if let Some(series_description) = &output.series_description {
            let result_url = extension_url(extension_base, RESULT_CONTENT_DEFINITION);
            let mut value = String::from("{\"url\":");
            push_string(&mut value, &result_url);
            value.push_str(",\"valueString\":");
            push_string(&mut value, series_description);
            value.push('}');
            extensions.push(value);
        }
    }
    if extensions.is_empty() {
        return;
    }
    out.push_str(",\"extension\":[");
    for (index, extension) in extensions.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(extension);
    }
    out.push(']');
}

fn push_codeable(out: &mut String, system: &str, code: &str, display: Option<&str>) {
    out.push_str("{\"coding\":[{\"system\":");
    push_string(out, system);
    out.push_str(",\"code\":");
    push_string(out, code);
    if let Some(display) = display {
        out.push_str(",\"display\":");
        push_string(out, display);
    }
    out.push_str("}]");
    if let Some(display) = display {
        out.push_str(",\"text\":");
        push_string(out, display);
    }
    out.push('}');
}

/// One `Questionnaire.item`.
fn push_item(out: &mut String, parameter: &Parameter) {
    out.push_str("{\"linkId\":");
    push_string(out, &parameter.code);
    out.push_str(",\"type\":");
    push_string(out, parameter.r#type.as_str());
    // `text` is the human label. The parameter's own text wins; otherwise
    // the Code Value is what a viewer shows, which is worse than a label and
    // better than a blank row.
    out.push_str(",\"text\":");
    push_string(out, parameter.text.as_deref().unwrap_or(&parameter.code));
    // `required` is true for every parameter: a parameter the model reads is
    // a parameter the requester has to state, or the model's own default
    // applies - and the default IS stated below (in `initial`, or on the
    // matching `answerOption` as `initialSelected`), so the item is never
    // left unanswered by a consumer that honours it.
    out.push_str(",\"required\":true");
    out.push_str(",\"repeats\":");
    out.push_str(if parameter.repeats { "true" } else { "false" });
    out.push_str(",\"code\":[");
    push_codeable(
        out,
        LOCAL_PARAMETER_SCHEME,
        &parameter.code,
        parameter.text.as_deref(),
    );
    out.push(']');
    if !parameter.options.is_empty() {
        out.push_str(",\"answerOption\":[");
        for (index, option) in parameter.options.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str("{\"valueCoding\":{\"code\":");
            push_string(out, &option.value);
            if let Some(display) = &option.display {
                out.push_str(",\"display\":");
                push_string(out, display);
            }
            out.push('}');
            // que-11: a choice default is the matching option's
            // `initialSelected`, never a separate `initial`.
            if let Some(default) = &parameter.default
                && option_is_default(option, default)
            {
                out.push_str(",\"initialSelected\":true");
            }
            out.push('}');
        }
        out.push(']');
    }
    // que-11: `initial` is absent when `answerOption` is present; a choice
    // default was written as `initialSelected` above.
    if let Some(default) = &parameter.default
        && parameter.options.is_empty()
    {
        // `initial` is a list of items, one per answer; a list default is
        // one entry per value, which is what `repeats: true` means.
        out.push_str(",\"initial\":[");
        match default {
            Value::List(values) => {
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    push_initial(out, parameter.r#type, value);
                }
            }
            value => push_initial(out, parameter.r#type, value),
        }
        out.push(']');
    }
    out.push('}');
}

/// Whether the option carries the parameter's default: the answerOption to
/// mark `initialSelected` (que-11 replaces a separate `initial` with it).
fn option_is_default(option: &Choice, default: &Value) -> bool {
    let code = &option.value;
    match default {
        Value::Text(value) => value == code,
        Value::List(items) => items
            .iter()
            .any(|item| matches!(item, Value::Text(value) if value == code)),
        _ => false,
    }
}

/// One answer in `initial`: the value property is named after the item type,
/// exactly as the value set's own `answerValueProperty` column spells it.
fn push_initial(out: &mut String, r#type: ParameterType, value: &Value) {
    let property = initial_property(r#type);
    out.push('{');
    push_string(out, property);
    out.push(':');
    match (r#type, value) {
        (ParameterType::Boolean, Value::Bool(flag)) => {
            out.push_str(if *flag { "true" } else { "false" });
        }
        (ParameterType::Integer, Value::Number(number)) => {
            out.push_str(&format!("{}", *number as i64));
        }
        (ParameterType::Decimal | ParameterType::Quantity, Value::Number(number)) => {
            out.push_str(&number.to_string());
        }
        (ParameterType::Choice | ParameterType::OpenChoice, Value::Text(text)) => {
            out.push_str("{\"code\":");
            push_string(out, text);
            out.push('}');
        }
        (_, Value::Text(text)) => push_string(out, text),
        // The validation refuses a default that does not fit its type before
        // this is reached; a mismatch here would still have to be valid JSON,
        // so the text form is written rather than nothing.
        (_, other) => push_string(out, &other.to_string_hint()),
    }
    out.push('}');
}

/// The value property the item type answers with, per the value set's
/// `answerValueProperty` column.
#[must_use]
pub fn initial_property(r#type: ParameterType) -> &'static str {
    match r#type {
        ParameterType::Boolean => "valueBoolean",
        ParameterType::Decimal => "valueDecimal",
        ParameterType::Integer => "valueInteger",
        ParameterType::Date => "valueDate",
        ParameterType::DateTime => "valueDateTime",
        ParameterType::Time => "valueTime",
        ParameterType::String | ParameterType::Text => "valueString",
        ParameterType::Url => "valueUri",
        ParameterType::Quantity => "valueQuantity",
        ParameterType::Choice | ParameterType::OpenChoice => "valueCoding",
        ParameterType::Attachment | ParameterType::Reference => "valueString",
    }
}

impl Value {
    /// The value spelled as a string, for the serializer's last resort.
    fn to_string_hint(&self) -> String {
        match self {
            Self::Bool(flag) => flag.to_string(),
            Self::Number(number) => number.to_string(),
            Self::Text(text) => text.clone(),
            Self::List(_) => String::new(),
        }
    }
}

/// The `Coding.system` the model files' own parameter Code Values live in.
///
/// A local parameter code (`CLIP_LIMIT`) is not a SNOMED concept, so the
/// served item code names a URN namespace this deployment owns rather than
/// pretending it is one. A URN is an identifier, not an address: it does not
/// resolve and an administrator never has to configure DNS for it.
pub const LOCAL_PARAMETER_SCHEME: &str = "urn:clarus:codes:parameter";

/// A `Bundle` of Questionnaires: the answer `GET {base}/Questionnaire` gives.
///
/// `base` is the request's own origin (`http://host:port`), which makes the
/// `link.url` and every `fullUrl` ABSOLUTE - the form FHIR requires
/// (`Bundle.link.url` is a uri; `Bundle.entry.fullUrl` is an absolute URL
/// server address or a UUID/OID URI). The same document served under another
/// host therefore names that host.
#[must_use]
pub fn search_bundle(
    models: &Models,
    total: Option<usize>,
    base: &str,
    extension_base: &str,
) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("{\"resourceType\":\"Bundle\",\"type\":\"searchset\"");
    if let Some(total) = total {
        out.push_str(",\"total\":");
        out.push_str(&total.to_string());
    }
    out.push_str(",\"link\":[{\"relation\":\"self\",\"url\":");
    push_string(&mut out, &format!("{base}/processors/Questionnaire"));
    out.push_str("}],\"entry\":[");
    for (index, model) in models.enabled().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str("{\"fullUrl\":");
        push_string(
            &mut out,
            &format!("{base}/processors/Questionnaire/{}", model.id),
        );
        out.push_str(",\"resource\":");
        out.push_str(&questionnaire(model, extension_base));
        out.push('}');
    }
    out.push_str("]}");
    out
}

/// What one request to the Questionnaire route answers.
///
/// The listener needs to tell "this model exists" from "it does not" without
/// inspecting the body, so the distinction is in the type rather than in a
/// string comparison on `resourceType`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// A `searchset` Bundle of the enabled models.
    Bundle(String),
    /// One Questionnaire resource.
    Resource(String),
    /// The id names no enabled model.
    NotFound,
}

/// One Questionnaire by id: the answer `GET {base}/Questionnaire/{id}` gives.
#[must_use]
pub fn by_id(models: &Models, id: &str, extension_base: &str) -> Answer {
    match models.enabled().find(|model| model.id == id) {
        Some(model) => Answer::Resource(questionnaire(model, extension_base)),
        None => Answer::NotFound,
    }
}

/// Append one JSON string literal.
///
/// The escaping is the crate's own convention (`receiver::push_json_string`):
/// a quote, a backslash and the control characters, with the `\u` form for
/// anything below a space. Everything else - including non-ASCII text from a
/// model's title - is written as UTF-8, which is what the `application/fhir+json`
/// media type means.
fn push_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if control < ' ' => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conf::Match;
    use crate::models::schema::{Choice, InputRequirements, Output, OutputCapabilities};

    #[test]
    fn a_choice_default_is_initial_selected_not_initial() {
        let model = Model {
            id: "proc".to_string(),
            enabled: true,
            title: None,
            description: None,
            provider: None,
            procedure_code: None,
            input_requirements: InputRequirements::default(),
            output_capabilities: OutputCapabilities::default(),
            parameters: vec![Parameter {
                code: "ALGORITHM".to_string(),
                r#type: ParameterType::Choice,
                default: Some(Value::Text("clahe".to_string())),
                min: None,
                max: None,
                options: vec![
                    Choice {
                        value: "clahe".to_string(),
                        display: Some("CLAHE".to_string()),
                    },
                    Choice {
                        value: "none".to_string(),
                        display: None,
                    },
                ],
                repeats: false,
                text: None,
            }],
            r#match: Match::default(),
            runtime: None,
            outputs: vec![],
        };
        let json = questionnaire(&model, "http://example.org/fhir");
        assert!(
            json.contains("\"initialSelected\":true"),
            "the matching answerOption is marked initialSelected: {json}"
        );
        assert!(
            !json.contains("\"initial\":"),
            "que-11: no separate initial when answerOption is present: {json}"
        );
        assert!(
            json.contains("\"required\":true"),
            "an item is required: {json}"
        );
        assert!(
            !json.contains("\"required\":false"),
            "no item may claim it is not required: {json}"
        );
    }

    #[test]
    fn an_output_series_description_is_served_as_result_content() {
        let model = Model {
            id: "proc".to_string(),
            enabled: true,
            title: None,
            description: None,
            provider: None,
            procedure_code: None,
            input_requirements: InputRequirements::default(),
            output_capabilities: OutputCapabilities::default(),
            parameters: vec![],
            r#match: Match::default(),
            runtime: None,
            outputs: vec![Output {
                kind: "derived".to_string(),
                series_description: Some("CLAHE".to_string()),
                profile_version: 1,
            }],
        };
        let json = questionnaire(&model, "http://example.org/fhir");
        assert!(
            json.contains("\"url\":\"http://example.org/fhir/clinfer-output-result-content.json\""),
            "the result-content extension is emitted: {json}"
        );
        assert!(
            json.contains("\"valueString\":\"CLAHE\""),
            "the series description is carried as valueString: {json}"
        );
    }

    #[test]
    fn an_output_without_series_description_has_no_result_content() {
        let model = Model {
            id: "proc".to_string(),
            enabled: true,
            title: None,
            description: None,
            provider: None,
            procedure_code: None,
            input_requirements: InputRequirements::default(),
            output_capabilities: OutputCapabilities::default(),
            parameters: vec![],
            r#match: Match::default(),
            runtime: None,
            outputs: vec![],
        };
        let json = questionnaire(&model, "http://example.org/fhir");
        assert!(
            !json.contains("clinfer-output-result-content"),
            "no result-content extension when there is no output: {json}"
        );
    }
}
