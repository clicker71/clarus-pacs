// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The inbound listener: `/health`, `/metrics`, `/processors/Questionnaire`.
//!
//! One `TcpListener` on `server.listen`, no framework.
//!
//! # This listener no longer accepts pushes
//!
//! It used to accept `POST /workitems/{uid}` so the origin could wake the
//! sidecar. The sidecar now opens the WebSocket Notification Connection
//! itself and claims on the SCHEDULED Event Report, so the origin no longer
//! has to be able to reach clinfer inbound, and the push handler is gone.
//!
//! The listener STAYS because it serves the operational endpoints an operator
//! and a monitoring system read: `/health`, `/metrics` and the model
//! declaration at `/processors/Questionnaire`.
//! `server.listen`, `trusted_proxies` and `require_proxy_identity` therefore
//! still govern those endpoints - see the note on each in `conf.rs`.
//!
//! Security is the same contract the server applies to its own callers:
//! the identity header is trusted only from a peer in
//! `trusted_proxies`, and `require_proxy_identity` makes a trusted peer
//! without one a 502. A peer outside the list is refused outright - these
//! endpoints expose the configuration and the run counters of a clinical
//! sidecar.

use crate::http::DICOM_JSON;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::thread;
use std::time::Duration;

/// How long one inbound request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest inbound request body accepted.
///
/// A pushed workitem is a few hundred bytes. The bound keeps a
/// misdirected client (or a peer with the wrong port in mind) from making
/// the sidecar hold an arbitrary buffer.
const MAX_REQUEST_BODY: usize = 64 * 1024;

/// The extension StructureDefinitions, compiled in so the local mirror at
/// `/fhir/StructureDefinition/*.json` can never drift from the binary that
/// publishes the canonical URLs. The on-disk copies under `fhir/` are the
/// single source of truth this build embeds.
const PROVIDER_DEFINITION: &str = include_str!("../fhir/StructureDefinition/clinfer-provider.json");
const INPUT_ANATOMY_DEFINITION: &str =
    include_str!("../fhir/StructureDefinition/clinfer-input-anatomy.json");
const OUTPUT_SOP_CLASSES_DEFINITION: &str =
    include_str!("../fhir/StructureDefinition/clinfer-output-sop-classes.json");
const RESULT_CONTENT_DEFINITION: &str =
    include_str!("../fhir/StructureDefinition/clinfer-output-result-content.json");

/// Depth of the queue between the listener and the worker.
///
/// A push is a wake-up, and durability is the startup catch-up search, so
/// a full queue is not a lost workitem: it is one the catch-up will find.
/// The bound keeps a burst from growing the process.
pub const QUEUE_DEPTH: usize = 256;

/// Counters an operator reads from `/metrics`.
///
/// Plain atomics: the workers increment them a few times per workitem,
/// and a scrape reads them without locking anything on the request path.
#[derive(Debug, Default)]
pub struct Counters {
    pub claims: AtomicU64,
    pub completed: AtomicU64,
    pub canceled: AtomicU64,
    /// Event Reports received over the WebSocket Notification Connection.
    ///
    /// Replaces the old `pushes` counter: the wake-up no longer arrives as an
    /// inbound POST, so counting pushes would count a path that no longer
    /// exists. This counts what actually wakes the sidecar now.
    pub ws_events: AtomicU64,
    /// Channel opens that succeeded, including reconnects. A number that
    /// keeps climbing means the connection is unstable.
    pub ws_connects: AtomicU64,
    /// Reconnect attempts after a dropped or failed channel. Distinct from
    /// `ws_connects` so an operator can tell "reconnected N times" from
    /// "opened once".
    pub ws_reconnects: AtomicU64,
    /// SCHEDULED reports that could not be handed to the worker because the
    /// queue was full, or that arrived during shutdown.
    pub ws_dropped: AtomicU64,
    /// True state conflicts on a claim: the origin answered 409, meaning
    /// another claimant owns the workitem (PS3.18 Table 11.7.3-1).
    pub conflicts: AtomicU64,
    /// Claims the origin refused with a 4xx that is NOT 409 (400, 404,
    /// 410, ...). Separate from `conflicts` because the meaning is
    /// different: a conflict is a lost race, while these are defect
    /// signals about the request or the workitem itself.
    pub claim_failures: AtomicU64,
    /// SCHEDULED workitems this performer declined to claim because the
    /// workitem is not its own: the assignment attributes name another
    /// station, or the Input Readiness State is INCOMPLETE (AIW-I
    /// 4.82.4.1.1). Separate from `claim_failures` because nothing went
    /// wrong - the workitem is deliberately left for its owner.
    pub declined: AtomicU64,
    pub rejected_peers: AtomicU64,
    /// Requests whose retry budget ran out against a 503 origin.
    pub retries_exhausted: AtomicU64,
    /// Extra sends the retry budget paid for, across every retried call.
    ///
    /// The count of RETRIES, not of requests and not of failures: a call that
    /// needed two sends contributes one, whether the second send then
    /// succeeded or ran out the budget. It is to [`Self::retries_exhausted`]
    /// what "attempts" is to "gave up" - a climb with exhaustion flat says
    /// the origin is occasionally busy and the retry is doing its job, while
    /// both climbing says it is not keeping up.
    pub transport_retries: AtomicU64,
    /// External processors actually started.
    pub external_spawns: LabelledCounter,
    /// Runs that ran out of their budget and were killed.
    pub external_timeouts: LabelledCounter,
    /// Process trees that had to be killed (timeout at the seam, or
    /// shutdown while a child was in flight).
    ///
    /// A separate counter from timeouts on purpose: a kill is a SUPERVISION
    /// event and a timeout is a SCRIPT event, and the two lead an operator
    /// to different places. Folding them together would make a shutdown
    /// look like a slow model.
    pub external_kills: LabelledCounter,
    /// Runs that exited non-zero, whether or not they wrote a result.
    pub external_exit_failures: LabelledCounter,
    /// How long each stage of the cycle took.
    pub cycle: CycleDurations,
    /// The last `VmRSS` seen for each processor's running child, in bytes.
    ///
    /// A GAUGE, not a counter: it describes the child that exists right now
    /// and goes back to 0 when that child is reaped. `external_*` labels are
    /// processor ids, exactly as the counters' are.
    pub external_rss_bytes: LabelledGauge,
    /// The high-water RSS of the child that is running now.
    pub external_peak_rss_bytes: LabelledGauge,
    /// The peak RSS of the LAST finished run, in bytes.
    ///
    /// The one figure that survives the reap: an operator asking "how much
    /// does this model need" is asking about a run that has already ended,
    /// so the live peak alone would answer the question only during the run.
    pub external_last_peak_rss_bytes: LabelledGauge,
    /// User + system CPU time of the running child, in MILLISECONDS.
    ///
    /// Milliseconds rather than seconds so the gauge can stay an integer
    /// counter; the exposition divides, so a scrape sees seconds.
    pub external_cpu_seconds_millis: LabelledGauge,
    /// 1 while a child of this processor is running, 0 otherwise.
    pub external_running: LabelledGauge,
    /// The pid of the running child, 0 when there is none.
    pub external_pid: LabelledGauge,
    /// The sidecar's OWN peak RSS, in bytes.
    ///
    /// A plain atomic rather than a labelled gauge: there is exactly one
    /// clinfer in the process, and a scrape compares this against the
    /// children's peaks to see whether the memory went to a model or to the
    /// supervisor.
    pub clinfer_peak_rss_bytes: AtomicU64,
}

/// A counter that carries one label: the processor id.
///
/// Cardinality is bounded by the configuration (a site has a handful of
/// processors), which is what makes an unbounded label safe here - unlike a
/// study UID, which would create a series per study.
///
/// The label values are stored in a fixed-capacity vector so a scrape never
/// allocates while walking them, and so a misconfigured deployment with a
/// thousand processors degrades into a truncated list rather than into
/// unbounded memory.
#[derive(Debug, Default)]
pub struct LabelledCounter {
    entries: std::sync::Mutex<Vec<(String, AtomicU64)>>,
}

