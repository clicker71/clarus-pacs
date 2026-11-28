// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The typed configuration.
//!
//! Everything the sidecar needs to know before it talks to the network is
//! decided here, at startup, from `clinfer.conf`. One decision is
//! deliberately a refusal rather than a warning, because it would
//! otherwise end as a wrong DICOM object in the archive:
//!
//! - a result kind whose encoder is not linked into this build is a
//!   startup error, never a silently skipped result.
//!
//! This file is the BASE layer: the origin, the engine and the models
//! DIRECTORY. What a model is lives in one file per model under that
//! directory (see [`crate::models`]), which is what makes adding a model a
//! matter of dropping a file rather than editing a shared one.
//!
//! Every model is an EXTERNAL PROCESSOR (see `abi/README.md`): clinfer
//! carries no inference engine of its own - the site's script decides
//! what a study produces, and the model's `outputs` carry the metadata of
//! the object clinfer writes.

/// The error type of every configuration operation.
pub use crate::value::{ConfError, Result};

use crate::value::Section;
use crate::yaml::{Yaml, parse_file};
use std::fmt;
use std::path::{Path, PathBuf};

/// Name of the configuration file looked for in the working directory.
pub const DEFAULT_CONFIG_FILE: &str = "clinfer.conf";

/// The models directory a configuration that names none gets, relative to
/// the configuration file.
pub const DEFAULT_MODELS_DIR: &str = "models";

/// Default address of the sidecar's own UPS-RS receiver.
const DEFAULT_LISTEN: &str = "127.0.0.1:8124";

/// Default station AE Title: the performer's own identity in the workitem
/// attributes that name it - (0040,0001) as a query key, (0040,4025) and
/// (0040,4036) as assignment guards, (0040,4028) as the performed station.
///
/// The standard's own default AE title (PS3.5 Annex A / PS3.4 CC.2.5-3 has
/// no default; `CLINFER` is the product name, so a site that does nothing
/// still gets a value that is distinct from every other actor's).
const DEFAULT_STATION_AET: &str = "CLINFER";

/// How often the catch-up search runs while the notification channel is
/// DOWN, in seconds.
///
/// 2 s matches the Weasis precedent (`--poll 2`) and is short enough that a
/// performer on a channel-less origin picks work up at human scale while
/// costing one worklist query per interval. `0` DISABLES the fallback, which
/// is valid here (unlike `engine.workers`): an operator on an origin that
/// does deliver Event Reports may not want the extra traffic, and a knob
/// whose off switch is missing is a knob operators route around.
const DEFAULT_POLL_INTERVAL_SECS: u32 = 2;

/// Upper bound for `server.poll_interval_secs`.
///
/// One hour: past that the "fallback" no longer looks like a fallback - a
/// performer polling slower than an hour is effectively waiting for a
/// restart, which is the behaviour this knob exists to remove. Refusing is
/// better than silently clamping, because an operator who wrote 86400
/// believes they configured something.
const MAX_POLL_INTERVAL_SECS: i64 = 3600;

/// What a rule's result encoder produces (`result.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultKind {
    /// Grayscale softcopy presentation state: box overlays.
    Pr,
    /// Segmentation object: pixel masks.
    Seg,
    /// Encapsulated STL: a surface.
    Stl,
    /// Structured report: machine-readable measurements.
    Sr,
    /// Enhanced SR: numeric findings (TID 1500).
    Measurements,
    /// A derived image: a new DICOM image of the source modality.
    Derived,
    /// RT Structure Set: ROI contours as patient-space polygons.
    Rtstruct,
}

impl ResultKind {
    /// Every kind, in the order the enum declares them.
    ///
    /// Public because a model file names a kind and the model layer must
    /// answer "is this a kind this build writes" without a second copy of
    /// the list.
    pub const ALL: [ResultKind; 7] = [
        Self::Pr,
        Self::Seg,
        Self::Stl,
        Self::Sr,
        Self::Measurements,
        Self::Derived,
        Self::Rtstruct,
    ];

