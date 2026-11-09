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
//! Every model is an EXTERNAL PROCESSOR (see `abi/README.md`): clinfer
//! carries no inference engine of its own - the site's script decides
//! what a study produces, and the rule's `result` carries the metadata
//! of the object clinfer writes.

/// The error type of every configuration operation.
pub use crate::value::{ConfError, Result};

use crate::value::{Section, str_items};
use crate::yaml::{Yaml, parse_file};
use std::fmt;
use std::path::{Path, PathBuf};

/// Name of the configuration file looked for in the working directory.
pub const DEFAULT_CONFIG_FILE: &str = "clinfer.conf";

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
    const ALL: [ResultKind; 7] = [
        Self::Pr,
        Self::Seg,
        Self::Stl,
        Self::Sr,
        Self::Measurements,
        Self::Derived,
        Self::Rtstruct,
    ];

    fn parse(text: &str) -> Option<Self> {
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
    fn parse(text: &str) -> Option<Self> {
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

/// How a source frame is fitted to a processor's input is the processor's
/// own business: a script declares its geometry in its own code. This
/// section therefore holds no resize or tensor declaration at all.

#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    /// The key the model was declared under; also its subscription unit.
    pub id: String,
    /// The processor program: the whole declaration of what the model is.
    ///
    /// clinfer runs it once per study as
    /// `interpreter script <workdir>` and encodes the objects its
    /// `result.json` declares. There is no in-process model kind: the
    /// sidecar is the transport, the site's script is the model.
    pub script: Option<PathBuf>,
    /// The interpreter that runs [`Self::script`], e.g. a venv's python.
    /// Defaults to `python3` when the script is declared without one.
    pub interpreter: Option<PathBuf>,
    /// Budget of one run, end to end.
    pub timeout_secs: u32,
    /// Ceiling on the `result.json` this model may write, in bytes.
    ///
    /// Per model rather than one global constant, because the document's
    /// size is a property of WHAT the model produces: a box list or a report
    /// is kilobytes, while a structure set grows with the traced polygon
    /// count, and the studies in one deployment differ by an order of
    /// magnitude. See [`DEFAULT_MAX_RESULT_BYTES`] for the reasoning and the
    /// measurements; a site with volumetric studies raises it on the model
    /// that produces them and leaves the rest where they are.
    pub max_result_bytes: u64,
}

/// A binding from one DICOM tag to value patterns.
#[derive(Debug, Clone, PartialEq)]
pub struct TagBinding {
    /// Upper-case hexadecimal `GGGGEEEE`.
    pub tag: String,
    /// Patterns matched case-insensitively with `*` wildcards.
    pub patterns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    /// Required, and the only mandatory key of a rule.
    pub modality: Vec<String>,
    /// (0040,4018) Scheduled Workitem Code Sequence Code Values, upper-cased.
    ///
    /// The PRIMARY key of a rule, and the reason it is first: the workitem
    /// carries its own intent (the server writes it from the study's modality
    /// or the requester sets it), so a rule can match WITHOUT fetching the
    /// study at all. The code is what the task was ASKED to do; the modality
    /// is a property of the input.
    ///
    /// An EMPTY list is unconstrained, which is exactly what every rule meant
    /// before this key existed - so an existing configuration keeps working
    /// with no migration.
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

    fn parse(text: &str) -> Option<Self> {
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

    fn parse(text: &str) -> Option<Self> {
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
pub struct RuleResult {
    pub kind: ResultKind,
    pub color: Option<String>,
    pub profile_version: u32,
    pub series_description: Option<String>,
    /// The label geometry for a pr result.
    pub label: LabelProfile,
    /// The sr result block, for an sr result.
    pub sr: Option<SrResult>,
    /// The measurements result block, for a measurements result.
    ///
    /// The same shape as `sr` - a series title and a template id - because
    /// it answers the same two questions about a different document. It is a
    /// separate field rather than a reuse of `sr` so that a result of one
    /// kind can never be handed the other kind's template.
    pub measurements: Option<SrResult>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub r#match: Match,
    pub model: String,
    pub result: RuleResult,
    /// Position in the file, used in messages.
    pub index: usize,
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

#[derive(Debug, Clone, PartialEq)]
pub struct Conf {
    pub server: ServerConf,
    pub engine: EngineConf,
    pub models: Vec<Model>,
    pub rules: Vec<Rule>,
    pub default_rule: DefaultRule,
}

impl Conf {
    #[must_use]
    pub fn model(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|model| model.id == id)
    }

    /// How many model processes may run at the same time.
    ///
    /// The ONE place the ceiling is computed, so startup, reload and the
    /// startup report cannot disagree about it:
    ///
    /// - absent -> every model with a script, which is exactly the behaviour
    ///   of a sidecar with one model and the natural reading of a site that
    ///   declared several;
    /// - configured larger -> capped to the number of models rather than
    ///   refused, because a ceiling above the number of threads that exist
    ///   is not an error, it is a redundant setting;
    /// - `0` never reaches here: the parser refuses it by name.
    #[must_use]
    pub fn workers(&self) -> usize {
        let models = self
            .models
            .iter()
            .filter(|model| model.script.is_some())
            .count();
        self.engine
            .workers
            .map_or(models, |workers| workers.min(models))
    }
}

/// Load and validate a configuration file.
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
    let conf = build(&document, absolute.parent()).map_err(|err| err.in_file(path))?;
    validate(&conf)?;
    Ok(conf)
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
    let models = parse_models(&mut root, base)?;
    let rules = parse_rules(&mut root)?;
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
        models,
        rules,
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
        trusted_proxies: owned(section.opt_str_list("trusted_proxies")?),
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

fn parse_models(root: &mut Section<'_>, base: Option<&Path>) -> Result<Vec<Model>> {
    let Some(models) = root.opt_mapping("models")? else {
        return Err(ConfError::new("models", "missing required section"));
    };
    let entries = models.entries();
    if entries.is_empty() {
        return Err(ConfError::new("models", "the mapping is empty"));
    }
    let mut out = Vec::with_capacity(entries.len());
    for (id, value) in entries {
        let mut model = Section::wrap(value, format!("models.{id}"))?;
        // Every model is an EXTERNAL PROCESSOR: a configured program that
        // reads the workdir and writes result.json. `script` is the whole
        // declaration of what the model is - clinfer carries no engine.
        let parsed = parse_external_model(&mut model, id, base)?;
        model.finish("a model")?;
        out.push(parsed);
    }
    Ok(out)
}

/// An external processor: a program clinfer runs once per study.
///
/// The workdir contract (see `abi/README.md`) is: clinfer writes the frames
/// and a manifest, runs `interpreter script <workdir>`, and reads the
/// script's `result.json`. The script never sees DICOM; keeping the object
/// writing inside clinfer is what makes the result conformant regardless of
/// what the site's script does.
///
/// `interpreter` defaults to `python3`. A site using a virtualenv points it
/// at the venv's own python binary - activation is an interactive-shell
/// convenience and has no place in a spawn path.
fn parse_external_model(model: &mut Section<'_>, id: &str, base: Option<&Path>) -> Result<Model> {
    let mut script = PathBuf::from(model.require_str("script")?);
    // A relative `script` is anchored at the configuration FILE, not at
    // the process cwd and not at the ABI workdir: the workdir is minted
    // per run, so nothing the site configures can live inside it, and
    // anchoring at the file is what makes a shipped demo configuration
    // work when the sidecar is started from anywhere.
    if script.is_relative()
        && let Some(dir) = base
    {
        script = dir.join(&script);
    }
    let mut interpreter = model
        .opt_str("interpreter")?
        .map_or_else(|| PathBuf::from(DEFAULT_INTERPRETER), PathBuf::from);
    // A relative interpreter THAT NAMES A PATH (has a separator) is
    // anchored at the configuration file exactly like `script`: the
    // child's cwd is the workdir, so a `./py` or `venv/bin/python`
    // resolved there would miss. A bare name stays bare - it is a PATH
    // lookup, and anchoring it would stop the platform python from ever
    // being found.
    if interpreter.is_relative()
        && interpreter.components().count() > 1
        && let Some(dir) = base
    {
        interpreter = dir.join(&interpreter);
    }
    // Optional with a default: a processor that does not say how long it
    // may take gets a generous budget rather than an unbounded wait.
    let timeout_secs = match model.opt_bounded_int("timeout_secs", 5, 1800)? {
        Some(value) => value as u32,
        None => DEFAULT_TIMEOUT_SECS,
    };
    let max_result_bytes = match model.opt_size("max_result_bytes")? {
        Some(value) => {
            if value == 0 {
                return Err(ConfError::new(
                    format!("{}.max_result_bytes", model.at()),
                    "0 is not a budget: omit the key to take the default",
                ));
            }
            if value > MAX_RESULT_BYTES_CEILING {
                return Err(ConfError::new(
                    format!("{}.max_result_bytes", model.at()),
                    format!(
                        "{value} is over the {MAX_RESULT_BYTES_CEILING}-byte ceiling; a \
                         result this large is a runaway script, not a study"
                    ),
                ));
            }
            value
        }
        None => DEFAULT_MAX_RESULT_BYTES,
    };
    Ok(Model {
        id: id.to_string(),
        script: Some(script),
        interpreter: Some(interpreter),
        timeout_secs,
        max_result_bytes,
    })
}

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
/// `max_result_bytes` on the model is where that is said.
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

fn parse_rules(root: &mut Section<'_>) -> Result<Vec<Rule>> {
    let items = root.opt_section_list("rules")?;
    if items.is_empty() {
        return Err(ConfError::new("rules", "the list is empty"));
    }
    let mut rules = Vec::with_capacity(items.len());
    for (index, mut item) in items.into_iter().enumerate() {
        let at = item.at().to_string();

        let mut m = match item.take("match")? {
            Some(value) => Section::wrap(value, format!("{at}.match"))?,
            None => {
                return Err(ConfError::new(
                    format!("{at}.match"),
                    "missing required key",
                ));
            }
        };
        let match_at = m.at().to_string();
        let modality: Vec<String> = m
            .require_str_list("modality")?
            .into_iter()
            .map(|value| value.to_ascii_uppercase())
            .collect();
        // The code is the OTHER optional key, and unlike the modality it is
        // not required: a rule written before codes existed constrains no
        // code, which is what keeps an existing configuration working.
        let code: Vec<String> = m
            .opt_str_list("code")?
            .into_iter()
            .map(|value| value.trim().to_ascii_uppercase())
            .collect();
        if code.iter().any(|value| value.is_empty()) {
            return Err(ConfError::new(
                m.child_at("code"),
                "a code value is empty; omit the key to constrain no code",
            ));
        }
        let study_description_contains = owned(m.opt_str_list("study_description_contains")?);
        let code_meaning_contains = owned(m.opt_str_list("code_meaning_contains")?);
        let body_part_contains = owned(m.opt_str_list("body_part_contains")?);
        let tags = match m.peek("tag") {
            Some(value) => {
                let parsed = parse_tag_bindings(value, &match_at)?;
                m.claim("tag");
                parsed
            }
            None => Vec::new(),
        };
        m.finish("a match block")?;
        let r#match = Match {
            modality,
            code,
            study_description_contains,
            code_meaning_contains,
            body_part_contains,
            tags,
        };

        let model = item.require_str("model")?.to_string();

        let mut result = match item.take("result")? {
            Some(value) => Section::wrap(value, format!("{at}.result"))?,
            None => {
                return Err(ConfError::new(
                    format!("{at}.result"),
                    "missing required key",
                ));
            }
        };
        let kind = parse_enum(
            &mut result,
            "kind",
            ResultKind::parse,
            ResultKind::ALL.iter().map(|k| k.as_str()).collect(),
        )?;
        let color = result.opt_str("color")?.map(str::to_string);
        match result.peek("colors") {
            // Per-class layer colors. The map is a presentation-state
            // feature; a rule producing anything else would carry a key
            // nothing reads.
            Some(_) if kind != ResultKind::Pr => {
                return Err(ConfError::new(
                    result.child_at("colors"),
                    format!("'{kind}' results have no per-class colors; pr only"),
                ));
            }
            Some(value) if color.is_none() => {
                let _ = value;
                return Err(ConfError::new(
                    result.child_at("color"),
                    "required when colors is present",
                ));
            }
            // Claimed but never read: no code in the crate consumes a per-class
            // colour map, so accepting it let an operator write a presentation
            // setting that silently did nothing. A key nothing reads is a
            // configuration error, and the start-up is where it should be told.
            Some(_) => {
                return Err(ConfError::new(
                    result.child_at("colors"),
                    "'colors' is not supported; no per-class colour map is read",
                ));
            }
            None => {}
        }
        let profile_version = match result.opt_bounded_int("profile_version", 1, 1_000_000)? {
            Some(value) => value as u32,
            None => 1,
        };
        let series_description = result.opt_str("series_description")?.map(str::to_string);
        let label = match result.opt_mapping("label")? {
            Some(mut block) => {
                let anchor = match block.opt_str("anchor")? {
                    Some(text) => LabelAnchor::parse(text).ok_or_else(|| {
                        ConfError::new(block.child_at("anchor"), "expected 'tl' or 'center'")
                    })?,
                    None => LabelAnchor::default(),
                };
                let offset_y_px = match block.opt_bounded_int("offset_y_px", -100_000, 100_000)? {
                    Some(value) => value as i32,
                    None => -110,
                };
                let offset_x_left_px =
                    match block.opt_bounded_int("offset_x_left_px", -100_000, 100_000)? {
                        Some(value) => value as i32,
                        None => -200,
                    };
                let offset_x_right_px =
                    match block.opt_bounded_int("offset_x_right_px", -100_000, 100_000)? {
                        Some(value) => value as i32,
                        None => 80,
                    };
                if offset_x_left_px >= offset_x_right_px {
                    return Err(ConfError::new(
                        block.child_at("offset_x_right_px"),
                        "must be greater than offset_x_left_px",
                    ));
                }
                block.finish("a label block")?;
                LabelProfile {
                    anchor,
                    offset_y_px,
                    offset_x_left_px,
                    offset_x_right_px,
                }
            }
            None => LabelProfile::default(),
        };
        let sr = match result.peek("sr") {
            Some(_) if kind != ResultKind::Sr => {
                return Err(ConfError::new(
                    result.child_at("sr"),
                    format!("'{kind}' results carry no sr block; sr only"),
                ));
            }
            Some(value) => {
                let parsed = parse_sr_block(value, &result.child_at("sr"), SrTemplate::default())?;
                result.claim("sr");
                Some(parsed)
            }
            // An sr result without a block is legal: the template and the
            // title both have defaults, and a report with no series
            // description is still a report. The block exists to say
            // something, not to be said.
            None if kind == ResultKind::Sr => Some(SrResult::default()),
            None => None,
        };
        // The measurements block: the same shape as sr, but its template
        // defaults to TID 1500, because that is the template an Enhanced SR
        // follows and the only one a numeric finding may declare.
        let measurements = match result.peek("measurements") {
            Some(_) if kind != ResultKind::Measurements => {
                return Err(ConfError::new(
                    result.child_at("measurements"),
                    format!("'{kind}' results carry no measurements block; measurements only"),
                ));
            }
            Some(value) => {
                let parsed =
                    parse_sr_block(value, &result.child_at("measurements"), SrTemplate::Tid1500)?;
                result.claim("measurements");
                Some(parsed)
            }
            None if kind == ResultKind::Measurements => Some(SrResult {
                title: None,
                template_id: SrTemplate::Tid1500,
            }),
            None => None,
        };
        result.finish("a result block")?;
        item.finish("a rule")?;

        rules.push(Rule {
            r#match,
            model,
            result: RuleResult {
                kind,
                color,
                profile_version,
                series_description,
                label,
                sr,
                measurements,
            },
            index,
        });
    }
    Ok(rules)
}

/// The `sr:` (or `measurements:`) block of a report result.
///
/// Both keys are optional, and the block itself is optional: a report result
/// with no block is a document with the defaults. A block on a result of any
/// other kind is refused by the caller, because a key nothing reads is a
/// declaration that will not happen.
///
/// `default_template` is the template the block's kind follows when the
/// administrator names none: TID 2000 for `sr`, TID 1500 for `measurements`.
/// The two blocks share this parser because they answer the same two
/// questions; they differ only in which template is the right default.
fn parse_sr_block(value: &Yaml, at: &str, default_template: SrTemplate) -> Result<SrResult> {
    let mut section = Section::wrap(value, at)?;
    let title = section.opt_str("title")?.map(str::to_string);
    let template_id = match section.opt_str("template_id")? {
        Some(text) => unknown_value(
            SrTemplate::parse,
            text,
            &section.child_at("template_id"),
            SrTemplate::ALL.iter().map(|t| t.as_str()).collect(),
        )?,
        None => default_template,
    };
    section.finish("a report block")?;
    Ok(SrResult { title, template_id })
}

fn parse_tag_bindings(value: &Yaml, at: &str) -> Result<Vec<TagBinding>> {
    let section_at = format!("{at}.tag");
    let mut section = Section::wrap(value, &section_at)?;
    let entries = section.entries();
    if entries.is_empty() {
        return Err(ConfError::new(&section_at, "the mapping is empty"));
    }
    let mut out = Vec::with_capacity(entries.len());
    for (key, patterns) in entries {
        let tag = normalize_tag(key).ok_or_else(|| {
            ConfError::new(format!("{section_at}.{key}"), "expected 8 hex digits")
        })?;
        let item_at = format!("{section_at}.{key}");
        let patterns = owned(str_items(patterns, &item_at)?);
        if patterns.is_empty() {
            return Err(ConfError::new(&item_at, "the list is empty"));
        }
        section.claim(key);
        out.push(TagBinding { tag, patterns });
    }
    section.finish("a tag block")?;
    Ok(out)
}

fn parse_enum<T: Copy>(
    section: &mut Section<'_>,
    key: &str,
    parse: impl Fn(&str) -> Option<T>,
    allowed: Vec<&str>,
) -> Result<T> {
    let at = section.child_at(key);
    let text = section
        .opt_str(key)?
        .ok_or_else(|| ConfError::new(&at, "missing required key"))?;
    unknown_value(parse, text, &at, allowed)
}

/// Read an enum key whose absence is legal.
///
/// The legal set is still enforced: a key that is present but names
/// nothing is a typo in a declaration, and treating a typo as "absent"
/// would silently apply the default.
fn unknown_value<T>(
    parse: impl Fn(&str) -> Option<T>,
    text: &str,
    at: &str,
    allowed: Vec<&str>,
) -> Result<T> {
    parse(text).ok_or_else(|| {
        ConfError::new(
            at,
            format!("unknown value '{text}'; expected {}", allowed.join(", ")),
        )
    })
}

fn owned(values: Vec<&str>) -> Vec<String> {
    values.into_iter().map(str::to_string).collect()
}

/// `00211109`, `0021,1109` and `0021, 1109` all name the same tag; the
/// canonical form is eight upper-case hex digits.
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

fn validate(conf: &Conf) -> Result<()> {
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
    use crate::yaml::parse_document;

    /// The document is leaked so a section can borrow it for the length of
    /// the test body; tests are short-lived processes.
    fn document(source: &str) -> &'static Yaml {
        Box::leak(Box::new(
            parse_document(source, Path::new("t")).expect("parses"),
        ))
    }

    #[test]
    fn every_result_kind_round_trips_through_its_name() {
        for kind in ResultKind::ALL {
            assert_eq!(ResultKind::parse(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn every_kind_has_a_built_encoder() {
        // The set is spelled out rather than derived, so dropping an
        // encoder without saying so here fails this test. Every kind is
        // listed because every kind is written by this crate: a `false`
        // here would refuse a configuration the build can actually serve,
        // which is how `seg` and `derived` were unreachable before.
        for kind in [
            ResultKind::Pr,
            ResultKind::Sr,
            ResultKind::Measurements,
            ResultKind::Seg,
            ResultKind::Stl,
            ResultKind::Derived,
            ResultKind::Rtstruct,
        ] {
            assert!(kind.encoder_built(), "{kind} has an encoder");
        }
        assert_eq!(ResultKind::ALL.len(), 7, "every kind is covered above");
    }

    #[test]
    fn tag_normalization_accepts_separators() {
        assert_eq!(normalize_tag("00211109").as_deref(), Some("00211109"));
        assert_eq!(normalize_tag("0021,1109").as_deref(), Some("00211109"));
        assert_eq!(normalize_tag("0021,ab09").as_deref(), Some("0021AB09"));
        assert_eq!(normalize_tag("0021, 1109").as_deref(), Some("00211109"));
        assert_eq!(normalize_tag("0021110"), None);
        // Whitespace is a separator, so this is not eight digits.
        assert_eq!(normalize_tag("0021 1109"), Some("00211109".to_string()));
        assert_eq!(normalize_tag("0021110g"), None);
        assert_eq!(normalize_tag(""), None);
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

    /// A rule's result block, parsed with the same reader `load` uses.
    fn rule_result_of(result_yaml: &str) -> RuleResult {
        let source = format!(
            "rules:\n  - match:\n      modality: [DX]\n    model: m\n    result:\n{result_yaml}"
        );
        let doc = document(&source);
        let mut root = Section::wrap(doc, "").expect("mapping");
        let rules = parse_rules(&mut root).expect("rules parse");
        assert_eq!(rules.len(), 1, "one rule");
        rules[0].result.clone()
    }

    #[test]
    fn the_label_block_parses_into_the_rule_result() {
        let result = rule_result_of(
            "      kind: pr\n      label:\n        anchor: center\n        offset_y_px: -50\n        offset_x_left_px: -150\n        offset_x_right_px: 90\n",
        );
        assert_eq!(result.label.anchor, LabelAnchor::Center);
        assert_eq!(result.label.offset_y_px, -50);
        assert_eq!(result.label.offset_x_left_px, -150);
        assert_eq!(result.label.offset_x_right_px, 90);
    }

    #[test]
    fn an_absent_label_block_defaults_to_the_corner() {
        let result = rule_result_of("      kind: pr\n");
        assert_eq!(result.label, LabelProfile::default());
    }

    // -- The measurements result block -------------------------------------

    #[test]
    fn a_measurements_result_defaults_its_template_to_tid_1500() {
        // The block is optional, and its default template is the one an
        // Enhanced SR follows - NOT the TID 2000 a text report would take.
        let result = rule_result_of("      kind: measurements\n");
        assert_eq!(result.kind, ResultKind::Measurements);
        let block = result.measurements.as_ref().expect("the block defaults");
        assert_eq!(block.template_id, SrTemplate::Tid1500);
        assert!(block.title.is_none());
        assert!(result.sr.is_none(), "measurements is not an sr block");
    }

    #[test]
    fn a_measurements_block_carries_its_title_and_template() {
        let result = rule_result_of(
            "      kind: measurements\n      measurements:\n        title: Chest classifier\n        template_id: TID1500\n",
        );
        let block = result.measurements.as_ref().expect("the block");
        assert_eq!(block.title.as_deref(), Some("Chest classifier"));
        assert_eq!(block.template_id, SrTemplate::Tid1500);
    }

    #[test]
    fn a_measurements_block_may_declare_either_template() {
        // The block's DEFAULT is TID 1500, but the template is the
        // administrator's to name and both ids are implemented by this
        // build. Pinned so the default is not mistaken for a constraint.
        let result = rule_result_of(
            "      kind: measurements\n      measurements:\n        template_id: TID2000\n",
        );
        let block = result.measurements.as_ref().expect("the block");
        assert_eq!(block.template_id, SrTemplate::Tid2000);
    }

    #[test]
    fn a_measurements_block_refuses_an_unknown_template() {
        // A value that is not a template this build implements. Both
        // Tid2000 and Tid1500 are legal in the block (the administrator may
        // declare either); an unlisted id is what the parser must refuse.
        let source = "rules:\n  - match:\n      modality: [DX]\n    model: m\n    result:\n      kind: measurements\n      measurements:\n        template_id: TID1419\n";
        let doc = document(source);
        let mut root = Section::wrap(doc, "").expect("mapping");
        let err = parse_rules(&mut root).expect_err("an unknown template is refused");
        assert!(err.to_string().contains("template_id"), "{err}");
    }

    #[test]
    fn a_measurements_block_on_a_text_report_is_refused() {
        // A key nothing reads is a declaration that will not happen: the
        // template would be parsed and then silently ignored.
        let source = "rules:\n  - match:\n      modality: [DX]\n    model: m\n    result:\n      kind: sr\n      measurements:\n        title: nope\n";
        let doc = document(source);
        let mut root = Section::wrap(doc, "").expect("mapping");
        let err = parse_rules(&mut root).expect_err("sr carries no measurements block");
        assert!(err.to_string().contains("measurements"), "{err}");
    }

    #[test]
    fn an_sr_block_on_a_measurements_result_is_refused() {
        let source = "rules:\n  - match:\n      modality: [DX]\n    model: m\n    result:\n      kind: measurements\n      sr:\n        title: nope\n";
        let doc = document(source);
        let mut root = Section::wrap(doc, "").expect("mapping");
        let err = parse_rules(&mut root).expect_err("measurements carries no sr block");
        assert!(err.to_string().contains("sr"), "{err}");
    }

    #[test]
    fn an_unknown_key_in_a_measurements_block_is_refused() {
        let source = "rules:\n  - match:\n      modality: [DX]\n    model: m\n    result:\n      kind: measurements\n      measurements:\n        titel: typo\n";
        let doc = document(source);
        let mut root = Section::wrap(doc, "").expect("mapping");
        let err = parse_rules(&mut root).expect_err("a misspelling is an error");
        assert!(err.to_string().contains("titel"), "{err}");
    }

    #[test]
    fn a_degenerate_label_box_is_rejected() {
        let source = "rules:\n  - match:\n      modality: [DX]\n    model: m\n    result:\n      kind: pr\n      label:\n        offset_x_left_px: 10\n        offset_x_right_px: -10\n";
        let doc = document(source);
        let mut root = Section::wrap(doc, "").expect("mapping");
        let err = parse_rules(&mut root).expect_err("degenerate box must fail");
        assert!(err.to_string().contains("offset_x_right_px"), "{err}");
    }

    #[test]
    fn an_unknown_anchor_value_is_rejected() {
        let source = "rules:\n  - match:\n      modality: [DX]\n    model: m\n    result:\n      kind: pr\n      label:\n        anchor: corner\n";
        let doc = document(source);
        let mut root = Section::wrap(doc, "").expect("mapping");
        let err = parse_rules(&mut root).expect_err("bad anchor must fail");
        assert!(err.to_string().contains("anchor"), "{err}");
        assert!(err.to_string().contains("'tl' or 'center'"), "{err}");
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

    // -- The result kinds --------------------------------------------------

    #[test]
    fn the_sr_template_names_round_trip() {
        for template in SrTemplate::ALL {
            assert_eq!(SrTemplate::parse(template.as_str()), Some(template));
        }
        assert_eq!(SrTemplate::parse("tid2000"), None, "the casing is fixed");
        assert_eq!(SrTemplate::parse("1419"), None);
        assert_eq!(SrTemplate::default(), SrTemplate::Tid2000);
        // The three spellings the object carries must agree on the one
        // template, or the report would name a template it does not follow.
        assert_eq!(SrTemplate::Tid2000.designator(), "DCMR");
        assert_eq!(SrTemplate::Tid2000.template_number(), "2000");
        // TID 1500 carries the same designator and its own number.
        assert_eq!(SrTemplate::Tid1500.as_str(), "TID1500");
        assert_eq!(SrTemplate::Tid1500.designator(), "DCMR");
        assert_eq!(SrTemplate::Tid1500.template_number(), "1500");
        assert_eq!(SrTemplate::parse("tid1500"), None, "the casing is fixed");
    }

    // -- The per-model result budget ---------------------------------------

    /// Parse one `models:` entry through the real reader.
    fn model_of(body: &str) -> Result<Model> {
        let source = format!("models:\n  m:\n{body}");
        let doc = document(&source);
        let mut root = Section::wrap(doc, "").expect("mapping");
        parse_models(&mut root, None).map(|models| models.into_iter().next().expect("one model"))
    }

    /// A relative `script` resolves against the configuration file's
    /// directory, so a packaged config works from any process cwd.
    #[test]
    fn a_relative_script_is_anchored_at_the_configuration_file() {
        let source = "models:\n  m:\n    script: ./totalseg.py\n";
        let doc = document(source);
        let mut root = Section::wrap(doc, "").expect("mapping");
        let base = Path::new("/opt/demo/pack");
        let models = parse_models(&mut root, Some(base)).expect("parses");
        let model = &models[0];
        assert_eq!(
            model.script,
            Some(PathBuf::from("/opt/demo/pack/totalseg.py")),
            "a relative script joins the config directory"
        );

        // An absolute script is left alone: the site pointed at a fixed
        // location and the parser must not second-guess it.
        let source = "models:\n  m:\n    script: /opt/clinfer/scripts/clahe.py\n";
        let doc = document(source);
        let mut root = Section::wrap(doc, "").expect("mapping");
        let models = parse_models(&mut root, Some(base)).expect("parses");
        assert_eq!(
            models[0].script,
            Some(PathBuf::from("/opt/clinfer/scripts/clahe.py"))
        );
    }

    /// `load` anchors at the file's directory even when the config path
    /// is a BARE file name: `Path::parent` of `pack.conf` is the empty
    /// path, so the resolver absolutizes the file first.
    #[test]
    fn absolutize_turns_a_bare_path_absolute() {
        let bare = Path::new("pack.conf");
        let absolute = super::absolutize(bare);
        assert!(absolute.is_absolute(), "{absolute:?} is not absolute");
        assert!(absolute.ends_with(bare), "{absolute:?} lost the file name");
        // Absolute on every platform by construction: a Unix-style
        // `/opt/x/...` is NOT absolute on Windows (no drive prefix).
        let fixed = std::env::current_dir()
            .expect("cwd")
            .join("opt/x/pack.conf");
        assert_eq!(
            super::absolutize(&fixed),
            fixed,
            "an absolute path is returned untouched"
        );
    }

    /// A relative interpreter that NAMES A PATH anchors at the config
    /// file like `script`; a bare name stays a PATH lookup.
    #[test]
    fn a_relative_interpreter_follows_the_script_anchor() {
        let base = Path::new("/opt/demo/pack");
        let parse = |body: &str| {
            let source = format!("models:\n  m:\n{body}");
            let doc = document(&source);
            let mut root = Section::wrap(doc, "").expect("mapping");
            parse_models(&mut root, Some(base)).expect("parses")
        };

        let anchored = parse("    script: /s/proc.py\n    interpreter: ./venv/bin/python\n");
        assert_eq!(
            anchored[0].interpreter,
            Some(PathBuf::from("/opt/demo/pack/venv/bin/python")),
            "a relative path-interpreter joins the config directory"
        );

        let bare = parse("    script: /s/proc.py\n    interpreter: python3\n");
        assert_eq!(
            bare[0].interpreter,
            Some(PathBuf::from("python3")),
            "a bare name stays a PATH lookup"
        );
    }

    /// An absent key takes the default, so an existing configuration is
    /// unchanged by the knob's arrival.
    #[test]
    fn a_model_without_a_result_budget_takes_the_default() {
        let model = model_of("    script: proc.py\n").expect("parses");
        assert_eq!(model.max_result_bytes, DEFAULT_MAX_RESULT_BYTES);
    }

    /// Both spellings of the same budget mean the same number: the suffixed
    /// form is the documented one and the bare count is what it decodes to.
    #[test]
    fn a_budget_is_read_from_a_suffix_or_a_bare_count() {
        let suffixed = model_of("    script: proc.py\n    max_result_bytes: 128m\n")
            .expect("a suffixed budget parses");
        assert_eq!(suffixed.max_result_bytes, 128 * 1024 * 1024);

        let bare = model_of("    script: proc.py\n    max_result_bytes: 134217728\n")
            .expect("a bare budget parses");
        assert_eq!(bare.max_result_bytes, 128 * 1024 * 1024);

        let kilobytes = model_of("    script: proc.py\n    max_result_bytes: 64k\n")
            .expect("a k suffix parses");
        assert_eq!(kilobytes.max_result_bytes, 64 * 1024);
    }

    /// Every unusable value is refused AT STARTUP with a message naming the
    /// key, rather than after a run has already read a gigabyte.
    #[test]
    fn an_unusable_budget_is_refused_with_its_key_named() {
        let cases: &[(&str, &str)] = &[
            // Zero is not a budget: it would refuse every result, and the
            // honest way to say "no result" is to remove the model.
            (
                "    script: proc.py\n    max_result_bytes: 0\n",
                "0 is not a budget",
            ),
            // Fractions are refused rather than rounded: rounding is a
            // silent wrong answer to a question nobody asked.
            (
                "    script: proc.py\n    max_result_bytes: 1.5m\n",
                "expected a byte count",
            ),
            // A unit the convention does not define.
            (
                "    script: proc.py\n    max_result_bytes: 64mb\n",
                "expected a byte count",
            ),
            // Negative, which is not a byte count in any reading.
            (
                "    script: proc.py\n    max_result_bytes: -1\n",
                "not a byte count",
            ),
            // Over the ceiling: a typo, not a deployment.
            (
                "    script: proc.py\n    max_result_bytes: 64g\n",
                "expected a byte count",
            ),
            (
                "    script: proc.py\n    max_result_bytes: 2g\n",
                "expected a byte count",
            ),
        ];
        for (body, expected) in cases {
            let err = model_of(body).expect_err(body);
            let text = err.to_string();
            assert!(
                text.contains("max_result_bytes"),
                "{body}: the message must name the key: {text}"
            );
            assert!(
                text.contains(expected),
                "{body}: expected {expected:?}, got {text}"
            );
        }
    }

    /// The ceiling itself is a plain integer count, so it is worth stating
    /// that the bound is inclusive at the top and exclusive of anything over.
    #[test]
    fn the_budget_ceiling_is_inclusive() {
        let at_ceiling = model_of(&format!(
            "    script: proc.py\n    max_result_bytes: {MAX_RESULT_BYTES_CEILING}\n"
        ))
        .expect("the ceiling itself is accepted");
        assert_eq!(at_ceiling.max_result_bytes, MAX_RESULT_BYTES_CEILING);

        let over = model_of(&format!(
            "    script: proc.py\n    max_result_bytes: {}\n",
            MAX_RESULT_BYTES_CEILING + 1
        ))
        .expect_err("one byte over is refused");
        assert!(over.to_string().contains("ceiling"), "{over}");
    }

    /// The budget belongs to the MODEL, so two models in one file may differ.
    #[test]
    fn two_models_may_carry_different_budgets() {
        let source = "models:\n  detect:\n    script: d.py\n    max_result_bytes: 1m\n  outline:\n    script: o.py\n    max_result_bytes: 512m\n";
        let doc = document(source);
        let mut root = Section::wrap(doc, "").expect("mapping");
        let models = parse_models(&mut root, None).expect("both models parse");

        let detect = models.iter().find(|m| m.id == "detect").expect("detect");
        let outline = models.iter().find(|m| m.id == "outline").expect("outline");
        assert_eq!(detect.max_result_bytes, 1024 * 1024);
        assert_eq!(outline.max_result_bytes, 512 * 1024 * 1024);
        assert_ne!(
            detect.max_result_bytes, outline.max_result_bytes,
            "one budget per model, not one per file"
        );
    }
}