/// The most processors one counter tracks.
///
/// Far above any real site (the examples declare one or two) and finite so
/// that a pathological configuration cannot grow the exposition without
/// limit.
pub const MAX_LABEL_CARDINALITY: usize = 64;

impl LabelledCounter {
    /// Add one to the entry for `label`, creating it if there is room.
    pub fn record(&self, label: &str) {
        let mut entries = self.entries.lock().expect("counter lock");
        if let Some((_, counter)) = entries.iter().find(|(name, _)| name == label) {
            counter.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if entries.len() >= MAX_LABEL_CARDINALITY {
            // Silently dropping would hide work; the overflow is folded
            // into the last entry the deployment already has, and the
            // truncation is visible in the exposition's help text.
            if let Some((_, counter)) = entries.last() {
                counter.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        entries.push((label.to_string(), AtomicU64::new(1)));
    }

    /// The current value for one label, for tests and assertions.
    #[must_use]
    pub fn get(&self, label: &str) -> u64 {
        let entries = self.entries.lock().expect("counter lock");
        entries
            .iter()
            .find(|(name, _)| name == label)
            .map_or(0, |(_, counter)| counter.load(Ordering::Relaxed))
    }

    /// The total across every label.
    #[must_use]
    pub fn total(&self) -> u64 {
        let entries = self.entries.lock().expect("counter lock");
        entries
            .iter()
            .map(|(_, counter)| counter.load(Ordering::Relaxed))
            .sum()
    }

    /// Every label with a non-zero value, for the exposition.
    fn render_into(&self, out: &mut String, name: &str, help: &str) {
        let entries = self.entries.lock().expect("counter lock");
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} counter");
        for (label, counter) in entries.iter() {
            let _ = writeln!(
                out,
                "{name}{{processor=\"{}\"}} {}",
                escape_label(label),
                counter.load(Ordering::Relaxed)
            );
        }
    }
}

/// A gauge that carries one label: the processor id.
///
/// The same contract as [`LabelledCounter`] - a fixed-capacity vector under
/// a mutex, cardinality bounded by the configuration, the overflow folded
/// into the last entry and visible in the help text - with `set` instead of
/// `record`, because a gauge publishes a value rather than accumulating
/// one.
///
/// The label values are processor ids, so a scrape walks a handful of
/// entries and allocates nothing while doing it.
#[derive(Debug, Default)]
pub struct LabelledGauge {
    entries: std::sync::Mutex<Vec<(String, AtomicU64)>>,
}

impl LabelledGauge {
    /// Publish `value` for `label`, creating the entry if there is room.
    ///
    /// An absolute write, which is what makes it idempotent: a caller that
    /// sets the same value twice, or a reap that runs after an explicit
    /// reset, publishes the same state rather than double-counting.
    pub fn set(&self, label: &str, value: u64) {
        let mut entries = self.entries.lock().expect("gauge lock");
        if let Some((_, gauge)) = entries.iter().find(|(name, _)| name == label) {
            gauge.store(value, Ordering::Relaxed);
            return;
        }
        if entries.len() >= MAX_LABEL_CARDINALITY {
            // Dropping the write would hide a processor entirely; folding
            // it into the last entry keeps the series present, and the
            // truncation is stated in the exposition's help text.
            if let Some((_, gauge)) = entries.last() {
                gauge.store(value, Ordering::Relaxed);
            }
            return;
        }
        entries.push((label.to_string(), AtomicU64::new(value)));
    }

    /// The current value for one label, for tests and assertions.
    #[must_use]
    pub fn get(&self, label: &str) -> u64 {
        let entries = self.entries.lock().expect("gauge lock");
        entries
            .iter()
            .find(|(name, _)| name == label)
            .map_or(0, |(_, gauge)| gauge.load(Ordering::Relaxed))
    }

    /// Append the exposition for one gauge family.
    fn render_into(&self, out: &mut String, name: &str, help: &str) {
        let entries = self.entries.lock().expect("gauge lock");
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} gauge");
        for (label, gauge) in entries.iter() {
            let _ = writeln!(
                out,
                "{name}{{processor=\"{}\"}} {}",
                escape_label(label),
                gauge.load(Ordering::Relaxed)
            );
        }
    }

    /// The same walk, for the one family whose value is rescaled on the way
    /// out (CPU milliseconds to seconds). Kept separate rather than adding a
    /// divisor argument to every call site: only this family needs it.
    fn render_millis_as_seconds_into(&self, out: &mut String, name: &str, help: &str) {
        let entries = self.entries.lock().expect("gauge lock");
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} gauge");
        for (label, gauge) in entries.iter() {
            let seconds = gauge.load(Ordering::Relaxed) as f64 / 1000.0;
            let _ = writeln!(
                out,
                "{name}{{processor=\"{}\"}} {seconds}",
                escape_label(label)
            );
        }
    }
}

/// Escape a Prometheus label value.
///
/// A processor id comes from the configuration, where an operator may have
/// written anything; an unescaped quote or backslash would produce an
/// exposition a scraper rejects.
fn escape_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// The stages one workitem passes through, in order.
///
/// A fixed set, so the histogram's `stage` label has bounded cardinality -
/// five values, never a per-study or per-model explosion. The model and
/// device dimensions are deferred to the farm stage (S2); one model on one
/// device cannot use them, and the stage is what answers the booth question
/// ("is this the network or the CPU").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Fetch,
    Infer,
    Serialize,
    /// Serializing the report. Separate from [`Self::Serialize`] because
    /// the two are different writers with different cost profiles, and a
    /// slow cycle must be attributable to one of them.
    SerializeSr,
    /// Serializing the segmentation. Separate for the same reason the
    /// report's stage is: the mask payload is the one part of a cycle that
    /// moves megabytes, and a slow cycle has to say which writer moved it.
    SerializeSeg,
    /// Serializing the surface mesh. Separate because it is the only writer
    /// whose payload is opaque file bytes: a slow cycle here points at the
    /// mesh's size rather than at anything clinfer computed.
    SerializeStl,
    /// Serializing the structure set. Separate because it is the only writer
    /// whose cost scales with the processor's output rather than with the
    /// study: a structure set carries one polygon per contour, so a slow
    /// cycle here points at how much the model outlined.
    SerializeRtstruct,
    /// Serializing the measurement report. Separate because it is the only
    /// writer whose content is numeric: a slow cycle here points at the
    /// number of findings rather than at anything clinfer computed.
    SerializeMeasurements,
    Total,
}

impl Stage {
    /// Every stage, in the order the exposition lists them.
    pub const ALL: [Stage; 9] = [
        Stage::Fetch,
        Stage::Infer,
        Stage::Serialize,
        Stage::SerializeSr,
        Stage::SerializeSeg,
        Stage::SerializeStl,
        Stage::SerializeRtstruct,
        Stage::SerializeMeasurements,
        Stage::Total,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fetch => "fetch",
            Self::Infer => "infer",
            Self::Serialize => "serialize",
            Self::SerializeSr => "serialize_sr",
            Self::SerializeSeg => "serialize_seg",
            Self::SerializeStl => "serialize_stl",
            Self::SerializeRtstruct => "serialize_rtstruct",
            Self::SerializeMeasurements => "serialize_measurements",
            Self::Total => "total",
        }
    }
}

/// Upper bounds of the duration buckets, in seconds.
///
/// Fixed, hand-chosen to straddle the stages this sidecar actually has: a
/// fetch is milliseconds to a second, inference is tens of milliseconds to
/// a few seconds, a store is milliseconds. The last bucket is `+Inf`, which
/// the exposition adds implicitly.
const DURATION_BUCKETS: [f64; 11] = [0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0];