    /// The kind a name denotes, or `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pr => "pr",
            Self::Seg => "seg",
            Self::Stl => "stl",
            Self::Sr => "sr",
            Self::Measurements => "measurements",
            Self::Derived => "derived",
            Self::Rtstruct => "rtstruct",
        }
    }

    /// Whether this build can encode the kind.
    ///
    /// The check is a match over the enum rather than a flag, so adding a
    /// kind to the enum without adding an encoder fails to compile at this
    /// exact spot. Every kind has an encoder: `pr`, `sr`, `measurements`,
    /// `derived`, `seg`, `stl` and `rtstruct` are all written by this crate,
    /// so a configuration that asks for one of them is a configuration this
    /// build can serve.
    #[must_use]
    pub fn encoder_built(self) -> bool {
        match self {
            Self::Pr
            | Self::Sr
            | Self::Measurements
            | Self::Seg
            | Self::Stl
            | Self::Derived
            | Self::Rtstruct => true,
        }
    }
}

impl fmt::Display for ResultKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the sidecar does with a workitem no rule matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultRule {
    /// Claim, then cancel: the study is accounted for instead of silent.
    Cancel,
    /// Never claim it: the workitem is left SCHEDULED and available to
    /// whoever does want it. Reading happens either way, because the
    /// decision is what tells this sidecar whether the workitem is its
    /// business at all.
    Off,
}

impl DefaultRule {
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "cancel" => Some(Self::Cancel),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cancel => "cancel",
            Self::Off => "off",
        }
    }
}

// How a source frame is fitted to a processor's input is the processor's
// own business: a script declares its geometry in its own code. This
// section therefore holds no resize or tensor declaration at all.

/// A binding from one DICOM tag to value patterns.
#[derive(Debug, Clone, PartialEq)]
pub struct TagBinding {
    /// Upper-case hexadecimal `GGGGEEEE`.
    pub tag: String,
    /// Patterns matched case-insensitively with `*` wildcards.
    pub patterns: Vec<String>,
}

/// Every signal a study is matched on. An empty field constrains nothing, so
/// the default is "every study", which is what a model that only states its
/// anatomy means.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Match {
    /// (0008,0061) values the model accepts, upper-cased.
    ///
    /// A model that names none accepts every modality - the reading an
    /// example file wants when it is about the OUTPUT rather than the input.
    pub modality: Vec<String>,
    /// (0040,4018) Scheduled Workitem Code Sequence Code Values, upper-cased.
    ///
    /// The PRIMARY key of a match, and the reason it is first: the workitem
    /// carries its own intent, so a match can be decided WITHOUT fetching
    /// the study at all.
    ///
    /// A model file does not set this key directly: it declares the procedure
    /// it performs at the top level (`procedure_code`), and the derived rule
    /// copies that Code Value here, so the workitem's code selects the model.
    /// A model that declares no procedure code leaves the list empty, which
    /// constrains nothing - the pre-key behaviour.
    pub code: Vec<String>,
    pub study_description_contains: Vec<String>,
    pub code_meaning_contains: Vec<String>,
    pub body_part_contains: Vec<String>,
    pub tags: Vec<TagBinding>,
}

/// Where the label's leader line starts on the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LabelAnchor {
    /// The box's top-left corner.
    #[default]
    TopLeft,
    /// The box's center.
    Center,
}

impl LabelAnchor {
    const ALL: [LabelAnchor; 2] = [Self::TopLeft, Self::Center];

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|anchor| anchor.as_str() == text)
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TopLeft => "tl",
            Self::Center => "center",
        }
    }
}

/// The PR label geometry: where the label box sits relative to the
/// detection box, and where the leader line starts.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelProfile {
    pub anchor: LabelAnchor,
    /// The label box center's Y delta from the anchor, in pixels.
    /// Negative puts the label above the box, clear of it.
    pub offset_y_px: i32,
    /// The label box left edge's X delta from the anchor, in pixels:
    /// negative puts the label left of the corner, so the leader runs
    /// down and to the right at a diagonal.
    pub offset_x_left_px: i32,
    /// The label box right edge's X delta from the anchor, in pixels.
    pub offset_x_right_px: i32,
}

impl Default for LabelProfile {
    fn default() -> Self {
        Self {
            anchor: LabelAnchor::default(),
            offset_y_px: -110,
            offset_x_left_px: -200,
            offset_x_right_px: 80,
        }
    }
}

/// The content template a report claims to follow (`result.sr.template_id`).
///
/// The enum is not a bare string because the template decides what the
/// content items may be, and the writer has to emit the matching template
/// number and designator: a free-form value would let an administrator name
/// a template the serializer does not implement.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SrResult {
    /// (0008,103E) of the report series, and the human title of the
    /// document.
    pub title: Option<String>,
    /// The content template the document claims to follow.
    pub template_id: SrTemplate,
}

