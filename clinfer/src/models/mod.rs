// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The model layer: one YAML file per model, and the two-layer configuration.
//!
//! `clinfer.conf` carries the sidecar's own settings - the origin, the
//! engine and the directory the models are in. Everything that describes a
//! MODEL lives in its own file under that directory, so adding a model is
//! dropping a file and removing one is deleting it, with no edit to the
//! shared configuration and no risk of disturbing another model's block.
//!
//! A model file has two halves and the split is deliberate:
//!
//! - the TOP-LEVEL fields (`title`, `description`, `provider`,
//!   `procedure_code`, `input_requirements`, `output_capabilities`,
//!   `parameters`) are what an external consumer of the model's declared
//!   capabilities reads: the AI Service Properties work item (AISPro, IHE
//!   RAD) settled on FHIR Questionnaire as the encoding, and
//!   `procedure_code` / `parameters` are the UPS attributes (0040,4018) and
//!   (0074,1210) the same service declares;
//! - `match` and `runtime` are what THIS sidecar needs and no other tool
//!   has a use for: which study a model is for and how to run it. `outputs`
//!   is both: `series_description` is capability, served as the result-content
//!   extension, while `kind` and `profile_version` stay internal.
//!
//! Only capability is ever served, which is why the split is a rule and not
//! a convention.
//!
//! # Why a directory
//!
//! A file per model is the shape that survives editing by several people and
//! by several tools: a diff touches one model, a broken file names itself in
//! the error, and the files that ARE valid keep working. The alternative -
//! one `models:` mapping in the shared file - made every model's parameters
//! share one indentation budget and one merge conflict.
//!
//! # Deterministic order
//!
//! Files are read sorted by name. Directory enumeration order is not a
//! promise on any platform, and a startup report whose lines move between
//! runs is a report an operator cannot diff. Duplicate identifiers are
//! therefore reported at the SECOND file in that sort order, not at
//! whichever one the filesystem happened to hand over first.

pub mod fhir;
pub mod schema;
pub mod validate;

pub use fhir::Answer;
pub use schema::{
    Anatomy, Choice, Code, InputRequirements, Model, Output, OutputCapabilities, Parameter,
    ParameterType, Provider, Runtime,
};
pub use validate::{Diagnostics, Problem};

use crate::conf::{Match, ResultKind, TagBinding};
use crate::value::{ConfError, Result, Section};
use crate::yaml::{Yaml, parse_file};
use std::path::{Path, PathBuf};

/// The file extension a model file must carry.
pub const MODEL_EXTENSION: &str = "yaml";

/// Every model in a directory, in the order they were read.
///
/// `enabled: false` files are still parsed and validated - an example that
/// is not read rots - and are returned with [`Model::enabled`] false; the
/// caller registers only the enabled ones.
#[derive(Debug, Clone, PartialEq)]
pub struct Models {
    /// The models, sorted by file name, disabled ones included.
    pub models: Vec<Model>,
}

impl Models {
    /// The models as the configuration holds them.
    ///
    /// `Conf::models` is already filtered to the enabled files, so this is a
    /// borrow of the same list rather than a second reading of the model
    /// files: a request must never see a different set than the worker runs.
    #[must_use]
    pub fn from_conf(conf: &crate::conf::Conf) -> Self {
        Self {
            models: conf.models.clone(),
        }
    }

    /// The models that are switched on.
    pub fn enabled(&self) -> impl Iterator<Item = &Model> {
        self.models.iter().filter(|model| model.enabled)
    }