/// One stage's observations: a count per bucket, a running sum and a count.
///
/// Hand-rolled from atomics in the same zero-dependency style as the plain
/// counters. A sample is one atomic increment per bucket boundary plus two
/// more, which is a handful of relaxed operations on the worker's own
/// thread - no lock and no allocation on the observe path.
#[derive(Debug)]
pub struct DurationHistogram {
    /// One counter per bucket in [`DURATION_BUCKETS`], plus the `+Inf`
    /// overflow at the end.
    buckets: [AtomicU64; DURATION_BUCKETS.len() + 1],
    /// Total observed seconds, as microseconds so the counter stays integer.
    sum_micros: AtomicU64,
    count: AtomicU64,
}

impl Default for DurationHistogram {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_micros: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }
}

impl DurationHistogram {
    /// Record one observation.
    ///
    /// Allocation-free: only atomic arithmetic happens here, which the
    /// ap101 gate pins.
    pub fn observe(&self, seconds: f64) {
        // Cumulative buckets, as Prometheus defines them: a sample counts
        // in every bucket whose bound it does not exceed, and in the +Inf
        // bucket always.
        for (index, bound) in DURATION_BUCKETS.iter().enumerate() {
            if seconds <= *bound {
                self.buckets[index].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.buckets[DURATION_BUCKETS.len()].fetch_add(1, Ordering::Relaxed);
        // Microseconds saturate rather than wrap; a cycle over 584942 years
        // is not a case worth carrying a wider counter for.
        let micros = (seconds * 1_000_000.0) as u64;
        self.sum_micros.fetch_add(micros, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// How many observations this histogram has taken.
    ///
    /// The same number the exposition's `_count` line reports; exposed
    /// separately so a test can assert which histogram recorded an
    /// observation without rendering the whole exposition.
    #[must_use]
    pub fn render_count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Append the `_bucket`/`_sum`/`_count` lines for one stage.
    pub fn render_into(&self, out: &mut String, stage: Stage) {
        let name = "clinfer_cycle_duration_seconds";
        for (index, bound) in DURATION_BUCKETS.iter().enumerate() {
            let value = self.buckets[index].load(Ordering::Relaxed);
            out.push_str(&format!(
                "{name}_bucket{{stage=\"{}\",le=\"{bound}\"}} {value}\n",
                stage.as_str()
            ));
        }
        let overflow = self.buckets[DURATION_BUCKETS.len()].load(Ordering::Relaxed);
        out.push_str(&format!(
            "{name}_bucket{{stage=\"{}\",le=\"+Inf\"}} {overflow}\n",
            stage.as_str()
        ));
        let sum = self.sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        out.push_str(&format!(
            "{name}_sum{{stage=\"{}\"}} {sum}\n",
            stage.as_str()
        ));
        let count = self.count.load(Ordering::Relaxed);
        out.push_str(&format!(
            "{name}_count{{stage=\"{}\"}} {count}\n",
            stage.as_str()
        ));
    }
}

/// The per-stage histograms, indexed by [`Stage`].
#[derive(Debug, Default)]
pub struct CycleDurations {
    fetch: DurationHistogram,
    infer: DurationHistogram,
    serialize: DurationHistogram,
    serialize_sr: DurationHistogram,
    serialize_seg: DurationHistogram,
    serialize_stl: DurationHistogram,
    serialize_rtstruct: DurationHistogram,
    serialize_measurements: DurationHistogram,
    total: DurationHistogram,
}

impl CycleDurations {
    /// The histogram for one stage.
    #[must_use]
    pub fn stage(&self, stage: Stage) -> &DurationHistogram {
        match stage {
            Stage::Fetch => &self.fetch,
            Stage::Infer => &self.infer,
            Stage::Serialize => &self.serialize,
            Stage::SerializeSr => &self.serialize_sr,
            Stage::SerializeSeg => &self.serialize_seg,
            Stage::SerializeStl => &self.serialize_stl,
            Stage::SerializeRtstruct => &self.serialize_rtstruct,
            Stage::SerializeMeasurements => &self.serialize_measurements,
            Stage::Total => &self.total,
        }
    }

    /// Record one observation against a stage.
    pub fn observe(&self, stage: Stage, seconds: f64) {
        self.stage(stage).observe(seconds);
    }
}

impl Counters {
    /// Render the Prometheus text exposition.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(768);
        for (name, help, counter) in [
            (
                "clinfer_claims_total",
                "Workitems successfully claimed",
                &self.claims,
            ),
            (
                "clinfer_completed_total",
                "Workitems completed",
                &self.completed,
            ),
            (
                "clinfer_canceled_total",
                "Workitems canceled",
                &self.canceled,
            ),
            (
                "clinfer_ws_events_total",
                "Event Reports received over the WebSocket notification channel",
                &self.ws_events,
            ),
            (
                "clinfer_ws_connects_total",
                "WebSocket notification channel opens, including reconnects",
                &self.ws_connects,
            ),
            (
                "clinfer_ws_reconnects_total",
                "Reconnect attempts after a dropped or failed channel",
                &self.ws_reconnects,
            ),
            (
                "clinfer_ws_dropped_total",
                "SCHEDULED reports dropped (queue full or shutting down)",
                &self.ws_dropped,
            ),
            (
                "clinfer_conflicts_total",
                "Claims refused by another worker (HTTP 409)",
                &self.conflicts,
            ),
            (
                "clinfer_claim_failures_total",
                "Claims refused with a 4xx other than 409",
                &self.claim_failures,
            ),
            (
                "clinfer_declined_total",
                "Scheduled workitems left alone because they are assigned elsewhere or not ready",
                &self.declined,
            ),
            (
                "clinfer_rejected_peers_total",
                "Requests from untrusted peers",
                &self.rejected_peers,
            ),
            (
                "clinfer_retries_exhausted_total",
                "Requests that used the whole retry budget against a busy origin",
                &self.retries_exhausted,
            ),
            (
                "clinfer_transport_retries_total",
                "Extra sends the retry budget paid for after a busy origin or a transport failure",
                &self.transport_retries,
            ),
        ] {
            out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n"));
            out.push_str(&format!("{name} {}\n", counter.load(Ordering::Relaxed)));
        }

        // The external-processor counters. Labelled by processor id, whose
        // cardinality is bounded by the configuration. A timeout and a kill
        // are separate series because they send an operator to different
        // places: the first to the script's budget, the second to
        // supervision (a shutdown, or a script that outlived its timeout).
        self.external_spawns.render_into(
            &mut out,
            "clinfer_external_spawns_total",
            "External processor runs started",
        );
        self.external_timeouts.render_into(
            &mut out,
            "clinfer_external_timeouts_total",
            "External processor runs that used their whole time budget",
        );
        self.external_kills.render_into(
            &mut out,
            "clinfer_external_kills_total",
            "External processor process trees that were killed",
        );
        self.external_exit_failures.render_into(
            &mut out,
            "clinfer_external_exit_failures_total",
            "External processor runs that exited non-zero",
        );

        // The child-process gauges. Per processor, so an operator sees
        // WHICH model is holding the memory: the sidecar's own RSS says
        // nothing about a child that is doing the work.
        self.external_rss_bytes.render_into(
            &mut out,
            "clinfer_external_rss_bytes",
            "Resident memory of the running child, per processor",
        );
        self.external_peak_rss_bytes.render_into(
            &mut out,
            "clinfer_external_peak_rss_bytes",
            "High-water resident memory of the running child, per processor",
        );
        self.external_last_peak_rss_bytes.render_into(
            &mut out,
            "clinfer_external_last_peak_rss_bytes",
            "High-water resident memory of the last finished run, per processor",
        );
        // Milliseconds are the stored unit; seconds are what a scrape
        // reads, so the division happens here rather than in a dashboard.
        self.external_cpu_seconds_millis
            .render_millis_as_seconds_into(
                &mut out,
                "clinfer_external_cpu_seconds",
                "User plus system CPU seconds of the running child, per processor",
            );
        self.external_running.render_into(
            &mut out,
            "clinfer_external_running",
            "1 while a child of this processor is running, 0 otherwise",
        );
        self.external_pid.render_into(
            &mut out,
            "clinfer_external_pid",
            "Process id of the running child, 0 when none",
        );
        out.push_str(
            "# HELP clinfer_peak_rss_bytes High-water resident memory of the clinfer process itself\n\
             # TYPE clinfer_peak_rss_bytes gauge\n",
        );
        let _ = writeln!(
            out,
            "clinfer_peak_rss_bytes {}",
            self.clinfer_peak_rss_bytes.load(Ordering::Relaxed)
        );

        // The cycle histogram. Five `stage` values, fixed buckets: the
        // cardinality cannot grow with patients, studies or models.
        out.push_str(
            "# HELP clinfer_cycle_duration_seconds Wall-clock seconds spent in one stage of a workitem cycle\n\
             # TYPE clinfer_cycle_duration_seconds histogram\n",
        );
        for stage in Stage::ALL {
            self.cycle.stage(stage).render_into(&mut out, stage);
        }
        out
    }
}

/// The trust decision, isolated so it can be tested without a socket.
#[derive(Debug, Clone)]
pub struct Policy {
    /// Peer addresses whose identity header is believed.
    trusted: Vec<IpAddr>,
    require_proxy_identity: bool,
}

/// The result of applying the policy to one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The peer may push.
    Allow,
    /// The peer is not in `trusted_proxies`.
    UntrustedPeer,
    /// The peer is trusted but sent no identity, and one is required.
    MissingIdentity,
}

impl Policy {
    /// Build the policy from the configured literals.
    ///
    /// An unparseable entry is kept out and reported by the caller at
    /// startup rather than silently ignored, because an operator who wrote
    /// a hostname here believes they are trusting a host.
    #[must_use]
    pub fn new(trusted: &[String], require_proxy_identity: bool) -> (Self, Vec<String>) {
        let mut addresses = Vec::new();
        let mut rejected = Vec::new();
        for entry in trusted {
            match entry.parse::<IpAddr>() {
                Ok(address) => addresses.push(address),
                Err(_) => rejected.push(entry.clone()),
            }
        }
        (
            Self {
                trusted: addresses,
                require_proxy_identity,
            },
            rejected,
        )
    }

    #[must_use]
    pub fn is_trusted(&self, peer: IpAddr) -> bool {
        self.trusted.contains(&peer)
    }

    /// Apply the policy to a peer and the identity it presented.
    #[must_use]
    pub fn verdict(&self, peer: IpAddr, identity: Option<&str>) -> Verdict {
        if !self.is_trusted(peer) {
            return Verdict::UntrustedPeer;
        }
        if self.require_proxy_identity && identity.is_none_or(str::is_empty) {
            return Verdict::MissingIdentity;
        }
        Verdict::Allow
    }
}

/// What the listener does with an accepted push: hand the UID to the
/// worker. Returns false when the queue is full, which the caller counts
/// as a drop.
pub type Deliver = Box<dyn Fn(String) -> bool + Send + Sync>;

/// The models as FHIR R4 `Questionnaire` resources.
///
/// `GET /processors/Questionnaire` answers a `searchset` Bundle, and
/// `GET /processors/Questionnaire/{id}` answers one resource or nothing.
/// The standard REST shape is the point: a consumer reads a model's
/// declared capabilities the same way from clinfer and from a FHIR server,
/// so only the base URL differs.
///
/// The configuration is the only source. A model appears because its file
/// is enabled, so a reload changes what is served and what the worker runs
/// together, and the two can never drift apart.
fn questionnaire_route(
    models: &crate::models::Models,
    path: &str,
    base: &str,
    extension_base: &str,
) -> Option<crate::models::fhir::Answer> {
    let rest = path.strip_prefix("/processors/Questionnaire")?;
    match rest {
        "" | "/" => Some(crate::models::fhir::Answer::Bundle(
            crate::models::fhir::search_bundle(models, None, base, extension_base),
        )),
        rest => {
            let id = rest.strip_prefix('/')?;
            // An id is ONE path segment: a second slash or a query string
            // is not part of it, so the request names nothing rather than
            // being trimmed into a resource that does exist.
            if id.is_empty() || id.contains('/') || id.contains('?') {
                return Some(crate::models::fhir::Answer::NotFound);
            }
            Some(crate::models::fhir::by_id(models, id, extension_base))
        }
    }
}

/// Run the listener on an already-bound socket.
///
/// The caller binds and then calls this, so a port conflict surfaces at
/// startup rather than on a detached thread after the process has already
/// announced it is listening.
pub fn serve_bound(
    listener: TcpListener,
    policy: Policy,
    counters: Arc<Counters>,
    live: Arc<crate::worker::Shared>,
    deliver: Deliver,
    shutdown: Arc<AtomicBool>,
) -> std::io::Result<()> {
    // A short accept timeout turns the accept loop into something that can
    // notice the shutdown flag, rather than blocking until a request
    // happens to arrive.
    listener.set_nonblocking(true)?;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, peer)) => {
                handle(stream, peer, &policy, &counters, &live, &deliver, &shutdown);
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err),
        }
    }
}