/// The content template a structured report declares.
///
/// Two values, one per writer: TID 2000 (Basic Text SR) describes a document
/// whose whole content is one text block, and TID 1500 (Measurement Report)
/// describes a document of numeric findings, which is the only conformant
/// home for a NUM. A third template would be a third writer, not a third
/// key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SrTemplate {
    /// TID 2000, "Basic Text SR"; the `DCMR` designator.
    #[default]
    Tid2000,
    /// TID 1500, "Measurement Report"; the `DCMR` designator. The template
    /// an Enhanced SR (`.88.22`) declares, because a numeric finding has no
    /// legal home in a Basic Text SR.
    Tid1500,
}

impl SrTemplate {
    const ALL: [SrTemplate; 2] = [Self::Tid2000, Self::Tid1500];

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|template| template.as_str() == text)
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tid2000 => "TID2000",
            Self::Tid1500 => "TID1500",
        }
    }

    /// The coding scheme designator of the template id in
    /// (0040,A504) ContentTemplateSequence.
    #[must_use]
    pub fn designator(self) -> &'static str {
        match self {
            Self::Tid2000 => "DCMR",
            Self::Tid1500 => "DCMR",
        }
    }

    /// The template number as the attribute's own text.
    #[must_use]
    pub fn template_number(self) -> &'static str {
        match self {
            Self::Tid2000 => "2000",
            Self::Tid1500 => "1500",
        }
    }
}

impl fmt::Display for SrTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerConf {
    pub base_url: String,
    pub identity_user: String,
    pub listen: String,
    pub trusted_proxies: Vec<String>,
    pub require_proxy_identity: bool,
    /// (0040,0001) Scheduled Station AE Title of THIS performer, and the
    /// Code Value this performer compares against in the assignment
    /// attributes (0040,4025, (0040,4036), (0040,4026).
    ///
    /// One deployment-wide value, not an instance identity: two sidecars
    /// on the same site that serve the same station carry the same title,
    /// because the origin's query key and the origin's assignment name the
    /// STATION, not the process.
    pub station_aet: String,
    /// Human-readable station name, the Code Meaning of (0040,4028)
    /// Performed Station Name Code Sequence.
    ///
    /// Defaults to `station_aet`: the code's meaning is what an operator
    /// reads in the archive, and an AE title is a worse label than a site
    /// name but a truthful one.
    pub station_name: String,
    /// Seconds between catch-up searches WHILE THE NOTIFICATION CHANNEL IS
    /// DOWN; `0` disables the fallback.
    ///
    /// The fallback exists because the Err arm of the notifier loop used to
    /// do nothing but sleep: on an origin that never accepts the channel, a
    /// workitem created after startup stayed invisible until a restart. It
    /// does NOT replace the channel - a successful connect still uses the
    /// push path, and the poll only runs while there is no channel to use.
    pub poll_interval_secs: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EngineConf {
    /// Where work directories and the result cache live.
    pub cache_dir: PathBuf,
    /// How many model processes may run at the same time.
    ///
    /// `None` means "as many as there are models", which is the default and
    /// the only value that cannot be wrong for a site that did not think
    /// about the question. The upper bound is NOT known when this is parsed
    /// (the models are read after the engine), so the cap lives in
    /// [`Conf::workers`] - one rule, one place.
    pub workers: Option<usize>,
}

/// The base URL under which the extension StructureDefinitions are published.
///
/// The public repository is the default. A site that hosts the definitions
/// itself - its own site, a national mirror, or an intranet server - points
/// this at that base; every extension URL is `{base}/{file}.json`. The admin
/// never has to touch code to move the schemas.
pub const DEFAULT_EXTENSION_BASE: &str =
    "https://github.com/clicker71/clarus-pacs/blob/main/clinfer/fhir/StructureDefinition";

/// The FHIR-facing settings: where the capability document's extension
/// definitions are published. Everything else the document serves is derived
/// from the models.
#[derive(Debug, Clone, PartialEq)]
pub struct FhirConf {
    /// Base URL of the extension StructureDefinitions; see
    /// [`DEFAULT_EXTENSION_BASE`].
    pub extension_base: String,
}

/// The base configuration: everything that is not a model.
///
/// `models` is the registry the `models_dir` produced, already filtered to
/// the enabled files, and `rules` is derived from it: a model IS its rules,
/// so the two can never disagree about which model a study runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Conf {
    pub server: ServerConf,
    pub engine: EngineConf,
    pub fhir: FhirConf,
    /// Where the model files live, as the configuration spells it.
    pub models_dir: PathBuf,
    pub models: Vec<crate::models::Model>,
    /// One entry per (model, output), in model-file order.
    pub rules: Vec<Rule>,
    pub default_rule: DefaultRule,
}