    /// How many models are switched on.
    #[must_use]
    pub fn enabled_len(&self) -> usize {
        self.models.iter().filter(|model| model.enabled).count()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

/// Load and parse every `.yaml` file in `dir`.
///
/// The parse itself does not decide whether the directory is USABLE: a model
/// with a broken script or a duplicated procedure code is a problem the
/// validation tiers report together with the others, so this function is
/// about reading, and [`validate::check`] is about accepting.
pub fn load_dir(dir: &Path) -> Result<Models> {
    let names = list_files(dir)?;
    let mut models = Vec::with_capacity(names.len());
    for name in names {
        let path = dir.join(&name);
        // The identifier is the FILE STEM, not a key inside the file: the
        // file name is what an operator sees in the directory listing, and a
        // second name for the same model would be one name too many.
        let id = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .ok_or_else(|| ConfError::new(name.clone(), "the file name has no stem"))?;
        let document = parse_file(&path)
            .map_err(|err| ConfError::new(file_label(&path), err.to_string()).in_file(&path))?;
        let model = parse(&document, &id, Some(dir)).map_err(|err| err.in_file(&path))?;
        models.push(model);
    }
    Ok(Models { models })
}

/// The `.yaml` file names in `dir`, sorted.
///
/// A missing directory is an empty one: a deployment that serves nothing is
/// a valid deployment, and refusing to start because "models/" is absent
/// would make the simplest working configuration impossible.
fn list_files(dir: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(ConfError::new(
                dir.display().to_string(),
                format!("cannot list the models directory: {err}"),
            ));
        }
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| {
            ConfError::new(
                dir.display().to_string(),
                format!("cannot read the models directory: {err}"),
            )
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // The comparison is on the extension, not on the whole name: a file
        // called `.yaml` is a dot file with no stem, and the stem check in
        // `load_dir` refuses it with its own message rather than this list
        // silently skipping it.
        let is_model = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case(MODEL_EXTENSION));
        if !is_model {
            continue;
        }
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Read one model from its parsed document.
///
/// `id` is the file stem; `base` is the model directory, the anchor for a
/// relative script. The readings are in the order a reader of the file meets
/// them, so the first error reported is the first error in the file rather
/// than the first one this function happens to check.
pub fn parse(document: &Yaml, id: &str, base: Option<&Path>) -> Result<Model> {
    let mut root = Section::wrap(document, "")?;

    let enabled = root.opt_bool("enabled")?.unwrap_or(true);
    let title = root.opt_str("title")?.map(str::to_string);
    let description = root.opt_str("description")?.map(str::to_string);
    let provider = match root.take("provider")? {
        Some(value) => Some(schema::parse_provider(value, "provider")?),
        None => None,
    };
    let procedure_code = match root.take("procedure_code")? {
        Some(value) => Some(schema::parse_code(value, "procedure_code")?),
        None => None,
    };
    let input_requirements = parse_input_requirements(&mut root)?;
    let output_capabilities = parse_output_capabilities(&mut root)?;
    let parameters = schema::parse_parameters(&mut root)?;
    let r#match = parse_match(&mut root)?;
    let runtime = parse_runtime(&mut root)?;
    let outputs = schema::parse_outputs(&mut root)?;
    root.finish("a model")?;

    let mut model = Model {
        id: id.to_string(),
        enabled,
        title,
        description,
        provider,
        procedure_code,
        input_requirements,
        output_capabilities,
        parameters,
        r#match,
        runtime,
        outputs,
    };
    // A relative script is anchored at the MODEL FILE's directory, not at
    // the process cwd and not at the workdir: the workdir is minted per run,
    // so nothing the site configures can live inside it, and anchoring at the
    // file is what makes a model directory copyable to another machine whole.
    if let Some(base) = base
        && let Some(runtime) = model.runtime.as_mut()
        && runtime.script.is_relative()
    {
        runtime.script = base.join(&runtime.script);
    }
    Ok(model)
}

/// The modality and body-region filter the model declares.
///
/// `anatomy` is DECLARED here and resolved by nothing yet: the coded
/// (0008,2218) / (0008,2228) reading it names is a later step, and the
/// free-text keys in `match` are what decide a study today. It is parsed now
/// because a code that nothing reads is still a claim an operator can check
/// against the model's own documentation.
fn parse_input_requirements(root: &mut Section<'_>) -> Result<InputRequirements> {
    let Some(value) = root.take("input_requirements")? else {
        return Ok(InputRequirements::default());
    };
    let mut section = Section::wrap(value, "input_requirements")?;
    let modalities = section
        .opt_str_list("modalities")?
        .into_iter()
        .map(str::to_ascii_uppercase)
        .collect();
    let photometric_interpretation = section
        .opt_str_list("photometric_interpretation")?
        .into_iter()
        .map(str::to_ascii_uppercase)
        .collect();
    let anatomy = match section.take("anatomy")? {
        Some(value) => parse_anatomy_list(value, "input_requirements.anatomy")?,
        None => Vec::new(),
    };
    section.finish("input_requirements")?;
    Ok(InputRequirements {
        modalities,
        photometric_interpretation,
        anatomy,
    })
}

/// One anatomy entry, coded so that a consumer can filter on the code rather
/// than on its display text.
fn parse_anatomy_list(value: &Yaml, at: &str) -> Result<Vec<Anatomy>> {
    let Some(items) = value.as_list() else {
        return Err(ConfError::type_error(at, "a list", value));
    };
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let item_at = format!("{at}[{index}]");
        let mut section = Section::wrap(item, &item_at)?;
        let system = section.require_str("system")?.trim().to_string();
        let code = section.require_str("code")?.trim().to_string();
        let meaning = section.opt_str("meaning")?.map(str::to_string);
        section.finish("an anatomy entry")?;
        // A code with no scheme is a number that names something in some
        // vocabulary or other; the scheme is what makes it resolvable, so an
        // empty one is refused where the values are known to be missing.
        if system.is_empty() {
            return Err(ConfError::new(
                format!("{item_at}.system"),
                "must not be empty; it is the code system a consumer resolves the code in",
            ));
        }
        if code.is_empty() {
            return Err(ConfError::new(
                format!("{item_at}.code"),
                "must not be empty; it is the value a consumer matches",
            ));
        }
        out.push(Anatomy {
            system,
            code,
            meaning,
        });
    }
    Ok(out)
}

fn parse_output_capabilities(root: &mut Section<'_>) -> Result<OutputCapabilities> {
    let Some(value) = root.take("output_capabilities")? else {
        return Ok(OutputCapabilities::default());
    };
    let mut section = Section::wrap(value, "output_capabilities")?;
    let sop_classes = section
        .opt_str_list("sop_classes")?
        .into_iter()
        .map(|uid| uid.trim().to_string())
        .collect();
    section.finish("output_capabilities")?;
    Ok(OutputCapabilities { sop_classes })
}

/// The study filter, read with the same reader the old shared `rules:` block
/// used, so an operator moving a rule into a model file moves the keys and
/// nothing else.
///
/// `code` is NOT one of the keys here, unlike the retired block: the code a
/// workitem carries is a property of the REQUEST, and a model file describes
/// the model. The procedure the model implements has its own home at the top
/// level (`procedure_code`), where a consumer can read it.
fn parse_match(root: &mut Section<'_>) -> Result<Match> {
    let Some(value) = root.take("match")? else {
        return Ok(Match::default());
    };
    let mut section = Section::wrap(value, "match")?;
    let modality = section
        .opt_str_list("modality")?
        .into_iter()
        .map(|value| value.trim().to_ascii_uppercase())
        .collect();
    let study_description_contains = section
        .opt_str_list("study_description_contains")?
        .into_iter()
        .map(str::to_string)
        .collect();
    let code_meaning_contains = section
        .opt_str_list("code_meaning_contains")?
        .into_iter()
        .map(str::to_string)
        .collect();
    let body_part_contains = section
        .opt_str_list("body_part_contains")?
        .into_iter()
        .map(str::to_string)
        .collect();
    let tags = match section.take("tag_bindings")? {
        Some(value) => parse_tag_bindings(value, "match.tag_bindings")?,
        None => Vec::new(),
    };
    section.finish("match")?;
    Ok(Match {
        modality,
        code: Vec::new(),
        study_description_contains,
        code_meaning_contains,
        body_part_contains,
        tags,
    })
}

fn parse_tag_bindings(value: &Yaml, at: &str) -> Result<Vec<TagBinding>> {
    let mut section = Section::wrap(value, at)?;
    let entries = section.entries();
    if entries.is_empty() {
        return Err(ConfError::new(at, "the mapping is empty"));
    }
    let mut out = Vec::with_capacity(entries.len());
    for (key, patterns) in entries {
        let tag = crate::conf::normalize_tag(key)
            .ok_or_else(|| ConfError::new(format!("{at}.{key}"), "expected 8 hex digits"))?;
        let item_at = format!("{at}.{key}");
        let patterns = crate::value::str_items(patterns, &item_at)?
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<String>>();
        if patterns.is_empty() {
            return Err(ConfError::new(&item_at, "the list is empty"));
        }
        section.claim(key);
        out.push(TagBinding { tag, patterns });
    }
    section.finish("a tag binding block")?;
    Ok(out)
}

/// How to run the model. A relative `script` is anchored at the FILE's
/// directory, so a model directory can be copied to another machine whole.
fn parse_runtime(root: &mut Section<'_>) -> Result<Option<Runtime>> {
    let Some(value) = root.take("runtime")? else {
        return Ok(None);
    };
    let mut section = Section::wrap(value, "runtime")?;
    let script = PathBuf::from(section.require_str("script")?);
    let interpreter = section.opt_str("interpreter")?.map(PathBuf::from);
    let timeout_secs = match section.opt_bounded_int("timeout_secs", 5, 1800)? {
        Some(value) => u32::try_from(value).map_err(|_| {
            ConfError::new(
                "runtime.timeout_secs",
                format!("{value} does not fit a second count"),
            )
        })?,
        None => crate::conf::DEFAULT_TIMEOUT_SECS,
    };
    let max_result_bytes = match section.opt_size("max_result_bytes")? {
        Some(0) => {
            return Err(ConfError::new(
                "runtime.max_result_bytes",
                "0 is not a budget: omit the key to take the default",
            ));
        }
        Some(value) if value > crate::conf::MAX_RESULT_BYTES_CEILING => {
            return Err(ConfError::new(
                "runtime.max_result_bytes",
                format!(
                    "{value} is over the {}-byte ceiling; a result this large is a runaway \
                     script, not a study",
                    crate::conf::MAX_RESULT_BYTES_CEILING
                ),
            ));
        }
        Some(value) => value,
        None => crate::conf::DEFAULT_MAX_RESULT_BYTES,
    };
    section.finish("runtime")?;
    Ok(Some(Runtime {
        script,
        interpreter,
        timeout_secs,
        max_result_bytes,
    }))
}

/// The SOP classes one result kind produces, per the result matrix.
///
/// The matrix is `analysis/1509_architect_clinfer_result_matrix_REPORT.md`;
/// the UIDs are PS3.6 SOP Class UIDs and are spelled here because the
/// mapping is what `output_capabilities.sop_classes` is checked against, and
/// a model that declares an output its encoder cannot produce is a
/// declaration that will not happen.
///
/// `sr` has THREE classes because the template decides which one the writer
/// emits; a model producing a text report names one of the three, and all
/// three are accepted because all three are SR documents this build writes.
pub const fn sop_classes(kind: ResultKind) -> &'static [&'static str] {
    match kind {
        ResultKind::Pr => &["1.2.840.10008.5.1.4.1.1.11.1"],
        ResultKind::Seg => &["1.2.840.10008.5.1.4.1.1.66.4"],
        ResultKind::Stl => &["1.2.840.10008.5.1.4.1.1.104.3"],
        ResultKind::Sr | ResultKind::Measurements => &[
            "1.2.840.10008.5.1.4.1.1.88.11",
            "1.2.840.10008.5.1.4.1.1.88.22",
            "1.2.840.10008.5.1.4.1.1.88.33",
        ],
        ResultKind::Rtstruct => &["1.2.840.10008.5.1.4.1.1.481.3"],
        // A derived image is a new instance of the SOURCE modality's class,
        // so no class can be named before a study is in hand. The empty
        // slice is the honest answer, and the validation treats every
        // declared class as unmatchable rather than inventing one.
        ResultKind::Derived => &["1.2.840.10008.5.1.4.1.1.7"],
    }
}

/// Whether `uid` is a SOP class the given kind produces.
#[must_use]
pub fn kind_produces(kind: ResultKind, uid: &str) -> bool {
    sop_classes(kind).contains(&uid)
}

/// The encoder kind a model file's `outputs[].kind` names, if this build has
/// one.
#[must_use]
pub fn known_output_kind(kind: &str) -> Option<ResultKind> {
    ResultKind::parse(kind)
}