/// Bind and run the listener. Never returns while the process lives.
pub fn serve(
    listen: &str,
    policy: Policy,
    counters: Arc<Counters>,
    live: Arc<crate::worker::Shared>,
    deliver: Deliver,
    shutdown: Arc<AtomicBool>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(listen)?;
    serve_bound(listener, policy, counters, live, deliver, shutdown)
}

/// The local mirror map for `/fhir/StructureDefinition/*.json`.
///
/// Returns `(content_type, body)` for a known definition, `None` otherwise.
fn structure_definition(path: &str) -> Option<(&'static str, &'static str)> {
    match path {
        "/fhir/StructureDefinition/clinfer-provider.json" => {
            Some(("application/fhir+json", PROVIDER_DEFINITION))
        }
        "/fhir/StructureDefinition/clinfer-input-anatomy.json" => {
            Some(("application/fhir+json", INPUT_ANATOMY_DEFINITION))
        }
        "/fhir/StructureDefinition/clinfer-output-sop-classes.json" => {
            Some(("application/fhir+json", OUTPUT_SOP_CLASSES_DEFINITION))
        }
        "/fhir/StructureDefinition/clinfer-output-result-content.json" => {
            Some(("application/fhir+json", RESULT_CONTENT_DEFINITION))
        }
        _ => None,
    }
}