/// One model's declaration of one object it produces.
///
/// Derived rather than configured: the model file states the outputs, and
/// this is the worker's view of one of them. The index is its position in
/// the derived list, which is what a report or a log names.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    /// Position in [`Conf::rules`], used in messages.
    pub index: usize,
    /// The id of the model that produces it.
    pub model: String,
    pub result: RuleResult,
    /// The study filter, taken from the model.
    pub r#match: Match,
}

/// The metadata of one produced object.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleResult {
    pub kind: ResultKind,
    pub series_description: Option<String>,
    pub profile_version: u32,
    /// The label geometry of a `pr` result.
    ///
    /// It is the encoder's default rather than a model field: a label
    /// offset is a rendering decision of THIS writer, not a property of the
    /// model, and a site that needs another offset says so in the writer.
    pub label: LabelProfile,
}

impl Conf {
    /// The enabled model with this id.
    #[must_use]
    pub fn model(&self, id: &str) -> Option<&crate::models::Model> {
        self.models
            .iter()
            .find(|model| model.enabled && model.id == id)
    }

    /// How many model processes may run at the same time.
    ///
    /// The ONE place the ceiling is computed, so startup, reload and the
    /// startup report cannot disagree about it:
    ///
    /// - absent -> every enabled model, which is exactly the behaviour of a
    ///   sidecar with one model and the natural reading of a site that
    ///   dropped in several;
    /// - configured larger -> capped to the number of models rather than
    ///   refused, because a ceiling above the number of threads that exist
    ///   is not an error, it is a redundant setting;
    /// - `0` never reaches here: the parser refuses it by name.
    #[must_use]
    pub fn workers(&self) -> usize {
        let models = self.models.iter().filter(|model| model.enabled).count();
        self.engine
            .workers
            .map_or(models, |workers| workers.min(models))
    }
}

/// Load and validate a configuration file and its models directory.
///
/// The models directory is resolved against the CONFIGURATION FILE's
/// directory, so a deployment is a directory that can be moved whole.
pub fn load(path: &Path) -> Result<Conf> {
    let document = parse_file(path).map_err(|err| {
        // The YAML layer knows the line; the caller knows the file.
        ConfError::new(file_label(path), err.to_string()).in_file(path)
    })?;
    // The directory the file lives in is the anchor for every RELATIVE
    // path in the document, so a packaged configuration stays correct
    // no matter which directory the sidecar was started from. The file
    // itself is made ABSOLUTE first: `--config pack.conf` has no
    // directory component, and anchoring at the empty parent would
    // leave relative paths unresolved.
    let absolute = absolutize(path);
    let base = absolute.parent().map(Path::to_path_buf);
    let mut conf = build(&document, base.as_deref()).map_err(|err| err.in_file(path))?;
    load_models(&mut conf)?;
    validate(&conf)?;
    Ok(conf)
}

/// Read the models directory into `conf`.
///
/// Every file is checked (a disabled example included); an enabled model is
/// additionally checked for a script that exists, and an ENABLED model is
/// the only thing registered - so one broken example cannot stop a site
/// serving the models it did install, while a broken enabled model stops the
/// start rather than being silently absent.
fn load_models(conf: &mut Conf) -> Result<()> {
    let dir = &conf.models_dir;
    let loaded = crate::models::load_dir(dir)?;
    let diagnostics = crate::models::validate::check(dir, &loaded);
    if !diagnostics.is_empty() {
        return Err(ConfError::new(
            dir.display().to_string(),
            format!("\n{}", diagnostics.report()),
        ));
    }
    let enabled: Vec<crate::models::Model> = loaded.enabled().cloned().collect();
    conf.rules = derive_rules(&enabled);
    conf.models = enabled;
    Ok(())
}