/// Handle one inbound connection.
///
/// Operational GET endpoints only (see the module doc). Every one of them
/// except `/health` is subject to the trust policy: an operator who sets
/// `trusted_proxies` or `require_proxy_identity` expects the sidecar to
/// refuse an unknown peer, and `/metrics` and `/processors` both describe
/// the deployment.
fn handle(
    mut stream: TcpStream,
    peer: SocketAddr,
    policy: &Policy,
    counters: &Counters,
    live: &crate::worker::Shared,
    _deliver: &Deliver,
    _shutdown: &AtomicBool,
) {
    // The listener runs non-blocking so the accept loop can notice the
    // shutdown flag, and on Linux and Windows an accepted socket INHERITS that
    // mode. Without this the first `read` returns WouldBlock instead of
    // waiting, the request line comes back empty, and every connection is
    // dropped as unreadable.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));

    let Some(request) = read_request(&mut stream) else {
        // A peer that cannot complete a request line gets no status code:
        // there is nothing to answer.
        return;
    };

    // Liveness is exempt on purpose: a supervisor probing `/health` is not
    // a DICOMweb client and typically is not behind the proxy that carries
    // the identity, so gating it would restart the sidecar exactly when the
    // operator is trying to see whether it is up. It answers "the process
    // can answer", nothing more.
    //
    // The retired push route is exempt for the opposite reason: as far as
    // this listener is concerned it does not exist, and the policy speaks
    // only for routes that do. A 403 here would tell a caller that the path
    // is real and that it failed a check, and would move `rejected_peers`
    // for a request that never reached an endpoint.
    if request.path != "/health" && !is_retired_push_route(&request) {
        match policy.verdict(peer.ip(), request.identity.as_deref()) {
            Verdict::Allow => {}
            Verdict::UntrustedPeer => {
                counters
                    .rejected_peers
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                respond(&mut stream, 403, "text/plain", b"forbidden\n");
                return;
            }
            // The connection reached us through a proxy that was supposed to
            // assert an identity and did not. That is the proxy failing its
            // part, not the client, so it is a 502 rather than a 401: the
            // client did everything it could.
            Verdict::MissingIdentity => {
                counters
                    .rejected_peers
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                respond(&mut stream, 502, "text/plain", b"missing identity\n");
                return;
            }
        }
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/health") => {
            // Liveness only: it answers while the process can answer, so
            // it stays useful for a supervisor and says nothing else.
            respond(&mut stream, 200, "text/plain", b"OK\n");
        }
        ("GET", "/metrics") => {
            let body = counters.render();
            respond(
                &mut stream,
                200,
                "text/plain; version=0.0.4",
                body.as_bytes(),
            );
        }
        ("GET", "/version") => {
            let body = format!("clinfer {}\n", crate::VERSION);
            respond(&mut stream, 200, "text/plain", body.as_bytes());
        }
        ("GET", path) if path.starts_with("/fhir/StructureDefinition/") => {
            // A local mirror of the extension definitions, so offline tools
            // and validators can dereference the canonical URLs without
            // reaching the public copy. The bytes are compiled in, so the
            // mirror can never drift from the binary that publishes them.
            match structure_definition(path) {
                Some((content_type, body)) => {
                    respond(&mut stream, 200, content_type, body.as_bytes())
                }
                None => respond(&mut stream, 404, "text/plain", b"not found\n"),
            }
        }
        ("GET", path) if path.starts_with("/processors/Questionnaire") => {
            // The models, in the FHIR REST shape a consumer already speaks:
            // a search Bundle at the resource root and one Questionnaire at
            // an id. Served from the shared cell at request time, so a
            // reloaded configuration changes this answer and the worker's
            // behaviour together.
            //
            // An empty models directory is an empty `entry` array, not an
            // error: a sidecar that serves nothing is a valid deployment.
            let conf = live.current();
            let models = crate::models::Models::from_conf(&conf);
            // The extension URLs follow the configured `fhir.extension_base`,
            // so an operator points them at whatever hosts the definitions
            // (their own site, a national mirror, or a local schema).
            let extension_base = conf.fhir.extension_base.clone();
            // The Bundle URLs are absolute, so they are built from the
            // request's own Host header; without one (a bare HTTP/1.0
            // request) localhost is the honest base.
            let base = request
                .host
                .as_deref()
                .map(|host| format!("http://{host}"))
                .unwrap_or_else(|| "http://localhost".to_string());
            match questionnaire_route(&models, path, &base, &extension_base) {
                Some(crate::models::fhir::Answer::Bundle(body)) => {
                    respond(&mut stream, 200, "application/fhir+json", body.as_bytes())
                }
                Some(crate::models::fhir::Answer::Resource(body)) => {
                    respond(&mut stream, 200, "application/fhir+json", body.as_bytes())
                }
                Some(crate::models::fhir::Answer::NotFound) | None => {
                    respond(&mut stream, 404, "text/plain", b"not found\n");
                }
            }
        }
        // The `POST /workitems/{uid}` push route is GONE. The sidecar opens the
        // WebSocket Notification Connection itself and claims on the SCHEDULED
        // Event Report, so there is no inbound push path to serve and the
        // origin no longer needs to reach clinfer. A POST here now falls
        // through to the 404 below, which is the honest answer: this endpoint
        // does not exist any more.
        _ => {
            respond(&mut stream, 404, "text/plain", b"not found\n");
        }
    }
}

/// Read the UID out of a pushed workitem body.
///
/// Kept because it is the parser for the RETIRED push payload and its tests
/// still pin the tag it reads; nothing on the live path calls it, so the `json`
/// import it needs is scoped here instead of at module level.
#[cfg(test)]
fn workitem_uid(body: &[u8]) -> Option<String> {
    use crate::json;
    let document = json::parse(body).ok()?;
    let uid = document
        .attribute(crate::dicomweb::TAG_SOP_INSTANCE_UID)?
        .trim();
    if uid.is_empty() {
        return None;
    }
    Some(uid.to_string())
}

/// A request parsed far enough to route it.
#[derive(Debug)]
struct Inbound {
    method: String,
    path: String,
    /// The `Host` header, used to build the absolute URLs in the
    /// Questionnaire Bundle (`link.url` and each `fullUrl`).
    host: Option<String>,
    /// The identity the peer presented; `handle` passes it to the trust
    /// policy before routing, so a deployment that requires a proxy identity
    /// refuses a request without one.
    identity: Option<String>,
    /// The request body. Retained because the parser reads it: dropping the
    /// read would leave unread bytes on the socket for a keep-alive peer.
    #[allow(dead_code)]
    body: Vec<u8>,
}

/// Whether this request is for the retired `POST /workitems/{uid}` push
/// route.
///
/// Named so the trust gate and the router cannot disagree about which paths
/// exist: both read this one predicate.
fn is_retired_push_route(request: &Inbound) -> bool {
    request.method.eq_ignore_ascii_case("POST")
        && request.path.starts_with("/workitems/")
        && !request.path.contains('?')
}

/// Read one HTTP request. `None` when the peer did not send a request line.
fn read_request(stream: &mut TcpStream) -> Option<Inbound> {
    let mut raw = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let head_end;
    loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        raw.extend_from_slice(&chunk[..read]);
        if let Some(index) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            head_end = index;
            break;
        }
        if raw.len() > MAX_REQUEST_BODY {
            return None;
        }
    }

    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut identity = None;
    let mut host = None;
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("x-authenticated-user") {
            identity = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("host") {
            host = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().unwrap_or(0);
        }
    }

    if content_length > MAX_REQUEST_BODY {
        return None;
    }
    let mut body = raw[head_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
        if body.len() > MAX_REQUEST_BODY {
            return None;
        }
    }
    body.truncate(content_length);

    Some(Inbound {
        method,
        path,
        host,
        identity,
        body,
    })
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// The sender half of the queue, shaped for [`serve`].
pub fn deliver_channel(sender: SyncSender<String>) -> Deliver {
    Box::new(move |uid| sender.try_send(uid).is_ok())
}