/// One rule per (model, output).
///
/// The chain semantics are unchanged: every rule that matches runs, in
/// model-file order, and each delivers its own object. A model declaring
/// several outputs therefore delivers several objects from ONE run, which is
/// what a detector producing a presentation state AND a report needs.
fn derive_rules(models: &[crate::models::Model]) -> Vec<Rule> {
    let mut rules = Vec::new();
    for model in models {
        for output in &model.outputs {
            let Some(kind) = ResultKind::parse(&output.kind) else {
                continue;
            };
            // The procedure code IS the workitem-code key: the value the
            // requester put in (0040,4018) is what selects the model, so the
            // derived rule carries it in its match. A model that declares no
            // procedure code matches every code, which is the pre-key reading.
            let mut r#match = model.r#match.clone();
            if let Some(code) = &model.procedure_code {
                r#match.code = vec![code.value.trim().to_ascii_uppercase()];
            }
            rules.push(Rule {
                index: rules.len(),
                model: model.id.clone(),
                r#match,
                result: RuleResult {
                    kind,
                    series_description: output.series_description.clone(),
                    profile_version: output.profile_version,
                    label: LabelProfile::default(),
                },
            });
        }
    }
    rules
}

/// The same path, guaranteed absolute.
///
/// `Path::parent` of a bare file name is the EMPTY path, so joining it
/// with a relative script changes nothing; the file must be anchored at
/// the current directory first. An absolute path is returned untouched.
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        // No current directory is a broken process, not a decision point:
        // return the path as spelled and let the spawn report it.
        Err(_) => path.to_path_buf(),
    }
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn build(document: &Yaml, base: Option<&Path>) -> Result<Conf> {
    let mut root = Section::wrap(document, "")?;

    let server = parse_server(&mut root)?;
    let engine = parse_engine(&mut root)?;
    let fhir = parse_fhir(&mut root)?;
    let models_dir = match root.opt_str("models_dir")? {
        Some(text) => {
            let path = PathBuf::from(text);
            match (base, path.is_relative()) {
                (Some(dir), true) => dir.join(path),
                _ => path,
            }
        }
        // The shipped default: a deployment is a directory holding
        // `clinfer.conf` and `models/`, and a config that says nothing gets
        // that directory beside the file it lives in.
        None => match base {
            Some(dir) => dir.join(DEFAULT_MODELS_DIR),
            None => PathBuf::from(DEFAULT_MODELS_DIR),
        },
    };
    let default_rule = match root.opt_str("default_rule")? {
        Some(text) => DefaultRule::parse(text).ok_or_else(|| {
            ConfError::new(
                "default_rule",
                format!("unknown value '{text}'; expected cancel or off"),
            )
        })?,
        None => DefaultRule::Cancel,
    };
    root.finish("the file")?;

    Ok(Conf {
        server,
        engine,
        fhir,
        models_dir,
        models: Vec::new(),
        rules: Vec::new(),
        default_rule,
    })
}

fn parse_server(root: &mut Section<'_>) -> Result<ServerConf> {
    let mut section = match root.take("server")? {
        Some(value) => Section::wrap(value, "server")?,
        None => return Err(ConfError::new("server", "missing required section")),
    };
    let conf = ServerConf {
        base_url: section.require_str("base_url")?.to_string(),
        identity_user: section.require_str("identity_user")?.to_string(),
        listen: section
            .opt_str("listen")?
            .unwrap_or(DEFAULT_LISTEN)
            .to_string(),
        trusted_proxies: section
            .opt_str_list("trusted_proxies")?
            .into_iter()
            .map(str::to_string)
            .collect(),
        require_proxy_identity: section.opt_bool("require_proxy_identity")?.unwrap_or(false),
        station_aet: section
            .opt_str("station_aet")?
            .unwrap_or(DEFAULT_STATION_AET)
            .to_string(),
        station_name: section
            .opt_str("station_name")?
            .unwrap_or_default()
            .to_string(),
        // `0` means OFF, and the range check is a separate arm so the
        // message can say which of the two bounds was violated - an
        // operator who wrote -5 and one who wrote 86400 have different
        // problems.
        poll_interval_secs: match section.opt_int("poll_interval_secs")? {
            Some(value) if value < 0 => {
                return Err(ConfError::new(
                    "server.poll_interval_secs",
                    format!("{value} is not an interval; seconds start at 0 (0 disables polling)"),
                ));
            }
            Some(value) if value > MAX_POLL_INTERVAL_SECS => {
                return Err(ConfError::new(
                    "server.poll_interval_secs",
                    format!(
                        "{value} is longer than the {MAX_POLL_INTERVAL_SECS}s maximum; a fallback poll slower than that is not a fallback"
                    ),
                ));
            }
            Some(value) => value as u32,
            None => DEFAULT_POLL_INTERVAL_SECS,
        },
    };
    // An absent name means "the AE Title is the label", which is the
    // honest default: a site that names its station at all will name it
    // the same way in every deployment of that station.
    let conf = ServerConf {
        station_name: if conf.station_name.is_empty() {
            conf.station_aet.clone()
        } else {
            conf.station_name
        },
        ..conf
    };
    section.finish("server")?;
    Ok(conf)
}

fn parse_engine(root: &mut Section<'_>) -> Result<EngineConf> {
    let mut section = match root.take("engine")? {
        Some(value) => Section::wrap(value, "engine")?,
        None => return Err(ConfError::new("engine", "missing required section")),
    };
    // Zero is refused rather than treated as "no limit", and the message
    // says what the key is FOR: an operator who wrote 0 meant to pause, a
    // state this key cannot express - a ceiling of no children is a sidecar
    // that claims work and never runs it, which is worse than a refusal at
    // startup.
    let workers = match section.opt_int("workers")? {
        Some(0) => {
            return Err(ConfError::new(
                "engine.workers",
                "0 is not a ceiling; it is the ceiling of concurrently running model \
                 processes and must be at least 1",
            ));
        }
        Some(value) if value < 0 => {
            return Err(ConfError::new(
                "engine.workers",
                format!("{value} is not a ceiling; counts start at 1"),
            ));
        }
        Some(value) => Some(value as usize),
        None => None,
    };
    let conf = EngineConf {
        cache_dir: PathBuf::from(section.require_str("cache_dir")?),
        workers,
    };
    section.finish("engine")?;
    Ok(conf)
}

fn parse_fhir(root: &mut Section<'_>) -> Result<FhirConf> {
    let mut section = match root.take("fhir")? {
        Some(value) => Section::wrap(value, "fhir")?,
        None => {
            return Ok(FhirConf {
                extension_base: DEFAULT_EXTENSION_BASE.to_string(),
            });
        }
    };
    let conf = FhirConf {
        extension_base: section
            .opt_str("extension_base")?
            .unwrap_or(DEFAULT_EXTENSION_BASE)
            .to_string(),
    };
    section.finish("fhir")?;
    Ok(conf)
}

/// `00211109`, `0021,1109` and `0021, 1109` all name the same tag; the/// canonical form is eight upper-case hex digits.
#[must_use]
pub fn normalize_tag(text: &str) -> Option<String> {
    let cleaned: String = text
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ',')
        .collect();
    if cleaned.len() != 8 || !cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(cleaned.to_ascii_uppercase())
}

/// Split `host:port`. The host may be an IPv6 literal in brackets.
#[must_use]
pub fn parse_listen(text: &str) -> Option<(String, u16)> {
    let (host, port) = if let Some(rest) = text.strip_prefix('[') {
        let close = rest.find(']')?;
        (rest[..close].to_string(), rest[close..].strip_prefix("]:")?)
    } else {
        let colon = text.rfind(':')?;
        (text[..colon].to_string(), &text[colon + 1..])
    };
    if host.is_empty() {
        return None;
    }
    let port: u16 = port.parse().ok()?;
    if port == 0 {
        return None;
    }
    Some((host, port))
}

// -- Cross-section validation ------------------------------------------

/// Default budget of one processor's `result.json`, in bytes (64 MiB).
///
/// A STRUCTURE SET grows with the traced polygon count: every contour
/// carries three coordinates per point, so the document is proportional to
/// the pixels a model outlines. Measured on live CT with the producer still
/// emitting every traced pixel:
///
/// ```text
///   36 frames / 528 contours   16628332 bytes   15.9 MiB
///   53 frames / 521 contours   28106522 bytes   26.8 MiB
/// ```
///
/// That is roughly 0.5 MiB per frame, so 64 MiB covers a study well over a
/// hundred frames larger than the ones measured, while still stopping a
/// script that loops. The limit is not a security boundary - the script is
/// operator-configured and shares the configuration's trust domain - it is
/// the difference between a bounded failure and a disk filled before the
/// timeout notices.
///
/// It is a DEFAULT, not a ceiling: a volumetric study needs more, and
/// `runtime.max_result_bytes` on the model is where that is said.
pub const DEFAULT_MAX_RESULT_BYTES: u64 = 64 * 1024 * 1024;