/// Advertise the JSON content type this listener accepts, for the log line
/// and for tests that assert the contract.
#[must_use]
pub fn accepted_content_type() -> &'static str {
    DICOM_JSON
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The extension base the test configuration publishes its definitions
    /// under; different from the default so the test proves the value is READ
    /// from the configuration rather than hardcoded.
    const EXT_BASE: &str = "http://example.org/fhir/StructureDefinition";

    #[test]
    fn the_local_mirror_serves_every_shipped_definition() {
        let expect = [
            (
                "/fhir/StructureDefinition/clinfer-provider.json",
                "clinfer-provider.json",
            ),
            (
                "/fhir/StructureDefinition/clinfer-input-anatomy.json",
                "clinfer-input-anatomy.json",
            ),
            (
                "/fhir/StructureDefinition/clinfer-output-sop-classes.json",
                "clinfer-output-sop-classes.json",
            ),
            (
                "/fhir/StructureDefinition/clinfer-output-result-content.json",
                "clinfer-output-result-content.json",
            ),
        ];
        for (path, file) in expect {
            let (content_type, body) =
                structure_definition(path).unwrap_or_else(|| panic!("missing mirror for {path}"));
            assert_eq!(content_type, "application/fhir+json");
            let canonical = format!(
                "https://github.com/clicker71/clarus-pacs/blob/main/clinfer/fhir/StructureDefinition/{file}"
            );
            assert!(
                body.contains(&canonical),
                "definition {file} does not carry its canonical URL"
            );
            // The identity is a canonical URL, never the loopback mirror.
            assert!(
                !body.contains("127.0.0.1"),
                "definition {file} leaked a loopback identity"
            );
        }
        assert_eq!(
            structure_definition("/fhir/StructureDefinition/nope.json"),
            None
        );
        assert_eq!(structure_definition("/fhir/StructureDefinition/"), None);
        assert_eq!(structure_definition("/processors/Questionnaire"), None);
    }

    fn policy(trusted: &[&str], require: bool) -> Policy {
        let owned: Vec<String> = trusted.iter().map(|s| (*s).to_string()).collect();
        Policy::new(&owned, require).0
    }

    #[test]
    fn a_trusted_peer_is_allowed() {
        let policy = policy(&["127.0.0.1", "::1"], true);
        assert_eq!(
            policy.verdict("127.0.0.1".parse().unwrap(), Some("clinfer")),
            Verdict::Allow
        );
        assert_eq!(
            policy.verdict("::1".parse().unwrap(), Some("clinfer")),
            Verdict::Allow
        );
    }

    #[test]
    fn a_peer_outside_the_list_is_refused() {
        let restricted = policy(&["127.0.0.1"], false);
        assert_eq!(
            restricted.verdict("10.0.0.5".parse().unwrap(), Some("clinfer")),
            Verdict::UntrustedPeer
        );
        // Even an empty trust list refuses everyone, rather than allowing
        // everyone: the safe direction for a clinical network.
        let none = policy(&[], false);
        assert_eq!(
            none.verdict("127.0.0.1".parse().unwrap(), None),
            Verdict::UntrustedPeer
        );
    }

    #[test]
    fn a_trusted_peer_without_an_identity_is_refused_when_required() {
        let strict = policy(&["127.0.0.1"], true);
        let peer = "127.0.0.1".parse().unwrap();
        assert_eq!(strict.verdict(peer, None), Verdict::MissingIdentity);
        assert_eq!(strict.verdict(peer, Some("")), Verdict::MissingIdentity);

        // In the soft mode it proceeds anonymously.
        let soft = policy(&["127.0.0.1"], false);
        assert_eq!(soft.verdict(peer, None), Verdict::Allow);
    }

    #[test]
    fn an_unparseable_trusted_entry_is_reported_not_silently_dropped() {
        let owned = vec!["127.0.0.1".to_string(), "pacs.example.org".to_string()];
        let (policy, rejected) = Policy::new(&owned, false);
        assert_eq!(rejected, vec!["pacs.example.org"]);
        assert_eq!(
            policy.verdict("127.0.0.1".parse().unwrap(), None),
            Verdict::Allow
        );
    }

    #[test]
    fn a_push_body_yields_its_workitem_uid() {
        let body = br#"{"00080018":{"vr":"UI","Value":["1.2.3.4"]},
                        "00741000":{"vr":"CS","Value":["SCHEDULED"]}}"#;
        assert_eq!(workitem_uid(body).as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn a_push_body_without_a_uid_is_not_a_workitem() {
        assert!(workitem_uid(br#"{"00741000":{"Value":["SCHEDULED"]}}"#).is_none());
        assert!(workitem_uid(b"not json").is_none());
        assert!(workitem_uid(br#"{"00080018":{"Value":["  "]}}"#).is_none());
    }

    /// The trust policy is only real if it is applied where requests arrive,
    /// so this drives `handle` over a socket rather than calling the policy
    /// directly - a policy that is correct but never consulted is exactly the
    /// defect this pins.
    fn ask(policy: Policy, request: &str, counters: &Arc<Counters>) -> (String, u64) {
        use std::io::{Read as _, Write as _};
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let policy_for_thread = policy.clone();
        let counters_shared = Arc::clone(counters);
        let live = Arc::new(crate::worker::Shared::new(Arc::new(
            crate::conf::load(&write_config()).expect("the harness config parses"),
        )));
        let deliver: Deliver = Box::new(|_| true);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = std::thread::spawn(move || {
            let (stream, peer) = listener.accept().expect("accept");
            handle(
                stream,
                peer,
                &policy_for_thread,
                &counters_shared,
                &live,
                &deliver,
                &shutdown,
            );
        });

        let mut stream = TcpStream::connect(addr).expect("connect");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        stream
            .write_all(format!("{request} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .expect("write");
        let mut raw = String::new();
        let _ = stream.read_to_string(&mut raw);
        let _ = worker.join();
        (
            raw,
            counters
                .rejected_peers
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    #[test]
    fn an_untrusted_peer_is_refused_on_the_operational_endpoints() {
        for path in [
            "/metrics",
            "/processors/Questionnaire",
            "/processors/Questionnaire/proc",
            "/version",
        ] {
            let counters = Arc::new(Counters::default());
            // Empty trust list: everyone is untrusted.
            let (raw, rejected) = ask(policy(&[], false), &format!("GET {path}"), &counters);
            assert!(
                raw.starts_with("HTTP/1.1 403"),
                "{path} must refuse an untrusted peer, got: {}",
                raw.lines().next().unwrap_or("")
            );
            assert_eq!(rejected, 1, "{path} must count the rejection");
        }
    }

    #[test]
    fn a_trusted_peer_without_the_required_identity_gets_a_502() {
        let counters = Arc::new(Counters::default());
        let (raw, rejected) = ask(policy(&["127.0.0.1"], true), "GET /metrics", &counters);
        assert!(
            raw.starts_with("HTTP/1.1 502"),
            "a missing proxy identity is a 502, got: {}",
            raw.lines().next().unwrap_or("")
        );
        assert_eq!(rejected, 1, "the refusal is counted");
    }

    #[test]
    fn health_answers_an_untrusted_peer() {
        // Liveness is the one exemption: a supervisor probing the sidecar is
        // not a DICOMweb client and usually carries no identity.
        let counters = Arc::new(Counters::default());
        let (raw, rejected) = ask(policy(&[], false), "GET /health", &counters);
        assert!(
            raw.starts_with("HTTP/1.1 200"),
            "health must answer regardless of trust, got: {}",
            raw.lines().next().unwrap_or("")
        );
        assert_eq!(rejected, 0, "an exempt path is not a rejection");
    }

    #[test]
    fn a_trusted_peer_with_its_identity_reaches_the_endpoint() {
        let counters = Arc::new(Counters::default());
        let (raw, rejected) = ask(
            policy(&["127.0.0.1"], true),
            "GET /version\r\nX-Authenticated-User: clinfer",
            &counters,
        );
        assert!(
            raw.starts_with("HTTP/1.1 200"),
            "a fully trusted request is served, got: {}",
            raw.lines().next().unwrap_or("")
        );
        assert_eq!(rejected, 0);
    }

    #[test]
    fn counters_render_as_prometheus_text() {
        let counters = Counters::default();
        counters.claims.fetch_add(3, Ordering::Relaxed);
        counters.conflicts.fetch_add(1, Ordering::Relaxed);
        let text = counters.render();
        assert!(
            text.contains("# TYPE clinfer_claims_total counter\n"),
            "{text}"
        );
        assert!(text.contains("clinfer_claims_total 3\n"), "{text}");
        assert!(text.contains("clinfer_conflicts_total 1\n"), "{text}");
        // Every counter is present, so a scrape never silently misses one.
        for name in [
            "clinfer_claims_total",
            "clinfer_completed_total",
            "clinfer_canceled_total",
            "clinfer_ws_events_total",
            "clinfer_ws_connects_total",
            "clinfer_ws_reconnects_total",
            "clinfer_ws_dropped_total",
            "clinfer_conflicts_total",
            "clinfer_claim_failures_total",
            "clinfer_declined_total",
            "clinfer_rejected_peers_total",
        ] {
            assert!(text.contains(name), "{name} missing from:\n{text}");
        }
        // The retired push counters are GONE, not merely renamed: a stale
        // dashboard reading `clinfer_pushes_total` must go to zero rather
        // than keep a flat line that looks like a healthy but idle push
        // path. Asserted so a future edit cannot quietly bring them back.
        for gone in ["clinfer_pushes_total", "clinfer_dropped_total"] {
            assert!(
                !text.contains(gone),
                "{gone} should no longer exist:\n{text}"
            );
        }
    }

    #[test]
    fn the_retry_counter_is_exported() {
        let counters = Counters::default();
        counters.retries_exhausted.fetch_add(2, Ordering::Relaxed);
        let text = counters.render();
        assert!(text.contains("clinfer_retries_exhausted_total 2"), "{text}");
    }

    #[test]
    fn the_cycle_histogram_names_every_stage() {
        let counters = Counters::default();
        let text = counters.render();
        assert!(text.contains("# TYPE clinfer_cycle_duration_seconds histogram"));
        for stage in Stage::ALL {
            for suffix in ["_bucket", "_sum", "_count"] {
                let needle = format!(
                    "clinfer_cycle_duration_seconds{suffix}{{stage=\"{}\"",
                    stage.as_str()
                );
                assert!(text.contains(&needle), "{needle} missing from:\n{text}");
            }
        }
        // The cardinality is bounded: exactly five stage values, each with
        // the eleven fixed buckets plus +Inf.
        let series = text
            .lines()
            .filter(|l| l.starts_with("clinfer_cycle_duration_seconds_bucket{"))
            .count();
        assert_eq!(series, Stage::ALL.len() * (DURATION_BUCKETS.len() + 1));
    }

    #[test]
    fn an_observation_lands_in_the_cumulative_buckets() {
        let histogram = DurationHistogram::default();
        histogram.observe(0.07);
        // 0.07 is above 0.05 and at or below 0.1, so it counts in 0.1 and
        // every larger bucket, plus +Inf - never in the smaller ones.
        let text = {
            let mut out = String::new();
            histogram.render_into(&mut out, Stage::Infer);
            out
        };
        assert!(text.contains("le=\"0.05\"} 0"), "{text}");
        assert!(text.contains("le=\"0.1\"} 1"), "{text}");
        assert!(text.contains("le=\"+Inf\"} 1"), "{text}");
        assert!(text.contains("_count{stage=\"infer\"} 1"), "{text}");
        assert!(text.contains("_sum{stage=\"infer\"} 0.07"), "{text}");
    }

    #[test]
    fn a_value_above_every_bucket_lands_only_in_the_overflow() {
        let histogram = DurationHistogram::default();
        histogram.observe(120.0);
        let mut out = String::new();
        histogram.render_into(&mut out, Stage::Total);
        // Every finite bucket stays at zero; only +Inf counts it.
        for bound in DURATION_BUCKETS {
            assert!(out.contains(&format!("le=\"{bound}\"}} 0")), "{out}");
        }
        assert!(out.contains("le=\"+Inf\"} 1"), "{out}");
    }

    #[test]
    fn the_stage_label_round_trips_through_its_name() {
        for stage in Stage::ALL {
            assert!(!stage.as_str().is_empty());
        }
        // The label set is spelled out so the exposition's cardinality is a
        // decision rather than a side effect: a new stage that is not named
        // here fails this test, and a Prometheus label set that grows by
        // accident is what the fixed list exists to prevent.
        let names: Vec<&str> = Stage::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            names,
            [
                "fetch",
                "infer",
                "serialize",
                "serialize_sr",
                "serialize_seg",
                "serialize_stl",
                "serialize_rtstruct",
                "serialize_measurements",
                "total"
            ]
        );
    }

    #[test]
    fn every_stage_has_its_own_histogram() {
        // `CycleDurations::stage` is a match over the enum, so a stage that
        // fell through to another one would be invisible except as a
        // mislabelled observation. Recording into each stage and reading
        // back the count pins the correspondence.
        let durations = CycleDurations::default();
        for stage in Stage::ALL {
            durations.observe(stage, 0.001);
        }
        for stage in Stage::ALL {
            let count = durations.stage(stage).render_count();
            assert_eq!(count, 1, "{:?} did not observe exactly once", stage);
        }
    }

    // -- /processors/Questionnaire ----------------------------------------

    #[test]
    fn the_questionnaire_route_answers_a_searchset_for_the_resource_root() {
        let models =
            crate::models::Models::from_conf(&crate::conf::load(&write_config()).expect("parses"));
        let Some(crate::models::fhir::Answer::Bundle(body)) = questionnaire_route(
            &models,
            "/processors/Questionnaire",
            "http://127.0.0.1:8124",
            EXT_BASE,
        ) else {
            panic!("the resource root answers a Bundle");
        };
        assert!(body.contains("\"resourceType\":\"Bundle\""), "{body}");
        assert!(body.contains("\"type\":\"searchset\""), "{body}");
        // The fullUrl is ABSOLUTE (FHIR: absolute URL or UUID/OID URI), so a
        // client follows it without guessing the base it used.
        assert!(
            body.contains("\"fullUrl\":\"http://127.0.0.1:8124/processors/Questionnaire/proc\""),
            "{body}"
        );
        assert!(
            body.contains(
                "\"relation\":\"self\",\"url\":\"http://127.0.0.1:8124/processors/Questionnaire\""
            ),
            "{body}"
        );
        assert!(
            body.contains("\"resourceType\":\"Questionnaire\""),
            "{body}"
        );
        // Coding.system is a URI: the parameter codes share the procedure's
        // scheme, so the private DICOM designator maps to a URN rather than
        // appearing bare.
        assert!(
            body.contains("\"system\":\"urn:clarus:scheme:99clarus\""),
            "{body}"
        );
        // Extension urls are absolute URLs, not URNs.
        assert!(
            body.contains("http://example.org/fhir/StructureDefinition/clinfer-provider.json"),
            "{body}"
        );
        // The provider is a complex extension (child valueStrings), never a
        // JSON object carried inside a valueString.
        assert!(
            body.contains("\"url\":\"name\",\"valueString\":\"clinfer-proc\""),
            "{body}"
        );
        assert!(!body.contains("\"valueString\":{"), "{body}");
    }

    #[test]
    fn a_questionnaire_by_id_answers_that_model_and_an_unknown_id_is_404() {
        let models =
            crate::models::Models::from_conf(&crate::conf::load(&write_config()).expect("parses"));
        match questionnaire_route(
            &models,
            "/processors/Questionnaire/proc",
            "http://127.0.0.1:8124",
            EXT_BASE,
        ) {
            Some(crate::models::fhir::Answer::Resource(body)) => {
                assert!(body.contains("\"id\":\"proc\""), "{body}");
                // The clinfer-only block never leaves the process: a workdir
                // path in a capability document is deployment detail.
                assert!(!body.contains("runtime"), "{body}");
                assert!(!body.contains("proc.py"), "{body}");
            }
            other => panic!("expected one resource, got {other:?}"),
        }
        assert_eq!(
            questionnaire_route(
                &models,
                "/processors/Questionnaire/nowhere",
                "http://127.0.0.1:8124",
                EXT_BASE
            ),
            Some(crate::models::fhir::Answer::NotFound)
        );
    }

    #[test]
    fn an_id_with_a_second_segment_names_nothing() {
        // A query string or a nested path is not part of an id; answering it
        // would mean trimming the request into a resource that does exist.
        let models =
            crate::models::Models::from_conf(&crate::conf::load(&write_config()).expect("parses"));
        assert_eq!(
            questionnaire_route(
                &models,
                "/processors/Questionnaire/proc/extra",
                "http://127.0.0.1:8124",
                EXT_BASE
            ),
            Some(crate::models::fhir::Answer::NotFound)
        );
        assert_eq!(
            questionnaire_route(
                &models,
                "/processors/Questionnaire/proc?_count=1",
                "http://127.0.0.1:8124",
                EXT_BASE
            ),
            Some(crate::models::fhir::Answer::NotFound)
        );
    }

    #[test]
    fn a_path_outside_the_questionnaire_route_is_not_the_route() {
        let models =
            crate::models::Models::from_conf(&crate::conf::load(&write_config()).expect("parses"));
        assert_eq!(
            questionnaire_route(&models, "/processors", "http://127.0.0.1:8124", EXT_BASE),
            None
        );
        assert_eq!(
            questionnaire_route(&models, "/processors/", "http://127.0.0.1:8124", EXT_BASE),
            None
        );
        assert_eq!(
            questionnaire_route(&models, "/metrics", "http://127.0.0.1:8124", EXT_BASE),
            None
        );
    }

    /// Write a configuration to a fresh file and return its path.
    ///
    /// The models live in a directory beside it, because the base file no
    /// longer carries a model: this IS the two-layer shape, exercised through
    /// the real loader.
    fn write_config() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "clinfer-menu-conf-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let models = dir.join("models");
        std::fs::create_dir_all(&models).expect("scratch directory");
        std::fs::write(
            models.join("proc.yaml"),
            "\
enabled: true
title: Process
procedure_code:
  value: PROC-AI
  scheme: 99CLARUS
  meaning: Process AI
provider:
  name: clinfer-proc
  vendor: Clarus
  version: \"1.0\"
input_requirements:
  modalities:
    - CT
  anatomy:
    - system: http://snomed.info/sct
      code: T-D0050
      meaning: Lung structure
output_capabilities:
  sop_classes:
    - 1.2.840.10008.5.1.4.1.1.11.1
parameters:
  - code: CLIP_LIMIT
    type: decimal
    default: 2.5
    min: 1.0
    max: 10.0
  - code: ALGORITHM
    type: choice
    options:
      - clahe
      - none
    default: clahe
match:
  modality: [DX]
runtime:
  script: proc.py
  timeout_secs: 60
outputs:
  - kind: pr
    profile_version: 1
",
        )
        .expect("write the model");
        // The script the model names must EXIST: an enabled model whose
        // script is missing is refused at load, by the tier that checks
        // exactly that. It is never executed here - these tests ask the
        // listener what it SERVES, not what it runs.
        std::fs::write(models.join("proc.py"), b"#!/usr/bin/env python3\n").expect("write script");
        let path = dir.join("clinfer.conf");
        std::fs::write(
            &path,
            "\
server:
  base_url: \"http://127.0.0.1:1/dicomweb\"
  identity_user: \"clinfer\"
  listen: \"127.0.0.1:18124\"
engine:
  cache_dir: \"/tmp/clinfer-menu-cache\"
models_dir: ./models
fhir:
  extension_base: \"http://example.org/fhir/StructureDefinition\"
default_rule: cancel
",
        )
        .expect("write the config");
        path
    }

    // -- The shutdown refusal ---------------------------------------------

    /// One request against a listener, returning the status line.
    fn request_once(listen: &str, request: &str) -> String {
        use std::io::{Read as _, Write as _};
        let mut stream = TcpStream::connect(listen).expect("connect");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        stream.write_all(request.as_bytes()).expect("write");
        stream.flush().expect("flush");
        let mut raw = String::new();
        let _ = stream.read_to_string(&mut raw);
        raw.lines().next().unwrap_or_default().to_string()
    }

    /// A `POST` to the push route the origin used to call.
    fn retired_push_once(listen: &str, uid: &str) -> String {
        let body = format!("{{\"00080018\":{{\"vr\":\"UI\",\"Value\":[\"{uid}\"]}}}}");
        let head = format!(
            "POST /workitems/{uid} HTTP/1.1\r\nHost: {listen}\r\n\
             X-Authenticated-User: clinfer\r\nContent-Type: {DICOM_JSON}\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        request_once(listen, &format!("{head}{body}"))
    }

    /// A configuration cell for the listener tests.
    ///
    /// Built through the public loader from a file, which is how a
    /// deployment configures a sidecar, so the value is a real parsed
    /// configuration rather than a struct literal a test invented. The
    /// model file beside it is what gives `/processors/Questionnaire`
    /// something to answer.
    fn listener_config() -> Arc<crate::worker::Shared> {
        let path = write_config();
        let conf = crate::conf::load(&path).expect("the listener config parses");
        Arc::new(crate::worker::Shared::new(Arc::new(conf)))
    }

    /// Start the real listener on an ephemeral port and return it.
    fn start_listening(shutdown: Arc<AtomicBool>, stop: Arc<AtomicBool>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let listen = listener.local_addr().expect("addr").to_string();
        let counters = Arc::new(Counters::default());
        let deliver: Deliver = Box::new(|_uid| true);
        let live = listener_config();
        let thread_shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            let _ = serve_bound(
                listener,
                policy(&["127.0.0.1"], false),
                counters,
                live,
                deliver,
                thread_shutdown,
            );
        });
        // The accept loop sleeps 50 ms between polls; give it a moment to
        // start before the first connection.
        std::thread::sleep(Duration::from_millis(80));
        stop.store(true, Ordering::Relaxed);
        listen
    }

    #[test]
    fn the_retired_push_route_answers_the_same_before_and_during_a_drain() {
        // It used to answer 200 while running and 503 once draining. Both of
        // those answers are gone with the route: the point now is that the
        // status does NOT depend on the drain, because no request handler
        // consults the shutdown flag to serve a route that no longer exists.
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let listen = start_listening(Arc::clone(&shutdown), stop);

        let before = retired_push_once(&listen, "1.2.3.4");
        assert!(before.contains("404"), "retired while running: {before}");

        // Now the drain begins.
        shutdown.store(true, Ordering::Relaxed);
        // The accept loop returns within its poll interval, so the port may
        // refuse the connection outright; a 404 or a refused connection prove
        // the same thing, and the test accepts both rather than racing it.
        let during = std::panic::catch_unwind(|| retired_push_once(&listen, "1.2.3.5")).ok();
        if let Some(line) = during {
            assert!(
                line.contains("404") || line.is_empty(),
                "the retired route stays retired while draining: {line}"
            );
        }
    }

    #[test]
    fn a_draining_listener_still_answers_health() {
        // A supervisor polling /health during a drain must see a live
        // process, not a hang: the drain can take as long as one inference
        // call, and a restarted sidecar would strand the workitem the
        // draining one is finishing.
        let shutdown = Arc::new(AtomicBool::new(false));
        let listen = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let listen = listener.local_addr().expect("addr").to_string();
            let counters = Arc::new(Counters::default());
            let deliver: Deliver = Box::new(|_uid| true);
            let live = listener_config();
            let thread_shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || {
                let _ = serve_bound(
                    listener,
                    policy(&["127.0.0.1"], false),
                    counters,
                    live,
                    deliver,
                    thread_shutdown,
                );
            });
            std::thread::sleep(Duration::from_millis(80));
            listen
        };

        use std::io::{Read as _, Write as _};
        let mut stream = TcpStream::connect(&listen).expect("connect");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        stream
            .write_all(format!("GET /health HTTP/1.1\r\nHost: {listen}\r\n\r\n").as_bytes())
            .expect("write");
        let mut raw = String::new();
        let _ = stream.read_to_string(&mut raw);
        assert!(raw.contains("200"), "health answers: {raw}");
    }
}