/// The largest per-model budget this parser accepts, in bytes (1 GiB).
///
/// The bound exists so a mistyped `max_result_bytes = 64g` is refused at
/// startup rather than after a run reads a gigabyte into memory. A gigabyte
/// is already far past any structure set a study can carry - the encoded
/// object has its own `ContourData` ceiling that no result size can lift -
/// so the number that gets a site working is well inside it, and the values
/// beyond it are typos rather than deployments.
pub const MAX_RESULT_BYTES_CEILING: u64 = 1024 * 1024 * 1024;

/// Default budget of one processor run, in seconds.
pub const DEFAULT_TIMEOUT_SECS: u32 = 300;

/// The interpreter an external processor runs under when none is given.
///
/// A bare name, not a path: it is resolved through `PATH` by the process
/// spawner, which is what lets a site rely on the platform's own python.
pub const DEFAULT_INTERPRETER: &str = "python3";

fn validate(conf: &Conf) -> Result<()> {
    // A rule is derived from a model, so the only way one can name a model
    // that does not exist or a kind without an encoder is a bug in the
    // derivation - and a bug there would store an object the build cannot
    // write, which is the one outcome worth refusing at startup.
    for rule in &conf.rules {
        let at = format!("rules[{}]", rule.index);
        if conf.model(&rule.model).is_none() {
            return Err(ConfError::new(
                format!("{at}.model"),
                format!("unknown model '{}'", rule.model),
            ));
        }
        if !rule.result.kind.encoder_built() {
            return Err(ConfError::new(
                format!("{at}.result.kind"),
                format!(
                    "'{}' encoder not built; this build encodes every declared kind, so this \
                     is a bug rather than a configuration error",
                    rule.result.kind
                ),
            ));
        }
    }

    if !conf.server.base_url.starts_with("http://") && !conf.server.base_url.starts_with("https://")
    {
        return Err(ConfError::new(
            "server.base_url",
            "expected an http:// or https:// URL",
        ));
    }
    if parse_listen(&conf.server.listen).is_none() {
        return Err(ConfError::new(
            "server.listen",
            format!("'{}' is not host:port", conf.server.listen),
        ));
    }
    if conf.server.require_proxy_identity && conf.server.trusted_proxies.is_empty() {
        return Err(ConfError::new(
            "server.trusted_proxies",
            "required when require_proxy_identity is true",
        ));
    }
    if conf.server.identity_user.trim().is_empty() {
        return Err(ConfError::new(
            "server.identity_user",
            "must not be empty; it becomes the X-Authenticated-User value",
        ));
    }
    // The station AE Title becomes a Code Value in (0040,4028) and a
    // comparison operand against the origin's (0040,4025). An empty one
    // would make every "is this workitem mine?" comparison compare two
    // absences, which reads as "assigned to me" - the one direction that
    // must never happen by accident.
    if conf.server.station_aet.trim().is_empty() {
        return Err(ConfError::new(
            "server.station_aet",
            "must not be empty; it is the (0040,0001) query key and the \
             (0040,4025) assignment comparison value",
        ));
    }
    if conf.server.station_name.trim().is_empty() {
        return Err(ConfError::new(
            "server.station_name",
            "must not be empty; it is the Code Meaning of (0040,4028)",
        ));
    }

    Ok(())
}

/// Whether an assignment attribute's Code Value names THIS station.
///
/// The comparison is the standard's own: (0040,4025) Scheduled Station
/// Name Code Sequence Code Value is the AE Title of the station a workitem
/// is assigned to (PS3.3 C.30.1 / AIW-I 4.80.4.1.2.1 maps the two), and AE
/// titles are case-insensitive by construction (PS3.5 Annex A). Leading
/// and trailing whitespace is insignificant in a code value, so it is
/// trimmed rather than compared byte for byte.
///
/// An empty or absent value is NOT a match, and the caller must treat it
/// as "not assigned to anyone" rather than as "assigned to us": the guard
/// exists to keep a performer off work that names a different station, and
/// a missing value names nobody.
#[must_use]
pub fn station_matches(code_value: &str, station_aet: &str) -> bool {
    let code_value = code_value.trim();
    !code_value.is_empty() && code_value.eq_ignore_ascii_case(station_aet.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_result_kind_round_trips_through_its_name() {
        for kind in ResultKind::ALL {
            assert_eq!(ResultKind::parse(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn the_label_anchor_names_round_trip() {
        for anchor in LabelAnchor::ALL {
            assert_eq!(LabelAnchor::parse(anchor.as_str()), Some(anchor));
        }
        for text in ["", "corner", "top_left", "TL", "centre", "0"] {
            assert_eq!(LabelAnchor::parse(text), None, "{text} should not parse");
        }
    }

    #[test]
    fn the_label_profile_defaults_to_the_corner_with_the_label_above() {
        let profile = LabelProfile::default();
        assert_eq!(profile.anchor, LabelAnchor::TopLeft);
        assert_eq!(profile.offset_y_px, -110);
        assert_eq!(profile.offset_x_left_px, -200);
        assert_eq!(profile.offset_x_right_px, 80);
    }

    #[test]
    fn the_sr_template_names_round_trip() {
        for template in SrTemplate::ALL {
            assert_eq!(SrTemplate::parse(template.as_str()), Some(template));
        }
        assert_eq!(SrTemplate::parse("tid2000"), None, "the casing is fixed");
        assert_eq!(SrTemplate::parse("1419"), None);
        assert_eq!(SrTemplate::default(), SrTemplate::Tid2000);
        assert_eq!(SrTemplate::Tid2000.designator(), "DCMR");
        assert_eq!(SrTemplate::Tid2000.template_number(), "2000");
        assert_eq!(SrTemplate::Tid1500.as_str(), "TID1500");
        assert_eq!(SrTemplate::Tid1500.designator(), "DCMR");
        assert_eq!(SrTemplate::Tid1500.template_number(), "1500");
        assert_eq!(SrTemplate::parse("tid1500"), None, "the casing is fixed");
    }

    #[test]
    fn tag_normalization_accepts_separators() {
        assert_eq!(normalize_tag("00211109").as_deref(), Some("00211109"));
        assert_eq!(normalize_tag("0021,1109").as_deref(), Some("00211109"));
        assert_eq!(normalize_tag("0021,ab09").as_deref(), Some("0021AB09"));
        assert_eq!(normalize_tag("0021110"), None);
        assert_eq!(normalize_tag("0021110g"), None);
        assert_eq!(normalize_tag(""), None);
    }

    #[test]
    fn listen_accepts_ipv4_and_bracketed_ipv6() {
        assert_eq!(
            parse_listen("127.0.0.1:8124"),
            Some(("127.0.0.1".into(), 8124))
        );
        assert_eq!(parse_listen("[::1]:8124"), Some(("::1".into(), 8124)));
        assert_eq!(parse_listen("127.0.0.1"), None);
        assert_eq!(parse_listen("127.0.0.1:0"), None);
        assert_eq!(parse_listen("127.0.0.1:notaport"), None);
        assert_eq!(parse_listen(":8124"), None);
    }

    #[test]
    fn a_models_procedure_code_becomes_the_rules_workitem_code_key() {
        let clahe = crate::models::Model {
            id: "clahe".to_string(),
            enabled: true,
            title: None,
            description: None,
            provider: None,
            procedure_code: Some(crate::models::Code {
                value: "IMG-CLAHE".to_string(),
                scheme: "99CLARUS".to_string(),
                meaning: Some("Adaptive Histogram Equalization (CLAHE)".to_string()),
            }),
            input_requirements: crate::models::InputRequirements::default(),
            output_capabilities: crate::models::OutputCapabilities::default(),
            parameters: Vec::new(),
            r#match: Match::default(),
            runtime: None,
            outputs: vec![crate::models::Output {
                kind: "derived".to_string(),
                series_description: Some("CLAHE".to_string()),
                profile_version: 1,
            }],
        };
        let rules = derive_rules(std::slice::from_ref(&clahe));
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].r#match.code, vec!["IMG-CLAHE".to_string()]);

        // A model that declares no procedure code matches every code: the
        // pre-key behaviour, not a rule that can never match.
        let bare = crate::models::Model {
            id: "bare".to_string(),
            procedure_code: None,
            ..clahe
        };
        let rules = derive_rules(&[bare]);
        assert_eq!(rules.len(), 1);
        assert!(rules[0].r#match.code.is_empty());
    }
}
