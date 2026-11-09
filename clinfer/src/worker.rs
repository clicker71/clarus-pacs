// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The worker: one coordinator thread, plus one thread per model.
//!
//! A workitem is a study, an inference is the expensive part, and the
//! origin's own claim is what arbitrates between several sidecars - so the
//! CLAIM stays on one thread and one thread only. What is parallel is the
//! part that waits on a child process: each configured processor owns a
//! thread of its own with a channel of its own, so two different models
//! work at the same time while any ONE model stays strictly serial.
//!
//! The ceiling on how many children may run at once is `engine.workers`
//! (`conf::Conf::workers`), enforced by a counting semaphore the model
//! threads acquire AFTER they take a job - so an idle thread consumes
//! nothing and the knob lowers concurrency without preempting a run.
//!
//! Durability does not rest on any of this. A push is a wake-up, and the
//! startup catch-up search is what guarantees every SCHEDULED workitem is
//! eventually seen, so a crash between accepting a push and claiming it
//! loses latency, not work.
//!
//! # Every wait is bounded
//!
//! The watchdog bounds a child, `wait_timeout` bounds the semaphore, and
//! the per-workitem completion channel is sized to the number of jobs
//! dispatched, so a model thread never blocks on its result. A single
//! unbounded receive anywhere would turn a shutdown into a hang, which is
//! the failure this shape exists to make impossible.

use crate::conf::Conf;
use crate::dicomweb::{
    Dicomweb, OutputRef, TAG_ACCESSION_NUMBER, TAG_BODY_PART, TAG_CODE_MEANING,
    TAG_MODALITIES_IN_STUDY, TAG_MODALITY, TAG_PATIENT_BIRTH_DATE, TAG_PATIENT_ID,
    TAG_PATIENT_NAME, TAG_PATIENT_SEX, TAG_REFERRING_PHYSICIAN, TAG_SOP_INSTANCE_UID,
    TAG_STUDY_DESCRIPTION, TAG_STUDY_ID, WorkItem,
};
use crate::matcher::StudyAttrs;
use crate::measurements_writer;
use crate::pixels::Geometry;
use crate::pr;
use crate::receiver::{Counters, Stage};
use crate::seg;
use crate::sr;
use crate::stl as stl_encoder;
use crate::ups::{self, Action, State};
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

/// How long the worker waits for a push before looking around.
///
/// Not a polling interval for work: the catch-up search runs once at
/// startup, and everything after that arrives as a push. The timeout
/// exists so the thread can notice the shutdown flag.
const IDLE_TIMEOUT: Duration = Duration::from_millis(500);

/// How many jobs one model's queue holds, beyond the one it is running.
///
/// ONE. Seriality per model is what makes the concurrency analysis finite:
/// a model runs exactly one child at a time, so its memory and its GPU are
/// what the configuration says they are. A deeper queue would also let one
/// claim park a long run behind a model while the coordinator holds the
/// workitem - the latency this shape exists to remove - and it would extend
/// the drain bound by the depth.
pub const MODEL_QUEUE_DEPTH: usize = 1;

/// How many consecutive instance fetches may spend their whole retry budget
/// on transport failures before the fetch gives up on the origin.
///
/// The fetch is one GET per instance, so a dead endpoint costs one whole
/// retry budget per instance: without a bound, a study of several hundred
/// instances is several hundred budgets of waiting for an answer that is not
/// coming. Three is the smallest run that cannot be a single dropped
/// connection - [`crate::http::MAX_ATTEMPTS`] already rode out the transient
/// case - so reaching it means the endpoint itself is unreachable, and the
/// run reports that instead of grinding on.
const FETCH_FAILURE_LIMIT: u32 = 3;

/// The reason a rule fails when its model has no thread to run on.
///
/// Two situations reach here and both are honest per-rule failures rather
/// than half-delivered studies:
///
/// - the rule names a model the RELOAD added - the thread set is fixed at
///   startup, so a reload changes rules and matching, not threads;
/// - the model's thread is GONE (it panicked). The message cannot tell the
///   two apart from here, so it names the fact they share: this model has no
///   thread, and the sidecar must be restarted for it to have one.
const NO_THREAD: &str = "this model has no running thread; restart the sidecar to run it";

/// The cancellation reason a successful chain uses when it stored nothing.
///
/// D-7: COMPLETED is reserved for a cycle that has an output to name in
/// (0040,4033). A cycle whose chain finished cleanly but produced no object
/// (a processor that produced no block to store) cancels with this reason
/// instead (PS3.4 Table CC.2.5-3).
pub const NOTHING_PRODUCED: &str = "nothing produced";

/// The transition a finished chain takes, given what it produced.
///
/// Pure, so the rule in D-7 is pinned by a test rather than inferred from
/// a wire fixture that needs an inference engine to reach this point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// The chain produced at least one object: COMPLETED, naming them.
    Complete,
    /// The chain finished but stored nothing: CANCELED with a reason.
    CancelEmpty,
    /// The chain failed: CANCELED with the failure's own reason.
    CancelFailed,
}

/// Choose the transition for a finished chain.
///
/// `reason` is the chain's failure, or `None` when it succeeded. The three
/// outcomes are distinct on the wire, and only `Complete` carries
/// (0040,4033).
#[must_use]
pub fn transition(reason: Option<&str>, produced: &[OutputRef]) -> Transition {
    match reason {
        Some(_) => Transition::CancelFailed,
        None if produced.is_empty() => Transition::CancelEmpty,
        None => Transition::Complete,
    }
}

/// What happened to one workitem, for the log and the counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Processed {
    /// Claimed and handed to the inference stage.
    Claimed,
    /// Another worker holds it.
    Conflict,
    /// Claimed, then canceled with a reason.
    Canceled(String),
    /// Matched no rule and the fallback is to leave it alone.
    LeftAlone,
    /// Already in a terminal state (COMPLETED or CANCELED), so there is
    /// nothing to act on. NOT a conflict: the standard has no notion of a
    /// conflict for a workitem that someone else already finished, and
    /// counting it as one made `clinfer_conflicts_total` report contention
    /// where there was none.
    Skipped,
    /// The origin or the transport failed.
    Failed(String),
}

/// The worker's handle on the running configuration.
///
/// The reloader replaces the configuration under a lock rather than the
/// worker holding a clone: a reload that arrived mid-workitem should apply
/// to the next one, and a worker holding a snapshot would keep applying
/// the old rules until it restarted.
///
/// Under thread-per-model the READER of `current()` is the coordinator
/// alone. A model thread never consults it: it receives the `Arc<Conf>`
/// snapshot inside its job.
pub struct Shared {
    conf: Mutex<Arc<Conf>>,
}

impl Shared {
    #[must_use]
    pub fn new(conf: Arc<Conf>) -> Self {
        Self {
            conf: Mutex::new(conf),
        }
    }

    #[must_use]
    pub fn current(&self) -> Arc<Conf> {
        self.conf.lock().expect("configuration lock").clone()
    }

    pub fn replace(&self, conf: Arc<Conf>) {
        *self.conf.lock().expect("configuration lock") = conf;
    }
}

// -- The concurrency ceiling and the per-model topology ------------------

/// The outcome of taking a permit.
///
/// An enum rather than `Result<bool, ()>`: `Err(())` says nothing about WHY
/// the permit was refused, and the two refusal cases differ in what the
/// caller has already done (nothing, versus having taken a job off its
/// queue).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquire {
    /// The permit was taken; run the job.
    Acquired,
    /// The flag was already set on entry, so no permit was taken and no job
    /// has been taken either.
    Stopped,
    /// The flag was set while waiting: a permit may be free now, but the
    /// caller must exit rather than run.
    Abandoned,
}

/// Std's `Semaphore`, which std does not have.
///
/// A token counter, nothing more: `workers` permits, each taken for the
/// duration of one child process and released the moment it is reaped.
/// `Mutex<usize>` + `Condvar` is the whole implementation, which is the
/// point - a lock-free queue of permits would be harder to read and no
/// faster at a handful of threads.
pub struct Semaphore {
    state: Mutex<SemState>,
    ready: Condvar,
}

/// `held` and `capacity` under ONE lock.
///
/// Two locks would put an ordering between them that every path has to keep,
/// and `set_capacity` would have to nest them. One lock makes the pair a
/// single consistent fact, which is what the ceiling really is.
struct SemState {
    held: usize,
    capacity: usize,
}

impl Semaphore {
    /// A semaphore with `capacity` permits.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(SemState { held: 0, capacity }),
            ready: Condvar::new(),
        }
    }

    /// The effective capacity, for reporting.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.state.lock().expect("semaphore lock").capacity
    }

    /// Follow a reload. A bump admits waiters at once; a cut takes effect at
    /// the next acquire, because a permit already held belongs to a child
    /// that is already running. The ceiling is a gate, not a kill switch.
    pub fn set_capacity(&self, capacity: usize) {
        self.state.lock().expect("semaphore lock").capacity = capacity;
        // Every waiter re-reads the capacity, so one that now fits must be
        // woken; `notify_all` rather than `notify_one` because a raise can
        // admit several.
        self.ready.notify_all();
    }

    /// Take one permit, giving up after [`IDLE_TIMEOUT`].
    ///
    /// The three outcomes are distinct and the caller must treat them so:
    ///
    /// - [`Acquired`](Acquire::Acquired) - run the job;
    /// - [`Stopped`](Acquire::Stopped) - no permit was taken and the
    ///   shutdown flag was already set on entry, so there is nothing to run;
    /// - [`Abandoned`](Acquire::Abandoned) - the flag was set WHILE waiting,
    ///   so a permit may be free but the caller must not take it.
    pub fn acquire(&self, shutdown: &AtomicBool) -> Acquire {
        if shutdown.load(Ordering::Relaxed) {
            return Acquire::Stopped;
        }
        let mut state = self.state.lock().expect("semaphore lock");
        loop {
            if state.held < state.capacity {
                state.held += 1;
                return Acquire::Acquired;
            }
            // The timeout is what makes this wait BOUNDED: without it a
            // model thread parked here would never notice the shutdown flag.
            let (guard, timeout) = self
                .ready
                .wait_timeout(state, IDLE_TIMEOUT)
                .expect("semaphore lock");
            state = guard;
            if timeout.timed_out() && shutdown.load(Ordering::Relaxed) {
                return Acquire::Abandoned;
            }
        }
    }

    /// Give one permit back.
    pub fn release(&self) {
        let mut state = self.state.lock().expect("semaphore lock");
        // Saturating rather than wrapping: a double release would otherwise
        // inflate the ceiling silently, and a wrapped `usize` is unbounded.
        state.held = state.held.saturating_sub(1);
        drop(state);
        self.ready.notify_one();
    }
}

/// Dropped at the end of a run, whatever happens, including an unwind.
///
/// A panicking model thread must not leak a permit: the ceiling would then
/// sit one lower for the life of the process, and a site with `workers = N`
/// would slowly stop running anything.
struct Released<'a>(&'a Semaphore);

impl Drop for Released<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// What one dispatched rule produced, or why it did not.
///
/// Each produced object is an [`OutputRef`] naming its SOP Class UID, Series
/// Instance UID and SOP Instance UID, so the completion names the class the
/// object actually has rather than an assumed one, and names the series so a
/// consumer can build a direct retrieve URL.
pub type RuleOutcome = Result<Vec<OutputRef>, String>;

/// A completion as it arrives on the coordinator's channel: which file-order
/// slot it belongs to, and what that slot produced.
///
/// The slot is carried WITH the answer rather than inferred from arrival
/// order. Models run concurrently, so the faster rule's answer arrives
/// first; assigning answers by arrival would put a later rule's OutputRefs
/// in an earlier slot and change (0040,4033) from run to run.
type SlotOutcome = (usize, RuleOutcome);

/// One rule's work, addressed to the thread that owns the model.
///
/// `rule` and `model` travel WITH the job rather than as indices into the
/// live configuration: a reload that reorders or shortens the rule list
/// must not change what an already-dispatched job runs. The `Arc<Conf>`
/// travels too, for the few values the run reads out of it (the workdir
/// root, the version stamp).
struct Job {
    rule: crate::conf::Rule,
    model: crate::conf::Model,
    conf: Arc<Conf>,
    origin: Arc<Dicomweb>,
    counters: Arc<Counters>,
    completion: SyncSender<SlotOutcome>,
    /// The slot this job fills in the chain's file order, so its answer can
    /// be placed without guessing from the arrival order.
    slot: usize,
    /// The workitem as the coordinator read it.
    ///
    /// It travels WITH the job rather than being re-read on the model
    /// thread: the read is a claim precondition the coordinator already
    /// paid for, and an Appendix V identity read twice could differ.
    workitem: Arc<WorkItem>,
}

/// The coordinator's dispatch parameters: one sender per model, and the
/// ceiling they run under.
///
/// Held as one value so the coordinator cannot be handed a sender map and
/// a semaphore that belong to different topologies.
#[derive(Clone)]
pub struct Topology {
    senders: std::collections::HashMap<String, SyncSender<Job>>,
    semaphore: Arc<Semaphore>,
    origin: Arc<Dicomweb>,
    counters: Arc<Counters>,
}

impl Topology {
    /// The topology for a configuration with no processors: nothing to
    /// dispatch, and a ceiling of one that nothing can reach.
    ///
    /// Used by the callers that only ever claim and cancel, and by tests
    /// that drive the decision paths without a model thread.
    #[must_use]
    pub fn without_models(origin: Arc<Dicomweb>, counters: Arc<Counters>) -> Self {
        Self {
            senders: std::collections::HashMap::new(),
            semaphore: Arc::new(Semaphore::new(1)),
            origin,
            counters,
        }
    }

    /// Start the model threads and hand back the topology that feeds them.
    ///
    /// The same call `main` makes, exposed so an integration test can drive
    /// a real dispatch through real threads without reimplementing the
    /// wiring - a test that built its own channels would be testing its own
    /// copy of the topology.
    #[must_use]
    pub fn spawn(conf: &Conf, origin: Arc<Dicomweb>, counters: Arc<Counters>) -> ModelThreads {
        ModelThreads::spawn(conf, origin, counters)
    }
}

/// The model threads, plus the channels and the ceiling that feed them.
///
/// `main` keeps this value alive for the whole run, which is what keeps the
/// threads' disconnect signal open: the model threads exit when the
/// coordinator's sender clone AND every sender in this struct are gone.
pub struct ModelThreads {
    topology: Topology,
    joins: Vec<thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl ModelThreads {
    /// Start one thread per model that has a script.
    ///
    /// The thread set is STARTUP-FIXED: `workers` lowers the ceiling, it
    /// does not create or destroy threads, and a reload that changes the
    /// model list changes rules and matching, not threads.
    #[must_use]
    pub fn spawn(conf: &Conf, origin: Arc<Dicomweb>, counters: Arc<Counters>) -> Self {
        let semaphore = Arc::new(Semaphore::new(conf.workers()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut senders = std::collections::HashMap::new();
        let mut joins = Vec::new();

        for model in conf.models.iter().filter(|model| model.script.is_some()) {
            // The pair is created here and the receiver moves wholesale into
            // the thread: leaving it in a map the caller could reach would
            // be a second owner of one channel.
            let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(MODEL_QUEUE_DEPTH);
            let semaphore = Arc::clone(&semaphore);
            let shutdown = Arc::clone(&shutdown);
            let handle = thread::Builder::new()
                .name(format!("clinfer-model-{}", model.id))
                .spawn(move || model_loop(rx, semaphore, shutdown))
                .expect("spawning a model thread");
            senders.insert(model.id.clone(), tx);
            joins.push(handle);
        }

        Self {
            topology: Topology {
                senders,
                semaphore,
                origin,
                counters,
            },
            joins,
            shutdown,
        }
    }

    /// The dispatch parameters for the coordinator.
    #[must_use]
    pub fn topology(&self) -> Topology {
        self.topology.clone()
    }

    /// The ceiling, so a reload can follow it.
    #[must_use]
    pub fn semaphore(&self) -> Arc<Semaphore> {
        Arc::clone(&self.topology.semaphore)
    }

    /// Stop the threads and wait for them.
    ///
    /// Called AFTER the coordinator has drained and exited: its sender clone
    /// dropped at that point, and this struct's map drops the other one,
    /// which is what a thread parked in `recv_timeout` wakes on.
    pub fn join(self) -> Vec<thread::JoinHandle<()>> {
        self.shutdown.store(true, Ordering::Relaxed);
        self.joins
    }
}

/// ONE thread per model: take a job, take a permit, run the child, release.
fn model_loop(queue: Receiver<Job>, semaphore: Arc<Semaphore>, shutdown: Arc<AtomicBool>) {
    loop {
        let job = match queue.recv_timeout(IDLE_TIMEOUT) {
            Ok(job) => job,
            Err(RecvTimeoutError::Timeout) => {
                // The idle slice is also what lets a thread notice a
                // shutdown that arrives while its queue is empty.
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                continue;
            }
            // Every sender is gone: the coordinator exited and `main`
            // dropped its own clone. This is the normal exit.
            Err(RecvTimeoutError::Disconnected) => return,
        };

        // Acquire AFTER taking the job, which is the whole reason an idle
        // thread costs nothing: a permit is held only while a child is
        // being set up or is running.
        match semaphore.acquire(&shutdown) {
            Acquire::Acquired => {}
            // Either refusal means exit. The coordinator is joined after
            // this thread, so it has already finished the workitem it holds
            // and nothing is left to run.
            Acquire::Stopped | Acquire::Abandoned => return,
        }
        let permit = Released(&semaphore);
        let outcome = run_job(&job);
        drop(permit);

        // A send failure means the coordinator stopped listening (it is
        // shutting down); the result has nowhere to go, which is not an
        // error - the workitem's state is the coordinator's business.
        let _ = job.completion.send((job.slot, outcome));
    }
}

/// Run one job on the model's own thread.
///
/// This is the body the coordinator used to execute inline: fetch, build,
/// spawn, validate, store. Nothing about it changed except who runs it.
fn run_job(job: &Job) -> RuleOutcome {
    let Some(script) = job.model.script.as_deref() else {
        // Only models with a script get a thread, so this is unreachable
        // from `spawn`; it is the honest answer rather than a panic for a
        // job built by hand.
        return Err(format!("model '{}' has no script", job.model.id));
    };
    run_external(
        &job.origin,
        &job.conf,
        &job.counters,
        &job.workitem,
        &job.rule,
        &job.model,
        script,
    )
}

/// Read, decide, and act on one workitem.
///
/// The order is: read the workitem, decide what it needs, and claim only when
/// there is something to do. Claiming first and deciding afterwards put a
/// workitem into IN PROGRESS before anything was known about it, so an
/// `Ignore` verdict left a claimed workitem that nobody owned - the stuck
/// state the first live run produced. Claiming is the right to act, so it
/// happens when the decision says to act.
///
/// `channels` is where a matched rule's work goes. A caller that only ever
/// claims and cancels passes [`Topology::without_models`], and the `Infer`
/// arm then reports a per-rule failure for any rule it is handed - which is
/// the truth: no thread exists to run it.
pub fn process(
    origin: &Dicomweb,
    conf: &Conf,
    uid: &str,
    counters: &Counters,
    channels: &Topology,
) -> Processed {
    let workitem = match origin.get_workitem(uid) {
        Ok(workitem) => workitem,
        Err(err) => return Processed::Failed(err.to_string()),
    };

    // Only an offered workitem can be taken. Anything else is either
    // already finished by someone (a terminal state: nothing to do) or
    // held by another claimant (IN PROGRESS: a real conflict).
    if State::parse(&workitem.state) != Some(State::Scheduled) {
        let terminal = matches!(
            State::parse(&workitem.state),
            Some(State::Completed) | Some(State::Canceled)
        );
        if !terminal {
            counters.conflicts.fetch_add(1, Ordering::Relaxed);
        }
        // Naming the workitem is the difference between a count and an
        // action: a workitem left IN PROGRESS by a sidecar that was killed
        // mid-claim stays that way until an operator resets it, and this
        // line is where they learn which one.
        eprintln!(
            "clinfer: workitem {uid} is {} rather than SCHEDULED; not claimed{}",
            workitem.state,
            if terminal {
                " (already finished by another worker)"
            } else {
                ". Reset its state through the origin's administration API if this \
                 was left behind by an interrupted run"
            }
        );
        return if terminal {
            Processed::Skipped
        } else {
            Processed::Conflict
        };
    }

    // AIW-I 4.82.4.1.1: a performer shall not claim work that is not its
    // own. The assignment attributes are read from the workitem that was
    // just fetched, and a refusal is a DECLINE, not an error: the workitem
    // is left SCHEDULED and untouched, so whoever it does name can take it.
    // An absent or empty assignment names nobody, which is an offer to
    // whoever can do the work - the case a station-less deployment runs in.
    if let Some(reason) = claim_refusal(&workitem, conf) {
        counters.declined.fetch_add(1, Ordering::Relaxed);
        eprintln!("clinfer: workitem {uid} not claimed: {reason}");
        return Processed::LeftAlone;
    }

    // The workitem's own intent: (0040,4018) Scheduled Workitem Code
    // Sequence. It travels WITH the workitem, so a rule constrained by code
    // can be decided before the study is read at all.
    let code = workitem
        .scheduled_workitem_code
        .as_ref()
        .map(|c| c.value.trim().to_ascii_uppercase());
    let mut attrs = StudyAttrs {
        code,
        ..StudyAttrs::default()
    };

    // The study the rules match against: read from the study itself, not
    // from the workitem. A workitem carries the input reference and the
    // step's own attributes; the clinical attributes a rule cares about
    // belong to the study.
    //
    // The read is DEFERRED until a rule still needs it: a code-constrained
    // rule is answered from the workitem alone, so the study fetch (and the
    // network round trip it costs) happens only when the modality or an
    // optional key has to be consulted. That is the "code first, modality
    // second" order, and it makes the common hot path cheaper rather than
    // dearer.
    if conf
        .rules
        .iter()
        .any(|rule| rule_needs_the_study(&rule.r#match))
        && let Some(study_uid) = workitem.study_instance_uid.as_deref()
    {
        // A study the origin will not hand over is a FAILED read, not a
        // study with no attributes. Treating it as the latter let an
        // unconstrained rule match and drove the workitem on - the DICOM
        // retrieve that follows then fails against a study that was never
        // there, or worse, a rule keyed on a missing attribute silently
        // falls through to a catch-all. The workitem is reported failed so
        // the operator sees the origin refusing the read.
        let study = match study_attrs(origin, study_uid, conf) {
            Ok(study) => study,
            Err(err) => return Processed::Failed(err),
        };
        attrs.modalities = study.modalities;
        attrs.study_description = study.study_description;
        attrs.body_part = study.body_part;
        attrs.code_meanings = study.code_meanings;
        attrs.tags = study.tags;
    }
    let decision = ups::decide(conf, &attrs);

    match ups::action(decision) {
        Action::Ignore => {
            // Nothing to do, so nothing is claimed: the workitem stays
            // SCHEDULED and stays available to whoever does want it.
            Processed::LeftAlone
        }
        Action::Cancel { reason } => {
            // A cancel still has to own the workitem, because only the
            // holder of the transaction UID may set a final state.
            let transaction_uid = match claim(origin, uid, counters) {
                Ok(uid) => uid,
                Err(outcome) => return outcome,
            };
            match cancel(origin, uid, &transaction_uid, &reason, counters) {
                Ok(()) => Processed::Canceled(reason),
                Err(err) => Processed::Failed(err),
            }
        }
        Action::Infer { rule_indices } => {
            // Every rule the decision named, in file order. A rule the
            // configuration no longer holds means it was reloaded between
            // the decision and here; not acting is the honest outcome, and
            // the alternative is guessing which rule was meant.
            if rule_indices
                .iter()
                .any(|index| conf.rules.get(*index).is_none())
            {
                return Processed::LeftAlone;
            }
            let transaction_uid = match claim(origin, uid, counters) {
                Ok(uid) => uid,
                Err(outcome) => return outcome,
            };
            // The step starts when this performer takes the workitem, so the
            // start datetime is read here, at the claim, and not later: a
            // reading taken just before the completion would claim the step
            // ran for however long the two adjacent statements took.
            let started_at = now_datetime();
            let cycle_started = Instant::now();
            let mut produced: Vec<OutputRef> = Vec::new();
            let reason = match workitem.study_instance_uid.as_deref() {
                Some(_) => run_chain(channels, conf, &workitem, &rule_indices, &mut produced).err(),
                None => Some("workitem names no study".to_string()),
            };
            counters
                .cycle
                .observe(Stage::Total, cycle_started.elapsed().as_secs_f64());

            match transition(reason.as_deref(), &produced) {
                // COMPLETED always names what was produced (0040,4033),
                // because a COMPLETED workitem with an empty Output
                // Information Sequence claims an output the consumer
                // cannot find: PS3.4 CC.2.5-3 types the sequence as the
                // statement of what the transition produced. A cycle that
                // produced nothing (no detections, or the cache marker
                // already recorded this version) therefore does NOT
                // complete - it cancels with a reason, which is the
                // standard's word for "stopped without producing".
                Transition::CancelEmpty => {
                    let reason = NOTHING_PRODUCED;
                    match cancel(origin, uid, &transaction_uid, reason, counters) {
                        Ok(()) => Processed::Canceled(reason.to_string()),
                        Err(err) => Processed::Failed(err),
                    }
                }
                Transition::Complete => {
                    // Completion is TWO requests, in this order (AIW-I
                    // 4.84.4.1.2.1 + PS3.18 11.3 Table 11.3-1): first the
                    // Update that names the created instances in (0040,4033)
                    // OutputInformationSequence and states the performed
                    // codes, then the Change State that moves the workitem to
                    // COMPLETED.
                    //
                    // They cannot be merged. The change-state transaction
                    // carries only the state and the locking UID (11.7.1.4),
                    // so an output list riding along with it is discarded -
                    // which is exactly what happened before this split: the
                    // workitem completed and carried no results.
                    //
                    // The Update goes first so that a failure leaves the
                    // workitem unclaimed-but-in-progress rather than
                    // COMPLETED with no results: the transition is the
                    // irreversible half, so it is last.
                    //
                    // The end datetime is read HERE, at the completion, and
                    // not reused from the claim: the two are the bounds of
                    // the step, and collapsing them would report a step that
                    // took no time.
                    let performed = performed_report(conf, &workitem, &started_at, &now_datetime());
                    let update = origin.update_attributes(
                        uid,
                        &transaction_uid,
                        &produced,
                        Some(&performed),
                    );
                    match update {
                        Err(err) => Processed::Failed(err.to_string()),
                        Ok(()) => {
                            match origin.update_state(
                                uid,
                                State::Completed.as_str(),
                                Some(&transaction_uid),
                                None,
                            ) {
                                Ok(()) => {
                                    counters.completed.fetch_add(1, Ordering::Relaxed);
                                    Processed::Claimed
                                }
                                // The claim was ours, so a refusal here is
                                // the origin's answer, not a lost race.
                                Err(err) => Processed::Failed(err.to_string()),
                            }
                        }
                    }
                }
                Transition::CancelFailed => {
                    let reason = reason.unwrap_or_else(|| NOTHING_PRODUCED.to_string());
                    // The work is abandoned after the claim, so the claim
                    // is released by cancelling - with the same UID.
                    // Nothing is stored: a chain that cannot finish must
                    // not leave a half-delivered study behind, so a
                    // failure here cancels the whole workitem.
                    match cancel(origin, uid, &transaction_uid, &reason, counters) {
                        Ok(()) => Processed::Canceled(reason),
                        Err(err) => Processed::Failed(err),
                    }
                }
            }
        }
    }
}

/// The performer's own report for the update that closes the step.
///
/// The station is configuration and the workitem code is read from the
/// workitem (AIW-I 4.84.4.1.2.1): the station is this performer's identity,
/// and the performed workitem code is the SCHEDULED code echoed back - "the
/// same code as Scheduled, if present". A workitem that scheduled no code
/// has nothing to echo, and inventing one would misreport what ran.
///
/// `started_at` and `ended_at` are the times the step actually ran, captured
/// by the caller where the claim and the completion happen. They are
/// arguments rather than read here because this function has no clock and no
/// way to tell the two moments apart: the claim is what starts the step and
/// the completion is what ends it, and both are facts only the caller holds.
/// PS3.3 Table C.30.3-1 makes them members of the performed report, and a
/// standard origin refuses COMPLETED without them, so a placeholder now would
/// be worse than no report at all.
///
/// The station is declared under a local coding scheme: an AE Title is a
/// site-local identifier, not a DICOM-defined vocabulary term, and claiming
/// otherwise would be a false statement about where the code comes from.
#[must_use]
fn performed_report(
    conf: &Conf,
    workitem: &crate::dicomweb::WorkItem,
    started_at: &str,
    ended_at: &str,
) -> crate::dicomweb::PerformedReport {
    crate::dicomweb::PerformedReport {
        station_aet: conf.server.station_aet.clone(),
        station_name: conf.server.station_name.clone(),
        station_scheme: crate::dicomweb::LOCAL_CODING_SCHEME.to_string(),
        workitem_code: workitem.scheduled_workitem_code.clone(),
        started_at: started_at.to_string(),
        ended_at: ended_at.to_string(),
    }
}

/// The current instant as a DICOM `DT`.
///
/// `DT` is the concatenation of a `DA` and a `TM` with no separator, and the
/// crate already derives both from one clock reading in `pr::now_stamp`. This
/// does not recompose them through a second call: two readings a second apart
/// would let a start datetime follow an end datetime, which is a
/// contradiction no reader could resolve.
///
/// The seconds-resolution form (`YYYYMMDDHHMMSS`, 14 characters) is the one
/// `DT` allows, and the one the probe against dcm4chee-arc 5.35.2 used.
#[must_use]
fn now_datetime() -> String {
    let (date, time) = crate::pr::now_stamp();
    format!("{date}{time}")
}

/// Why this performer must NOT claim a workitem, or `None` to claim it.
///
/// AIW-I 4.82.4.1.1 states two obligations, and this function is both:
///
/// - the workitem's assignment attributes (0040,4025 Scheduled Station
///   Name Code Sequence, (0040,4036) Human Performer Code Sequence,
///   (0040,4026) Scheduled Performing Physician Identification Sequence)
///   shall not name another performer. A Code Value that names a
///   different station is a refusal; a Code Value that names OURS is a
///   confirmation; an absent or empty value names nobody, which is the
///   "offered to whoever can serve it" case the profile also allows and
///   the one a site with no station routing runs in.
/// - (0040,4041) Input Readiness State = INCOMPLETE shall not be claimed.
///   The stated reason is the point of the guard: INCOMPLETE means the
///   inputs are still arriving, so claiming now would run a model over a
///   study that is not all there yet.
///
/// The refusal is a DECLINE, not an error. The workitem stays SCHEDULED
/// and untouched, so it remains available to the performer it does name
/// (or to this one, once readiness becomes READY).
#[must_use]
fn claim_refusal(workitem: &crate::dicomweb::WorkItem, conf: &Conf) -> Option<String> {
    let station = conf.server.station_aet.as_str();
    if !workitem.performer_code_values.is_empty()
        && !workitem
            .performer_code_values
            .iter()
            .any(|value| crate::conf::station_matches(value, station))
    {
        return Some(format!(
            "assigned to another performer ({}); this station is '{station}'",
            workitem.performer_code_values.join(", ")
        ));
    }
    // Only INCOMPLETE is a refusal. READY is the value that permits work,
    // and a third state this build does not know is not evidence of
    // unreadiness - refusing on an unknown value would make an origin's
    // future addition silently stop the site.
    if workitem
        .input_readiness
        .as_deref()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("INCOMPLETE"))
    {
        return Some("Input Readiness State is INCOMPLETE (AIW-I 4.82.4.1.1)".to_string());
    }
    None
}

/// Dispatch every matched rule's processor and collect what they stored.
///
/// The chain is all-or-nothing from the workitem's point of view: the
/// caller cancels on the first error, and the objects an earlier rule
/// already stored stay in the archive (nothing here deletes a stored
/// object) but the workitem is not COMPLETED, so the study is visibly not
/// fully processed rather than silently half processed.
///
/// # Concurrency changes the clock, not the semantics
///
/// Every rule is dispatched to its model's own thread FIRST, then the
/// results are joined. Two rules on two models therefore run at the same
/// time, while one model stays strictly serial (its channel holds one job
/// and one child, so a second rule naming the SAME model waits in the
/// channel - which is FIFO, giving file order for that model).
///
/// `produced` is assembled in FILE order regardless of the order the
/// completions arrive, so (0040,4033) stays deterministic and the
/// transition rules are exactly the ones they were.
///
/// # A failure still joins what it dispatched
///
/// The chain is all-or-nothing, but "all-or-nothing" is about the workitem,
/// not about the model threads. A rule that cannot be dispatched makes the
/// chain fail, and the jobs already in flight are STILL running: returning
/// on the spot would leave them writing objects for a workitem the caller
/// is about to CANCEL, which is a stored result for a step the archive says
/// did not happen. So every error path records the reason, stops
/// dispatching, closes the channel and JOINS the outstanding jobs before
/// returning.
///
/// # Which failures can still happen mid-dispatch
///
/// The three structural holes - a rule that is no longer in the
/// configuration, an unknown model, a model with no running thread - depend
/// only on the configuration and the topology, which are fixed for the
/// whole chain. They are therefore resolved in a PRE-PASS before anything
/// is sent, so they fail with the reason and with NOTHING stored: no rule
/// was dispatched at all. The only failure left inside the loop is a send
/// that finds its thread gone, and for that case the jobs already
/// dispatched are joined - their stored objects stay in the archive
/// (nothing here deletes a stored object), but the workitem does not
/// COMPLETE.
fn run_chain(
    channels: &Topology,
    conf: &Conf,
    workitem: &WorkItem,
    rule_indices: &[usize],
    produced: &mut Vec<OutputRef>,
) -> Result<(), String> {
    // Resolve (rule, model, sender) for EVERY slot before dispatching any
    // of them. A configuration hole at rule N would otherwise store the
    // results of rules 0..N-1 first and only then cancel the workitem -
    // objects for a step the archive reports as not done.
    let mut resolved: Vec<(&crate::conf::Rule, &crate::conf::Model, &SyncSender<Job>)> =
        Vec::with_capacity(rule_indices.len());
    for index in rule_indices {
        let Some(rule) = conf.rules.get(*index) else {
            return Err(format!("rules[{index}] is no longer in the configuration"));
        };
        let Some(model) = conf.model(&rule.model) else {
            return Err(format!(
                "rules[{index}] names unknown model '{}'",
                rule.model
            ));
        };
        let Some(sender) = channels.senders.get(&model.id) else {
            // No thread holds this model. The thread set is fixed at
            // startup, so this is a model a RELOAD added: an honest
            // per-rule failure, and the workitem cancels with it.
            return Err(format!(
                "rules[{index}] names model '{}': {NO_THREAD}",
                model.id
            ));
        };
        resolved.push((rule, model, sender));
    }

    let conf = Arc::new(conf.clone());
    let workitem = Arc::new(workitem.clone());
    // The completion channel is sized to the DISPATCH COUNT, which is what
    // guarantees no model thread ever blocks on its result send: the
    // coordinator holds every job's completion capacity before the first
    // job is dispatched.
    let (completion, completions) =
        std::sync::mpsc::sync_channel::<SlotOutcome>(rule_indices.len());
    // `None` means "this slot never got a job". Every dispatch overwrites
    // its own slot with the answer it received, and no slot is filled by
    // anyone else's.
    let mut local: Vec<Option<RuleOutcome>> = (0..rule_indices.len()).map(|_| None).collect();
    let mut failure: Option<String> = None;

    for (slot, (rule, model, sender)) in resolved.into_iter().enumerate() {
        // A previous iteration failed. Dispatching more work for a
        // workitem that is about to be cancelled would store objects
        // nobody claims, so the loop stops - but it stops WITHOUT
        // returning, so the jobs already sent are joined below.
        if failure.is_some() {
            break;
        }
        let job = Job {
            rule: rule.clone(),
            model: model.clone(),
            conf: Arc::clone(&conf),
            origin: Arc::clone(&channels.origin),
            counters: Arc::clone(&channels.counters),
            completion: completion.clone(),
            slot,
            workitem: Arc::clone(&workitem),
        };
        if sender.send(job).is_err() {
            // The thread is gone and nothing was queued for it. The jobs
            // already dispatched are still joined below, so their stored
            // objects are accounted for and the workitem still does not
            // COMPLETE - the chain contract.
            failure = Some(format!(
                "rules[{}] names model '{}': {NO_THREAD}",
                rule.index, model.id
            ));
            break;
        }
    }
    // The coordinator's own sender clone must go, or a model thread would
    // see the channel as still connected after this workitem.
    drop(completion);

    // Join whatever is in flight. `collect_answers` receives until every
    // dispatched job has reported, and a model thread keeps its sender clone
    // until it exits, so this blocks for as long as the slowest dispatched
    // child takes - which is the price of not storing an object for a
    // workitem the caller is about to cancel.
    if !collect_answers(&completions, &mut local) && failure.is_none() {
        // A thread dropped its sender without answering: it panicked. The
        // jobs it still owed are unknown, not successful.
        failure = Some(format!(
            "a model thread stopped without reporting: {NO_THREAD}"
        ));
    }

    // Join the answers, then place them in file order. Every arrival named
    // its slot, so a fast later rule cannot land in an earlier one.
    for (slot, _) in rule_indices.iter().enumerate() {
        let Some(answer) = local[slot].take() else {
            // This slot never got a job, so no answer belongs to it.
            continue;
        };
        match answer {
            Ok(stored) => produced.extend(stored),
            Err(reason) if failure.is_none() => failure = Some(reason),
            Err(_) => {}
        }
    }
    match failure {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

/// Collect every dispatched job's outcome into its own slot.
///
/// Receives until the channel closes rather than for a count: the number of
/// jobs still outstanding after a dispatch failure is not known here - the
/// caller broke out of its loop - and counting it wrong would either block
/// forever on an answer that is not coming or leave a thread's send
/// unread. A closed channel means every clone is gone, which is exactly
/// "every job has reported or its thread died".
///
/// Each arrival names its slot, so the answer is placed by identity rather
/// than by arrival order.
///
/// Returns `true` when the channel closed with every dispatched slot filled
/// (the ordinary case), and `false` when a slot that was dispatched never
/// reported, i.e. a model thread died holding it.
fn collect_answers(completions: &Receiver<SlotOutcome>, local: &mut [Option<RuleOutcome>]) -> bool {
    // Tracked separately from `local` because a slot is only filled once,
    // and the test for "everything reported" must not depend on the shape
    // of the answer itself (a successful empty outcome is a value).
    let mut reported = vec![false; local.len()];
    for (slot, outcome) in completions.iter() {
        if let Some(entry) = local.get_mut(slot) {
            *entry = Some(outcome);
            reported[slot] = true;
        }
    }
    reported.iter().all(|seen| *seen)
}

/// Run one external processor over the study and store the images it
/// derived.
///
/// The shape is: fetch the matching frames, write them into a fresh work
/// directory with a manifest, spawn the configured program, read the single
/// `result.json` it wrote back, and store one derived image per frame it
/// declared. The processor never sees a DICOM object, so it cannot produce
/// a malformed one: every attribute of the stored images is written here.
///
/// The success rule is the seam's: exit 0 AND a schema-valid result AND
/// geometry that matches the input AND one derived image per frame. A run
/// that produced nothing is not a failure - a detector that found nothing
/// is a legitimate answer - and it becomes the D-7 CANCELED transition the
/// caller already implements.
///
/// Returns the stored objects, each as an [`OutputRef`].
/// The workitem's scheduled processing parameters in the manifest's shape.
///
/// The workitem's `Parameter` carries exactly the name and the value the ABI
/// manifest needs, so the values move across unchanged - this is a
/// projection, not a second reading.
fn manifest_parameters(workitem: &WorkItem) -> Vec<crate::abi::manifest::ManifestParameter> {
    workitem
        .scheduled_processing_parameters
        .iter()
        .map(|parameter| crate::abi::manifest::ManifestParameter {
            name: parameter.name.clone(),
            value: parameter.value.clone(),
        })
        .collect()
}

fn run_external(
    origin: &Dicomweb,
    conf: &Conf,
    counters: &Counters,
    workitem: &WorkItem,
    rule: &crate::conf::Rule,
    model: &crate::conf::Model,
    script: &std::path::Path,
) -> Result<Vec<OutputRef>, String> {
    use crate::abi::workdir::{self, Frame};
    use crate::abi::{RunReport, external};

    let study_uid = workitem
        .study_instance_uid
        .as_deref()
        .ok_or_else(|| "workitem names no study".to_string())?;
    let interpreter = model
        .interpreter
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from(crate::conf::DEFAULT_INTERPRETER));
    let processor = external::Processor {
        id: model.id.clone(),
        interpreter,
        script: script.to_path_buf(),
        timeout_secs: model.timeout_secs,
        max_result_bytes: model.max_result_bytes,
    };

    // -- 1. Fetch the frames ------------------------------------------
    // The same series selection the native path uses, so a rule means the
    // same thing whichever model kind executes it.
    let study_document = origin.study(study_uid).map_err(|error| error.to_string())?;
    let study = study_document
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(&study_document);
    // Appendix V: the workitem states the identity when it has one, and the
    // study it names is the fallback. Resolved once here, where both are
    // already in hand, so no writer needs a second request or a second
    // parse.
    let identity = ResultIdentity::resolve(workitem, study);
    // The study's own identity, kept apart from the resolved one: `derived.rs`
    // and `stl.rs` name the patient from the STUDY, so the synthetic images
    // and the surface mesh agree with each other even when the workitem states
    // a different patient.
    let (study_patient_id, study_patient_name) = patient_identity(study);
    let series = origin
        .series(study_uid)
        .map_err(|error| error.to_string())?;
    let fetch_started = Instant::now();
    let mut owned: Vec<Vec<u8>> = Vec::new();
    let mut meta: Vec<FrameMeta> = Vec::new();
    // The fetch is instance-by-instance, so a dead origin costs one whole
    // retry budget per instance. Two things are tracked here: the FIRST
    // instance that failed, so a lone loss reports itself by UID rather
    // than as a whole-study failure, and the current run of consecutive
    // transport failures, so a dead endpoint is given up on after a couple
    // of budgets instead of one per instance of the study.
    let mut first_failure: Option<String> = None;
    let mut consecutive_transport_failures: u32 = 0;
    for series_uid in matching_series(&series, rule)? {
        let instances = origin
            .instances(study_uid, &series_uid)
            .map_err(|error| error.to_string())?;
        for instance_uid in sop_instance_uids(&instances)? {
            let mut attempts: u32 = 0;
            let fetched = origin.instance(study_uid, &series_uid, &instance_uid, &mut attempts);
            // A request that needed more than one send is a retry the run
            // paid for, counted whether or not the last one succeeded: the
            // number is what the origin's health reads as.
            if attempts > 1 {
                counters
                    .transport_retries
                    .fetch_add(u64::from(attempts - 1), Ordering::Relaxed);
            }
            let bytes = match fetched {
                Ok(bytes) => {
                    consecutive_transport_failures = 0;
                    bytes
                }
                Err(error) => {
                    // The classes that mean "the transport, not the origin":
                    // a timeout, a reset, or a body cut short. Those are what
                    // a dead endpoint and a dropped connection produce, and
                    // they are the ones a run of consecutive failures is
                    // built from. A 4xx or a parse refusal is the origin's
                    // answer about this instance and never counts toward the
                    // endpoint looking dead.
                    let transport = matches!(
                        error,
                        crate::dicomweb::Error::Http(
                            crate::http::HttpError::Io(_)
                                | crate::http::HttpError::Truncated { .. }
                        )
                    );
                    consecutive_transport_failures = if transport {
                        consecutive_transport_failures + 1
                    } else {
                        0
                    };
                    if consecutive_transport_failures >= FETCH_FAILURE_LIMIT {
                        return Err(format!(
                            "study {study_uid} fetch aborted: {FETCH_FAILURE_LIMIT} consecutive \
                             transport failures (origin unreachable)"
                        ));
                    }
                    // Not yet a dead endpoint: keep the reason for the first
                    // lost instance and carry on, so one bad instance between
                    // healthy neighbours is reported by UID at the end rather
                    // than costing the frames that would have followed it.
                    if first_failure.is_none() {
                        first_failure = Some(format!("fetch {instance_uid}: {error}"));
                    }
                    continue;
                }
            };
            let geometry = crate::pixels::geometry(&bytes)
                .map_err(|error| format!("{instance_uid}: {error}"))?;
            let sop_class_uid = crate::pixels::sop_class_uid(&bytes)
                .ok_or_else(|| format!("{instance_uid} carries no SOP Class UID (0008,0016)"))?
                .to_string();
            let photometric = crate::pixels::photometric_interpretation(&bytes)
                .unwrap_or_else(|| "MONOCHROME2".to_string());
            let modality = crate::pixels::modality(&bytes).unwrap_or_else(|| "OT".to_string());
            let frame = crate::pixels::frame(&bytes, &geometry)
                .map_err(|error| format!("{instance_uid} frame: {error}"))?;
            meta.push(FrameMeta {
                sop_uid: instance_uid,
                sop_class_uid,
                geometry,
                photometric,
                modality,
                series_uid: series_uid.clone(),
                // A.51.1: a segmentation shares its source's Frame of
                // Reference, so the source's own value is carried forward
                // here rather than invented at encode time.
                frame_of_reference: crate::pixels::frame_of_reference_uid(&bytes),
                pixel_spacing: crate::pixels::pixel_spacing(&bytes),
                slice_spacing: crate::pixels::spacing_between_slices(&bytes),
                position: crate::pixels::image_position_patient(&bytes),
                orientation: crate::pixels::image_orientation_patient(&bytes),
                frame_offset: owned.len(),
            });
            owned.push(frame.bytes().to_vec());
        }
    }
    counters
        .cycle
        .observe(Stage::Fetch, fetch_started.elapsed().as_secs_f64());
    // One instance (or a few, not enough to look like a dead endpoint) was
    // lost. That is a data-integrity answer, not a hiccup: the run cannot
    // claim to have read the study it did not finish reading, so it reports
    // the instance by UID and the caller cancels the workitem.
    if let Some(reason) = first_failure {
        return Err(reason);
    }
    if meta.is_empty() {
        return Err(format!("study {study_uid} has no matching frames"));
    }

    // -- 2. Build the work directory ----------------------------------
    let frames: Vec<Frame<'_>> = meta
        .iter()
        .map(|entry| Frame {
            sop_uid: &entry.sop_uid,
            bytes: owned[entry.frame_offset].as_slice(),
            width: entry.geometry.cols,
            height: entry.geometry.rows,
            bits: u32::from(entry.geometry.bits_allocated),
            signed: entry.geometry.signed,
            photometric: &entry.photometric,
            // The Image Plane Module geometry, published to the workdir
            // manifest so a producer can build an honest affine. These are
            // NOT new reads: the same four values were fetched above to
            // write this build's own SEG and RS geometry, and a producer
            // given a manifest without them would have to fetch every
            // instance a second time and re-derive C.7.6.2.1-1 in its own
            // language.
            pixel_spacing: entry.pixel_spacing,
            slice_spacing: entry.slice_spacing,
            position: entry.position,
            orientation: entry.orientation,
        })
        .collect();
    let label = format!("{study_uid}-{}", model.id);
    // Work directories live in their own subdirectory of the sidecar's
    // state directory: the cache directory also holds one marker file per
    // (study, profile), and mixing directories into that listing is how an
    // operator ends up unable to tell a leftover run from state. The
    // location comes from `workdir::root` so the startup sweep and the
    // builder cannot disagree about where the runs are.
    let built = workdir::build_with_parameters(
        &workdir::root(&conf.engine.cache_dir),
        &label,
        crate::abi::StudyContext {
            uid: study_uid.to_string(),
            description: study_text(study, TAG_STUDY_DESCRIPTION),
            modalities: study_modalities(study),
            body_part: study_text(study, TAG_BODY_PART),
        },
        frames.as_slice(),
        crate::VERSION,
        rule.result.profile_version,
        &manifest_parameters(workitem),
    )
    .map_err(|error| error.to_string())?;
    // The audit line: one per run, naming the study, the size of what was
    // written and a digest over the exact bytes. It is what makes a result
    // afterwards reconcilable with the directory that produced it. The
    // digest is the crate's existing one, taken over the seed the builder
    // returned, rather than a second hash invented for this path.
    println!(
        "external: {study_uid} model={} frames={} bytes={} digest128={} workdir={}",
        processor.id,
        built.frames,
        built.bytes,
        digest128(&built.seed),
        built.path.display()
    );

    // -- 3. Run it ----------------------------------------------------
    let id = processor.id.clone();
    let run_started = Instant::now();
    // The child metrics live for exactly one run: they are set when the
    // process exists, sampled on the watchdog's tick, and reset on every
    // exit path INCLUDING an unwind (the handle is an RAII guard).
    let mut metrics = crate::abi::external::ChildMetrics::new(counters, &processor.id);
    let report = external::run(&processor, &built.path, counters, &mut metrics, |line| {
        // The prefix is what tells an operator which processor a line came
        // from when two are configured; the script's own words are kept
        // verbatim after it, so nothing is lost in translation.
        println!("[external:{id}] {line}");
    })
    .map_err(|error| error.to_string())?;
    drop(metrics);
    counters
        .cycle
        .observe(Stage::Infer, run_started.elapsed().as_secs_f64());

    // -- 4. Decide ----------------------------------------------------
    let result = match report {
        RunReport {
            outcome: external::Outcome::Success,
            result: Some(result),
            ..
        } => result,
        other => {
            // A non-zero exit, a timeout, a kill or a missing result: each
            // is a failure with its own fixed phrase, and the script's
            // stderr never reaches a DICOM attribute.
            //
            // The DIAGNOSIS goes to the log even though the fixed phrase goes
            // to the workitem. Without it a "no result" reads as a missing
            // file, and the operator cannot tell an absent file from an
            // oversized one, an unreadable one, or one written in a shape
            // the schema refused - four faults with four different fixes.
            if let Some(diagnosis) = other.diagnosis() {
                println!("external: {study_uid} model={id}: {diagnosis}");
            }
            return Err(other
                .cancel_reason()
                .unwrap_or_else(|| "external processor failed".to_string()));
        }
    };
    if result.is_empty() {
        // A valid result that carries nothing: a detector that found
        // nothing. Not an error - the caller's D-7 rule turns an empty
        // cycle into CANCELED with a reason. The workdir is KEPT: an empty
        // result is the case an operator most often wants to re-run by
        // hand.
        println!("external: {study_uid} model={id} produced nothing");
        return Ok(Vec::new());
    }

    // A box naming a frame the study does not have is a contract violation,
    // and catching it BEFORE anything is stored is what keeps a study from
    // ending up with half its overlays. `meta.len()` is the frame count.
    let frame_count = u32::try_from(meta.len()).unwrap_or(u32::MAX);
    if let Some(highest) = result.max_box_frame()
        && highest > frame_count
    {
        return Err(format!(
            "model '{}' drew a box on frame {highest}; the study has {frame_count} frame(s)",
            model.id
        ));
    }
    // Same reasoning for a segmentation: the masks are one per source
    // image, so a count that disagrees with the study is refused before
    // anything is stored. The 1..N shape of the frame numbers is already
    // checked by the schema, which needs no frame count to do it.
    if let Some(seg) = &result.seg
        && seg.files.len() != meta.len()
    {
        return Err(format!(
            "model '{}' declared {} segmentation mask(s) for {} frame(s)",
            model.id,
            seg.files.len(),
            meta.len()
        ));
    }
    // A structure set names a frame per contour, and a contour on a frame
    // the study does not have is a contract violation. Checked BEFORE
    // anything is stored, for the same reason as the masks: a structure set
    // missing the contours that fell outside the study looks complete.
    //
    // This is also the gate that makes the geometry conversion total: the
    // writer needs a frame's (0020,0032)/(0020,0037)/(0028,0030), and those
    // live on the FrameMeta reached through this index.
    if let Some(rtstruct) = &result.rtstruct {
        for (roi_index, roi) in rtstruct.rois.iter().enumerate() {
            for (contour_index, contour) in roi.contours.iter().enumerate() {
                if contour.frame == 0 || contour.frame > frame_count {
                    return Err(format!(
                        "model '{}' drew ROI {} contour {} on frame {}; the study has \
                         {frame_count} frame(s)",
                        model.id,
                        roi_index + 1,
                        contour_index + 1,
                        contour.frame
                    ));
                }
            }
        }
    }
    // And for a surface: the mesh file has to be a binary STL whose header
    // agrees with its length BEFORE anything is stored. Storing a truncated
    // mesh would put a document in the study that no viewer can load, and
    // the study would look complete.
    if result.has_surface() {
        let stl = result
            .stl
            .as_ref()
            .expect("has_surface implies an stl block");
        let path = built.path.join(&stl.file);
        let document = std::fs::read(&path)
            .map_err(|error| format!("surface mesh ({}): {error}", path.display()))?;
        if !stl_encoder::is_well_formed(&document) {
            return Err(format!(
                "surface mesh {} is {} bytes but its header declares {} triangle(s) \
                 ({} bytes expected)",
                stl.file,
                document.len(),
                stl_encoder::declared_triangles(&document).unwrap_or(0),
                stl_encoder::document_bytes(
                    stl_encoder::declared_triangles(&document).unwrap_or(0)
                )
            ));
        }
    }

    let target = DerivedTarget {
        study_uid,
        rule,
        model,
        identity: &identity,
        study_patient_name,
        study_patient_id,
    };
    let mut stored: Vec<OutputRef> = Vec::new();
    if result.has_graphics() {
        stored.extend(store_boxes(origin, counters, &target, &meta, &result)?);
    }
    if result.has_text()
        && let Some(reference) = store_report(origin, counters, &target, &meta, &result)?
    {
        stored.push(reference);
    }
    if result.has_measurements()
        && let Some(reference) = store_measurements(origin, counters, &target, &meta, &result)?
    {
        stored.push(reference);
    }
    if result.has_derived() {
        stored.extend(store_derived(
            origin, counters, &target, &built, &meta, &result,
        )?);
    }
    if result.has_segmentation() {
        stored.extend(store_segmentation(
            origin, counters, &target, &built, &meta, &result,
        )?);
    }
    if result.has_surface() {
        stored.extend(store_surface(
            origin, counters, &target, &built, &meta, &result,
        )?);
    }
    if result.has_rtstruct() {
        stored.extend(store_rtstruct(origin, counters, &target, &meta, &result)?);
    }

    // Reached only when every block that was present was stored. The
    // workdir is removed here and ONLY here: the audit line above already
    // named it, and a rerun by hand is rebuilt from the manifest (the
    // frames are in the origin, which is where the run got them).
    remove_workdir(&built.path, model, study_uid);
    Ok(stored)
}

/// Remove a workdir whose run succeeded.
///
/// A failure to remove is reported, never escalated: the study is stored
/// and the workitem is about to COMPLETE, so turning a leftover directory
/// into a cancelled workitem would trade a real result for hygiene. The
/// 24-hour sweep is the backstop.
fn remove_workdir(path: &std::path::Path, model: &crate::conf::Model, study_uid: &str) {
    match std::fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) => println!(
            "external: {study_uid} model={} stored, but its workdir {} could not be \
             removed: {error} (the 24-hour sweep will retry)",
            model.id,
            path.display()
        ),
    }
}

/// One fetched frame's provenance, kept beside the bytes it describes.
struct FrameMeta {
    sop_uid: String,
    sop_class_uid: String,
    geometry: crate::pixels::Geometry,
    photometric: String,
    /// The modality the derived image will declare, copied from the
    /// instance the frame came from.
    modality: String,
    /// The series the frame belongs to, needed by the PR/SR reference
    /// items: a reference is grouped BY SERIES, and the processor's boxes
    /// arrive flat.
    series_uid: String,
    /// (0020,0052) of the instance, when it declares one. A segmentation
    /// shares this value with its source (PS3.3 A.51.1).
    frame_of_reference: Option<String>,
    /// (0028,0030) of the instance, when it declares one. Copied into the
    /// segmentation's Pixel Measures so a mask can be measured.
    pixel_spacing: Option<[f64; 2]>,
    /// (0018,0088) of the instance, when it declares one. Copied into the
    /// segmentation's Spacing Between Slices so the mask stack is a volume.
    slice_spacing: Option<f64>,
    /// (0020,0032) of the instance, when it declares one. Copied into the
    /// frame's Plane Position so the mask is placed in the source's own
    /// coordinate system.
    position: Option<[f64; 3]>,
    /// (0020,0037) of the instance, when it declares one. Copied into the
    /// segmentation's SHARED Plane Orientation so a reader can rotate the
    /// mask onto the source's image plane.
    orientation: Option<[f64; 6]>,
    /// Index into the borrowed byte buffers, so the frames vector can be
    /// built after the borrow of the metadata is taken.
    frame_offset: usize,
}

/// What one external run needs to write the derived objects: the study it
/// belongs to, the patient identity copied into them, and the model that
/// produced them.
struct DerivedTarget<'a> {
    study_uid: &'a str,
    rule: &'a crate::conf::Rule,
    model: &'a crate::conf::Model,
    /// AIW-I Appendix V identity, already resolved workitem-first (see
    /// [`ResultIdentity`]). Borrowed so the per-frame loop clones a field
    /// at a time instead of the whole struct.
    identity: &'a ResultIdentity,
    /// The STUDY's own (0010,0010)/(0010,0020), kept separately for the
    /// writers that are out of scope for Appendix V: `derived.rs` and
    /// `stl.rs` copy from the study and must keep doing exactly that.
    study_patient_name: Option<String>,
    study_patient_id: Option<String>,
}

/// Turn a validated result into stored derived images.
///
/// One image per input frame, in frame order: the contract types
/// `derived.files` as positionally aligned with the manifest, so the number
/// of files must equal the number of frames. Anything else is a contract
/// violation and cancels the cycle rather than storing a partial series -
/// a study with half its derived frames is worse than one with none,
/// because it looks complete.
fn store_derived(
    origin: &Dicomweb,
    counters: &Counters,
    target: &DerivedTarget<'_>,
    built: &crate::abi::Workdir,
    meta: &[FrameMeta],
    result: &crate::abi::schema::ResultFile,
) -> Result<Vec<OutputRef>, String> {
    let study_uid = target.study_uid;
    let rule = target.rule;
    let model = target.model;
    let Some(derived) = &result.derived else {
        // A result with boxes or text but no derived images: the pr/sr
        // path is a later step's business, and on this batch path a
        // processor that produced neither is a processor that produced
        // nothing usable.
        return Err(format!(
            "model '{}' produced no derived images for study {study_uid}",
            model.id
        ));
    };
    if derived.files.len() != meta.len() {
        return Err(format!(
            "model '{}' declared {} derived file(s) for {} frame(s)",
            model.id,
            derived.files.len(),
            meta.len()
        ));
    }

    let series_uid = derived_series_uid(study_uid, model, rule.result.profile_version);
    let mut stored: Vec<OutputRef> = Vec::with_capacity(meta.len());
    let serialize_started = Instant::now();
    for (position, file) in derived.files.iter().enumerate() {
        let frame = &meta[position];
        // The file is a NAME inside the workdir, which the schema already
        // enforced; joining it here is what turns it into the one path the
        // workdir contains.
        let path = built.path.join(&file.file);
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("derived frame {position} ({}): {error}", path.display()))?;
        // The processor DECLARES the geometry per output and clinfer checks
        // BYTE CONSISTENCY against that declaration. Equality with the
        // source is deliberately NOT checked: DICOM allows any size, so
        // upscale, downscale and crop are all legal - what has to be caught
        // is a file that does not match the shape it claims, because that
        // is a misregistered image rather than a resized one.
        //
        // An omitted field means the source geometry for that axis, so a
        // filter that preserves geometry (clahe) declares nothing.
        let geometry = Geometry {
            rows: file.height.unwrap_or(frame.geometry.rows),
            cols: file.width.unwrap_or(frame.geometry.cols),
            bits_allocated: match file.bits {
                Some(bits) => bits,
                None => frame.geometry.bits_allocated,
            },
            // Samples per pixel and the source's own bit semantics are not
            // the processor's to change: a filter does not add channels and
            // does not turn a signed image unsigned.
            samples: frame.geometry.samples,
            bits_stored: match file.bits {
                Some(bits) => u8::try_from(bits).unwrap_or(frame.geometry.bits_stored),
                None => frame.geometry.bits_stored,
            },
            frames: 1,
            inverted: frame.geometry.inverted,
            signed: frame.geometry.signed,
        };
        let expected = derived_bytes(&geometry);
        if bytes.len() != expected {
            return Err(format!(
                "derived frame {position} is {} bytes; its declared geometry is \
                 {}x{}x{} at {} bits = {} bytes",
                bytes.len(),
                geometry.cols,
                geometry.rows,
                geometry.samples,
                geometry.bits_allocated,
                expected
            ));
        }

        let instance_number = u32::try_from(position + 1).unwrap_or(1);
        let identity = crate::derived::DerivedIdentity {
            sop_instance_uid: derived_uid(study_uid, model, rule.result.profile_version, position),
            study_uid: study_uid.to_string(),
            series_uid: series_uid.clone(),
            instance_number,
            source_sop_class_uid: frame.sop_class_uid.clone(),
            source_sop_instance_uid: frame.sop_uid.clone(),
            // The patient identity comes from the STUDY, never from the
            // processor: the origin's SOP-owner check refuses a derived
            // object whose patient disagrees with the study's, and a
            // processor has no business inventing one.
            //
            // Note: `derived.rs` is out of scope for the AIW-I Appendix V
            // sprint, so it deliberately stays study-only. `study_*` are
            // the study's own values, which is what these two fields
            // carried before the resolver existed.
            patient_name: target.study_patient_name.clone(),
            patient_id: target.study_patient_id.clone(),
            source_modality: frame.modality.clone(),
            series_description: rule.result.series_description.clone(),
        };
        let pixels = crate::derived::DerivedPixels {
            bytes: &bytes,
            geometry,
            // Photometric interpretation is copied from the source: a
            // filter changes pixels, not the colour model.
            photometric: frame.photometric.clone(),
            frame_index: instance_number,
        };
        let object = crate::derived::serialize(&identity, &pixels);
        origin
            .store_instance(&object)
            .map_err(|error| format!("derived frame {position} could not be stored: {error}"))?;
        // The reference names the class the object itself carries: a derived
        // image is a Secondary Capture (derived.rs writes that in (0008,0016)),
        // not a GSPS, so hardcoding GSPS here pointed the completion at a
        // class no viewer could read for this object. The series is the
        // derived series the object was written into, so a consumer can build
        // the direct retrieve URL (PS3.3 Table 10-3b, series UID 1C).
        stored.push(OutputRef::new(
            crate::derived::SECONDARY_CAPTURE_SOP_CLASS_UID,
            series_uid.clone(),
            identity.sop_instance_uid,
        ));
    }
    // Serializing and storing one derived image per frame is the external
    // path's equivalent of the native path's serialize+stow pair, so it is
    // reported under the same stage names and can be compared against it.
    counters
        .cycle
        .observe(Stage::Serialize, serialize_started.elapsed().as_secs_f64());
    println!(
        "external: {study_uid} model={} stored {} derived image(s)",
        model.id,
        stored.len()
    );
    Ok(stored)
}

/// The SOP Instance UID of one derived image: derived from the study, the
/// model and the frame's position, so a re-run at the same profile
/// reproduces the same UID and a bumped profile produces a genuinely new
/// one.
fn derived_uid(
    study_uid: &str,
    model: &crate::conf::Model,
    profile: u32,
    position: usize,
) -> String {
    digest128(&format!(
        "derived:{study_uid}:{}:{profile}:{position}",
        model.id
    ))
}

/// The series the derived images of one model and profile go into.
fn derived_series_uid(study_uid: &str, model: &crate::conf::Model, profile: u32) -> String {
    digest128(&format!(
        "derived-series:{study_uid}:{}:{profile}",
        model.id
    ))
}

/// Store one GSPS overlay per frame that carries at least one graphic.
///
/// The frozen scope is one presentation state bound to ONE SOP instance, at
/// both levels that matter: the series reference names the frame's own
/// series and the annotation reference names the frame's own instance. A
/// frame with no boxes AND no contours gets no object - an overlay with no
/// graphics is not a result, and one empty PR per frame would flood the
/// study with objects that say nothing.
///
/// The geometry is the PROCESSOR's: a box's label, confidence and pixel
/// coordinates, and a contour's label and traced ring, are carried verbatim
/// into the existing encoder, which owns the label text and the GSPS
/// geometry. A contour has no confidence, so its text object is the label
/// alone - see `pr::Contour2D`.
fn store_boxes(
    origin: &Dicomweb,
    counters: &Counters,
    target: &DerivedTarget<'_>,
    meta: &[FrameMeta],
    result: &crate::abi::schema::ResultFile,
) -> Result<Vec<OutputRef>, String> {
    let study_uid = target.study_uid;
    let rule = target.rule;
    let model = target.model;
    let profile_version = rule.result.profile_version;
    let profile = pr::PrProfile::of(&rule.result);
    // The series the overlays go into. One series per (study, model,
    // profile), so a bumped profile produces a genuinely new series rather
    // than mixing two generations of the same overlay.
    let series_uid = pr_series_of_model(study_uid, model, profile_version);

    let mut stored: Vec<OutputRef> = Vec::new();
    let serialize_started = Instant::now();
    for (position, frame) in meta.iter().enumerate() {
        // 1-based, matching the manifest's index and the schema's default.
        let frame_number = u32::try_from(position + 1).unwrap_or(u32::MAX);
        // One pass over the boxes, no intermediate Vec: the box count is
        // the processor's to choose and this is the one place it is large.
        let mut detections: Vec<pr::Detection> = Vec::new();
        result.boxes_on_frame(frame_number, |entry| {
            detections.push(box_detection(entry, position));
        });
        // The traced outlines for the same frame. A frame is skipped only
        // when it carries NEITHER: a segmenter emits contours and no boxes,
        // and dropping such a frame would lose the whole result.
        let mut contours: Vec<pr::Contour2D> = Vec::new();
        result.contours_on_frame(frame_number, |entry| {
            contours.push(pr::Contour2D {
                label: entry.label.clone(),
                points: entry.points.clone(),
            });
        });
        if detections.is_empty() && contours.is_empty() {
            continue;
        }

        let identity = pr::PrIdentity {
            sop_instance_uid: pr_uid_of_model(study_uid, model, profile_version, frame_number),
            study_uid: study_uid.to_string(),
            series_uid: series_uid.clone(),
            referenced: vec![pr::ReferencedSeries {
                series_uid: frame.series_uid.clone(),
                images: vec![pr::ReferencedImage {
                    sop_class_uid: frame.sop_class_uid.clone(),
                    sop_instance_uid: frame.sop_uid.clone(),
                    // The corpus stores one SOP per frame, so a frame
                    // number would be a lie. Multi-frame sources get
                    // per-frame fetch + (0008,1160) as one follow-up
                    // (finding 20).
                    frame_number: None,
                    detections,
                    contours,
                }],
            }],
            // The displayed area is the FRAME's own geometry: the overlay
            // is bound to this instance, so covering the study's largest
            // image would crop the one it is attached to.
            source_rows: frame.geometry.rows,
            source_cols: frame.geometry.cols,
            source_monochrome1: frame.geometry.inverted,
            // Appendix V: workitem first, the study it names second. The
            // birth date and sex are carried here because the PR writer
            // types them as source-copied attributes and the previous code
            // wrote them empty regardless of what either source said.
            patient_name: target.identity.patient_name.clone(),
            patient_id: target.identity.patient_id.clone(),
            patient_id_issuer: target.identity.patient_id_issuer.clone(),
            patient_birth_date: target.identity.patient_birth_date.clone(),
            patient_sex: target.identity.patient_sex.clone(),
            study_id: target.identity.study_id.clone(),
            accession_number: target.identity.accession_number.clone(),
            referring_physician: target.identity.referring_physician.clone(),
            // (0008,1010) from (0040,4028), no fallback (Appendix V
            // Note 4 maps it FROM the workitem's sequence).
            station_name: target.identity.station_name.clone(),
            scheduled_protocol_code: target.identity.scheduled_workitem_code.clone(),
            requested_procedure_id: target.identity.requested_procedure_id.clone(),
            requested_procedure_description: target
                .identity
                .requested_procedure_description
                .clone(),
            requested_procedure_code: target.identity.requested_procedure_code.clone(),
            reason_for_requested_procedure: target.identity.reason_for_requested_procedure.clone(),
            reason_for_requested_procedure_code: target
                .identity
                .reason_for_requested_procedure_code
                .clone(),
        };
        let bytes = pr::serialize(&identity, &profile);
        origin.store_instance(&bytes).map_err(|error| {
            format!("overlay for frame {frame_number} could not be stored: {error}")
        })?;
        // The overlay IS a Grayscale Softcopy Presentation State (pr.rs writes
        // that in (0008,0016)), so the reference names GSPS here - the one path
        // for which the old hardcoded class was correct. The series is the PR
        // series the overlay was written into.
        stored.push(OutputRef::new(
            pr::GSPS_SOP_CLASS_UID,
            series_uid.clone(),
            identity.sop_instance_uid,
        ));
    }
    counters
        .cycle
        .observe(Stage::Serialize, serialize_started.elapsed().as_secs_f64());
    println!(
        "external: {study_uid} model={} stored {} overlay(s) over {} frame(s)",
        model.id,
        stored.len(),
        meta.len()
    );
    Ok(stored)
}

/// Store one BINARY segmentation per study from the declared masks.
///
/// One object per study, not one per frame: a segmentation is a stack of
/// frames, and the ABI's `files` list is positionally the study's frames.
/// The masks are copied VERBATIM into the bit-packed Pixel Data - the
/// processor already produced the exact bytes the standard wants, so an
/// unpack/repack here would only add a way for the two sides to disagree
/// about bit order.
fn store_segmentation(
    origin: &Dicomweb,
    counters: &Counters,
    target: &DerivedTarget<'_>,
    built: &crate::abi::workdir::Workdir,
    meta: &[FrameMeta],
    result: &crate::abi::schema::ResultFile,
) -> Result<Vec<OutputRef>, String> {
    let study_uid = target.study_uid;
    let rule = target.rule;
    let model = target.model;
    let profile_version = rule.result.profile_version;
    let Some(seg) = &result.seg else {
        return Ok(Vec::new());
    };
    // The mask count was checked against the study's frame count BEFORE
    // anything was stored (the caller's gate). It is deliberately not
    // repeated here: a second copy of one invariant is a second place for
    // it to drift, and this one could only ever be unreachable.
    debug_assert_eq!(
        seg.files.len(),
        meta.len(),
        "the caller checked the mask count against the study's frames"
    );

    let serialize_started = Instant::now();
    // The masks, in frame order, each read once. Payload moves one buffer
    // per frame: no intermediate Vec of the whole stack, because on a
    // 512x512 study that stack is the largest thing this path touches.
    // An ABI file packs one bit plane per segment; the writer splits it
    // into ONE FRAME PER SEGMENT (C.8.20.3.1), so the file size scales
    // with the declaration while each written frame is a single plane.
    let expected_file_bytes = seg::frame_bytes(&meta[0].geometry, seg.segments.len());
    let plane_bytes = seg::plane_bytes(&meta[0].geometry);
    let mut frames: Vec<seg::SegFrame> = Vec::with_capacity(meta.len() * seg.segments.len());
    // `files` is a 1..N permutation (the schema proved it), so the frame
    // number is the position in that run: index `frame - 1`.
    let mut ordered: Vec<Option<usize>> = vec![None; meta.len()];
    for (position, entry) in seg.files.iter().enumerate() {
        ordered[(entry.frame - 1) as usize] = Some(position);
    }
    for (index, frame) in meta.iter().enumerate() {
        let position = ordered[index]
            .ok_or_else(|| format!("segmentation mask for frame {} is missing", index + 1))?;
        let entry = &seg.files[position];
        let path = built.path.join(&entry.file);
        let bytes = std::fs::read(&path).map_err(|error| {
            format!(
                "segmentation mask for frame {} ({}): {error}",
                index + 1,
                path.display()
            )
        })?;
        // Byte consistency against the SOURCE frame's geometry: a mask is
        // the shape of the image it describes, so a file of any other size
        // is a mask that cannot be laid over that image. A named error,
        // never padded - padding would invent pixels that were never
        // classified.
        if bytes.len() != expected_file_bytes {
            return Err(format!(
                "segmentation mask for frame {} is {} bytes; a {}x{} 1-bit mask is {} bytes",
                index + 1,
                bytes.len(),
                frame.geometry.cols,
                frame.geometry.rows,
                expected_file_bytes
            ));
        }
        let source = seg::SegSource {
            sop_class_uid: frame.sop_class_uid.clone(),
            sop_instance_uid: frame.sop_uid.clone(),
            // (0008,1115) groups its instances by series, so every source
            // carries the series it came from.
            series_uid: frame.series_uid.clone(),
            // (0020,9113) Plane Position is a copy of the source's own
            // Image Position Patient, which is what places the mask.
            position: frame.position,
            // (0020,9116) Plane Orientation is a copy of the source's own
            // Image Orientation Patient, which is what rotates the mask.
            orientation: frame.orientation,
        };
        // C.8.20.3.1: a BINARY frame is ONE bit plane of ONE segment, so
        // the processor's multi-plane file becomes one frame per segment,
        // each naming its segment number in the per-frame functional
        // group.
        for (segment_index, _segment) in seg.segments.iter().enumerate() {
            let start = segment_index * plane_bytes;
            let mask = bytes[start..start + plane_bytes].to_vec();
            frames.push(seg::SegFrame {
                segment_number: u16::try_from(segment_index + 1).unwrap_or(u16::MAX),
                mask,
                source: source.clone(),
            });
        }
    }

    // (0070,0080) Content Label is Type 1 and an empty value is not an
    // acceptable one, so the first segment's label is the fallback when the
    // rule names no series description. A segmentation with no segments at
    // all cannot reach here (the schema refuses it), but the empty string
    // is written rather than unwrapped: this is the one path that could
    // otherwise panic, and a wrong label is not worth a crash.
    let content_label = identity_label(rule, seg);
    let series_uid = seg_series_of_model(study_uid, model, profile_version);
    let identity = seg::SegIdentity {
        sop_instance_uid: seg_uid_of_model(study_uid, model, profile_version),
        study_uid: study_uid.to_string(),
        series_uid: series_uid.clone(),
        // A.51.1: the segmentation shares the source Frame of Reference.
        // The manifest carries the source studies' frames of reference, so
        // this is copied when the source defines one and left absent
        // otherwise.
        frame_of_reference_uid: meta
            .iter()
            .find_map(|frame| frame.frame_of_reference.clone()),
        source_geometry: meta[0].geometry,
        // The source's own spacing, so a reader measures the mask in the
        // units the image was acquired in.
        pixel_spacing: meta.iter().find_map(|frame| frame.pixel_spacing),
        slice_spacing: meta.iter().find_map(|frame| frame.slice_spacing),
        // (0062,0009) names WHICH algorithm produced the segment, so the
        // model that ran is what it records. The schema requires a
        // non-empty model id, but the fixed name is the documented
        // fallback rather than an empty 1C attribute.
        algorithm_name: if model.id.is_empty() {
            crate::sr::MANUFACTURER.to_string()
        } else {
            model.id.clone()
        },
        content_label,
        segments: seg.segments.clone(),
        segments_overlap: seg.overlap.clone(),
        frames,
        // Appendix V: workitem first, the study it names second.
        patient_name: target.identity.patient_name.clone(),
        patient_id: target.identity.patient_id.clone(),
        patient_id_issuer: target.identity.patient_id_issuer.clone(),
        study_id: target.identity.study_id.clone(),
        accession_number: target.identity.accession_number.clone(),
        // (0008,1010) from (0040,4028), with no fallback (Appendix V
        // Note 4 maps it FROM the workitem's sequence).
        station_name: target.identity.station_name.clone(),
        scheduled_protocol_code: target.identity.scheduled_workitem_code.clone(),
        requested_procedure_id: target.identity.requested_procedure_id.clone(),
        requested_procedure_description: target.identity.requested_procedure_description.clone(),
        requested_procedure_code: target.identity.requested_procedure_code.clone(),
        reason_for_requested_procedure: target.identity.reason_for_requested_procedure.clone(),
        reason_for_requested_procedure_code: target
            .identity
            .reason_for_requested_procedure_code
            .clone(),
        series_description: rule.result.series_description.clone(),
    };
    let object = seg::serialize(&identity);
    origin.store_instance(&object).map_err(|error| {
        format!(
            "segmentation {} could not be stored: {error}",
            identity.sop_instance_uid
        )
    })?;
    counters.cycle.observe(
        Stage::SerializeSeg,
        serialize_started.elapsed().as_secs_f64(),
    );
    println!(
        "external: {study_uid} model={} stored a segmentation with {} segment(s) over {} frame(s)",
        model.id,
        identity.segments.len(),
        identity.frames.len()
    );
    // The object IS a Segmentation (seg.rs writes (0008,0016)), so the
    // reference names that class, plus the SEG series it was written into.
    Ok(vec![OutputRef::new(
        seg::SEG_SOP_CLASS_UID,
        series_uid,
        identity.sop_instance_uid,
    )])
}

/// (0070,0080) Content Label for a segmentation.
///
/// The series description when the rule names one, otherwise the first
/// segment's label. It is Type 1, so it must be non-empty; the caller's
/// schema guarantees at least one segment, and this returns an empty string
/// rather than panicking on the path that cannot happen.
fn identity_label(rule: &crate::conf::Rule, seg: &crate::abi::schema::Seg) -> String {
    if let Some(description) = &rule.result.series_description
        && !description.is_empty()
    {
        return description.clone();
    }
    seg.segments
        .first()
        .map(|segment| segment.label.clone())
        .unwrap_or_default()
}

/// The SOP Instance UID of one external model's segmentation.
///
/// Derived over the study, the model and the profile version, so a rerun
/// reproduces the same UID and a bumped profile produces a genuinely new
/// object rather than overwriting the previous generation.
fn seg_uid_of_model(study_uid: &str, model: &crate::conf::Model, profile: u32) -> String {
    digest128(&format!("external-seg:{study_uid}:{}:{profile}", model.id))
}

/// The series one external model's segmentation goes into.
fn seg_series_of_model(study_uid: &str, model: &crate::conf::Model, profile: u32) -> String {
    digest128(&format!(
        "external-seg-series:{study_uid}:{}:{profile}",
        model.id
    ))
}

/// Store one RT Structure Set per study.
///
/// One object per STUDY, not one per frame: a structure set is a single
/// document listing every ROI, and each contour inside it carries the slice
/// it was traced on. That is the opposite shape from a segmentation, which
/// is one object holding one frame per image.
///
/// The conversion from the ABI's pixel-index polygons to patient
/// millimetres happens HERE, for the reason the writer's module doc gives:
/// clinfer already holds every frame's geometry in `meta`, so it converts
/// once in one place rather than asking the processor to re-fetch the
/// instances and re-implement Equation C.7.6.2.1-1.
fn store_rtstruct(
    origin: &Dicomweb,
    counters: &Counters,
    target: &DerivedTarget<'_>,
    meta: &[FrameMeta],
    result: &crate::abi::schema::ResultFile,
) -> Result<Vec<OutputRef>, String> {
    let study_uid = target.study_uid;
    let rule = target.rule;
    let model = target.model;
    let profile_version = rule.result.profile_version;
    let Some(rtstruct) = &result.rtstruct else {
        return Ok(Vec::new());
    };
    // `has_rtstruct()` already proved the block is present and carries at
    // least one ROI, and the caller's gate proved every referenced frame
    // exists. Neither invariant is re-checked here: a second copy of one
    // invariant is a second place for it to drift.
    debug_assert!(!rtstruct.rois.is_empty(), "has_rtstruct implies a ROI");

    let serialize_started = Instant::now();

    // The distinct source slices, in first-appearance order. C.8.8.5.1
    // wants each referenced slice listed ONCE in the reference chain, not
    // once per contour that touches it, so the set is built here rather
    // than inside the writer.
    let mut sources: Vec<crate::rtstruct::RtSource> = Vec::new();
    let mut source_series: Vec<String> = Vec::new();
    let mut entries: Vec<crate::rtstruct::RtRoiEntry> = Vec::with_capacity(rtstruct.rois.len());

    for (roi_index, roi) in rtstruct.rois.iter().enumerate() {
        let mut contours = Vec::with_capacity(roi.contours.len());
        for contour in &roi.contours {
            // The gate above proved this index is in range, so this is a
            // lookup rather than a fallible conversion. `frame` is 1-based,
            // indexing the manifest's frames.
            let frame = meta
                .get((contour.frame - 1) as usize)
                .ok_or_else(|| format!("ROI {} names frame {}", roi_index + 1, contour.frame))?;
            let source = rtstruct_source(frame);
            if !sources.contains(&source) {
                if !source_series.contains(&source.series_uid) {
                    source_series.push(source.series_uid.clone());
                }
                sources.push(source.clone());
            }
            // The ABI hands over (column, row) pairs; the writer wants
            // patient millimetres. One conversion per point, through the
            // function unit-tested against Equation C.7.6.2.1-1.
            let points = contour
                .points
                .iter()
                .map(|point| crate::rtstruct::to_patient(&source, point[0], point[1]))
                .collect();
            contours.push(crate::rtstruct::RtContour { points, source });
        }
        entries.push(crate::rtstruct::RtRoiEntry {
            // ROI numbers are 1..N with no gaps, assigned as position + 1,
            // the same rule the writer's own pins assert.
            number: u16::try_from(roi_index + 1).unwrap_or(u16::MAX),
            roi: roi.clone(),
            contours,
        });
    }

    let series_uid = rtstruct_series_of_model(study_uid, model, profile_version);
    let identity = crate::rtstruct::RtIdentity {
        sop_instance_uid: rtstruct_uid_of_model(study_uid, model, profile_version),
        study_uid: study_uid.to_string(),
        series_uid: series_uid.clone(),
        // The source Frame of Reference, copied when the study defines one
        // (Usage U). An invented value would claim the contours live in a
        // coordinate system nothing else knows about.
        frame_of_reference_uid: meta
            .iter()
            .find_map(|frame| frame.frame_of_reference.clone()),
        label: rtstruct_label(model, profile_version),
        // (3006,0004) is Type 3 and purely descriptive: a Structure Set Name
        // is what a human reads in a list.
        name: rule
            .result
            .series_description
            .clone()
            .filter(|description| !description.is_empty()),
        rois: entries,
        // (0008,2111) Derivation Description is the ONE provenance channel
        // the A.19 modules provide: the RT SS IOD has no
        // segment-algorithm analogue, so the model that produced the
        // structure set is named here or nowhere.
        derivation_description: if model.id.is_empty() {
            crate::sr::MANUFACTURER.to_string()
        } else {
            model.id.clone()
        },
        // Appendix V identity, workitem first and the study it names second,
        // resolved exactly as the SEG writer resolves it.
        patient_name: target.identity.patient_name.clone(),
        patient_id: target.identity.patient_id.clone(),
        patient_id_issuer: target.identity.patient_id_issuer.clone(),
        patient_birth_date: target.identity.patient_birth_date.clone(),
        patient_sex: target.identity.patient_sex.clone(),
        study_id: target.identity.study_id.clone(),
        accession_number: target.identity.accession_number.clone(),
        referring_physician: target.identity.referring_physician.clone(),
        station_name: target.identity.station_name.clone(),
        series_description: rule.result.series_description.clone(),
        source_series,
        contour_images: sources,
    };
    // A contour too large for its own VR is refused HERE, before anything is
    // stored: the message becomes the workitem's cancellation reason, so the
    // operator sees which polygon was too big rather than a 400 from the
    // origin with a byte offset in it.
    let object = crate::rtstruct::serialize(&identity)?;
    origin.store_instance(&object).map_err(|error| {
        format!(
            "structure set {} could not be stored: {error}",
            identity.sop_instance_uid
        )
    })?;
    counters.cycle.observe(
        Stage::SerializeRtstruct,
        serialize_started.elapsed().as_secs_f64(),
    );
    let contour_count: usize = identity.rois.iter().map(|entry| entry.contours.len()).sum();
    println!(
        "external: {study_uid} model={} stored {} ROI(s) with {contour_count} contour(s) \
         over {} slice(s)",
        model.id,
        identity.rois.len(),
        identity.contour_images.len()
    );
    // The object IS an RT Structure Set (rtstruct.rs writes (0008,0016)), so
    // the reference names that class, plus the RTSTRUCT series it was written
    // into.
    Ok(vec![OutputRef::new(
        crate::rtstruct::RT_STRUCT_SOP_CLASS_UID,
        series_uid,
        identity.sop_instance_uid,
    )])
}

/// The source-slice view a contour reference needs.
///
/// A projection of [`FrameMeta`] onto exactly the fields a structure set
/// references, so the writer never sees the whole frame record.
fn rtstruct_source(frame: &FrameMeta) -> crate::rtstruct::RtSource {
    crate::rtstruct::RtSource {
        sop_class_uid: frame.sop_class_uid.clone(),
        sop_instance_uid: frame.sop_uid.clone(),
        // (3006,0014) groups its slices BY SERIES, and the processor's
        // contours arrive flat, so the series is what carries a contour
        // back to the images it was traced on.
        series_uid: frame.series_uid.clone(),
        position: frame.position,
        orientation: frame.orientation,
        pixel_spacing: frame.pixel_spacing,
    }
}

/// (3006,0002) Structure Set Label for an external structure set.
///
/// Type 1 and SH, so it must be non-empty and at most 16 characters. The
/// architect report rules the candidate is DERIVED FROM THE MODEL ID and the
/// profile version, not taken from the ABI's own `label` and not taken from
/// the rule's series description: the label names the production run, so a
/// reader can tell which model produced a structure set from the label
/// alone.
///
/// The cut is at a CHARACTER boundary, never mid-codepoint: a reader decodes
/// an SH as text, so a value split mid-sequence is not decodable. The
/// fallback is the word `RTSTRUCT`, used when the cut would leave nothing
/// (a model id of multi-byte glyphs only).
fn rtstruct_label(model: &crate::conf::Model, profile: u32) -> String {
    let candidate = if model.id.is_empty() {
        crate::rtstruct::RTSTRUCT_FALLBACK_LABEL.to_string()
    } else {
        format!("{}-{profile}", model.id)
    };
    let bounded: String = candidate
        .chars()
        .take(crate::abi::schema::MAX_STRUCTURE_SET_LABEL)
        .collect();
    if bounded.is_empty() {
        // Only reachable when the candidate is empty, which the check above
        // already excludes; belt and braces because the attribute is Type 1.
        crate::rtstruct::RTSTRUCT_FALLBACK_LABEL.to_string()
    } else {
        bounded
    }
}

/// The SOP Instance UID of one external model's structure set.
///
/// Derived over the study, the model and the profile version, so a rerun
/// reproduces the same UID and a bumped profile produces a genuinely new
/// object rather than overwriting the previous generation.
fn rtstruct_uid_of_model(study_uid: &str, model: &crate::conf::Model, profile: u32) -> String {
    digest128(&format!(
        "external-rtstruct:{study_uid}:{}:{profile}",
        model.id
    ))
}

/// The series one external model's structure set goes into.
fn rtstruct_series_of_model(study_uid: &str, model: &crate::conf::Model, profile: u32) -> String {
    digest128(&format!(
        "external-rtstruct-series:{study_uid}:{}:{profile}",
        model.id
    ))
}

/// Store one Encapsulated STL surface mesh per study.
///
/// One object per study, not one per frame: A.85.1 stores a single
/// encapsulated document, so the ABI's `stl.file` names one file holding
/// the whole surface. The bytes are copied VERBATIM into Encapsulated
/// Document (0042,0011) - the processor produced the STL and this writer is
/// not asked to reinterpret it, so a re-encode here would mean the mesh a
/// viewer loads is not the mesh that was computed.
///
/// The file was already validated as a well-formed binary STL by the
/// caller's gate, before anything was stored.
fn store_surface(
    origin: &Dicomweb,
    counters: &Counters,
    target: &DerivedTarget<'_>,
    built: &crate::abi::workdir::Workdir,
    meta: &[FrameMeta],
    result: &crate::abi::schema::ResultFile,
) -> Result<Vec<OutputRef>, String> {
    let study_uid = target.study_uid;
    let rule = target.rule;
    let model = target.model;
    let profile_version = rule.result.profile_version;
    let Some(stl) = &result.stl else {
        return Ok(Vec::new());
    };

    let serialize_started = Instant::now();
    let path = built.path.join(&stl.file);
    let document = std::fs::read(&path)
        .map_err(|error| format!("surface mesh ({}): {error}", path.display()))?;

    let series_uid = stl_series_of_model(study_uid, model, profile_version);
    let identity = stl_encoder::StlIdentity {
        sop_instance_uid: stl_uid_of_model(study_uid, model, profile_version),
        study_uid: study_uid.to_string(),
        series_uid: series_uid.clone(),
        // A.85.1 makes Frame of Reference Type 1, so it is always written.
        // The mesh's axes are the source images' when the source declared
        // them; when it did not, an own UID is derived, because a mesh has
        // a coordinate system whether or not its source named one.
        frame_of_reference_uid: stl_frame_of_reference(
            study_uid,
            model,
            profile_version,
            meta.iter()
                .find_map(|frame| frame.frame_of_reference.clone()),
        ),
        // The patient identity comes from the STUDY, never from the
        // processor: same rule as the derived, PR, SR and SEG writers.
        //
        // It is the STUDY-only pair, not `target.identity`: the workitem-first
        // resolution prefers the workitem's patient fields, and on a study
        // whose workitem names the patient differently that would make the
        // mesh disagree with the derived images it sits beside. Same fallback
        // chain as the derived writer, so both name the patient identically.
        patient_name: target.study_patient_name.clone(),
        patient_id: target.study_patient_id.clone(),
        study_id: None,
        accession_number: None,
        series_description: rule.result.series_description.clone(),
        document,
    };
    let object = stl_encoder::serialize(&identity);
    origin.store_instance(&object).map_err(|error| {
        format!(
            "surface mesh {} could not be stored: {error}",
            identity.sop_instance_uid
        )
    })?;
    counters.cycle.observe(
        Stage::SerializeStl,
        serialize_started.elapsed().as_secs_f64(),
    );
    println!(
        "external: {study_uid} model={} stored a surface mesh of {} triangle(s)",
        model.id,
        stl_encoder::declared_triangles(&identity.document).unwrap_or(0)
    );
    // The object IS a Surface Segmentation (STL) document (stl.rs writes
    // (0008,0016)), so the reference names that class, plus the STL series it
    // was written into.
    Ok(vec![OutputRef::new(
        stl_encoder::STL_SOP_CLASS_UID,
        series_uid,
        identity.sop_instance_uid,
    )])
}

/// The SOP Instance UID of one external model's surface mesh.
fn stl_uid_of_model(study_uid: &str, model: &crate::conf::Model, profile: u32) -> String {
    digest128(&format!("external-stl:{study_uid}:{}:{profile}", model.id))
}

/// The series one external model's surface mesh goes into.
fn stl_series_of_model(study_uid: &str, model: &crate::conf::Model, profile: u32) -> String {
    digest128(&format!(
        "external-stl-series:{study_uid}:{}:{profile}",
        model.id
    ))
}

/// The Frame of Reference a surface mesh declares.
///
/// A.85.1 makes (0020,0052) Type 1: it says what coordinate system the
/// mesh's axes are relative to. The source images' value is preferred,
/// because a mesh derived from them lives in their frame. When the source
/// declares none - legal, the attribute is Type 3 in many image IODs - the
/// mesh still has axes, so an own UID is derived rather than the attribute
/// being omitted: an absent Type 1 attribute is a malformed object, and
/// naming the frame honestly as this study's own is better than claiming a
/// frame nothing else knows about.
fn stl_frame_of_reference(
    study_uid: &str,
    model: &crate::conf::Model,
    profile: u32,
    source: Option<String>,
) -> String {
    source.unwrap_or_else(|| {
        digest128(&format!(
            "external-stl-for:{study_uid}:{}:{profile}",
            model.id
        ))
    })
}

/// The bytes one derived frame must contain, for a validated geometry.
///
/// The bit depth rounds UP to whole bytes: a 12-bit sample occupies two
/// bytes, not one and a half. Truncating the division would make a
/// 12-bit frame look twice as large as the file the processor can write,
/// so every derived image would be refused.
fn derived_bytes(geometry: &Geometry) -> usize {
    geometry.rows as usize
        * geometry.cols as usize
        * geometry.samples as usize
        * usize::from(geometry.bits_allocated).div_ceil(8)
}

/// One schema box as the encoder's own detection type.
/// `class_index` is not carried by the schema (a processor reports a label,
/// not a class number) and is set to 0: the label is what the text object
/// shows, and the encoder does not read the index for anything else.
fn box_detection(entry: &crate::abi::schema::PrBox, position: usize) -> pr::Detection {
    let _ = position;
    pr::Detection {
        label: entry.label.clone(),
        class_index: 0,
        confidence: entry.confidence as f32,
        x1: entry.x1 as f32,
        y1: entry.y1 as f32,
        x2: entry.x2 as f32,
        y2: entry.y2 as f32,
    }
}

/// Store one Basic Text SR for the study.
///
/// One document per study, per the frozen TID 2002 profile: the text is the
/// processor's answer written VERBATIM (clinfer adds no wording of its own),
/// and the evidence is every frame that was analyzed.
fn store_report(
    origin: &Dicomweb,
    counters: &Counters,
    target: &DerivedTarget<'_>,
    meta: &[FrameMeta],
    result: &crate::abi::schema::ResultFile,
) -> Result<Option<OutputRef>, String> {
    let study_uid = target.study_uid;
    let rule = target.rule;
    let model = target.model;
    let profile_version = rule.result.profile_version;
    let Some(sr) = &result.sr else {
        return Ok(None);
    };
    let text = sr.text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if meta.is_empty() {
        // A document that reports on nothing is not a report. The caller
        // reaches here only with frames present, so this is a guard rather
        // than a case, but an empty evidence list would make the SR
        // unreadable rather than merely empty.
        return Err(format!(
            "model '{}' produced report text but no frame was analyzed",
            model.id
        ));
    }

    // One reference item per series, in the order the frames were fetched:
    // a viewer binds the report to an image by walking these.
    let mut referenced: Vec<sr::SrReferencedSeries> = Vec::new();
    for frame in meta {
        match referenced
            .iter_mut()
            .find(|series| series.series_uid == frame.series_uid)
        {
            Some(series) => series.images.push(sr::SrReferencedImage {
                sop_class_uid: frame.sop_class_uid.clone(),
                sop_instance_uid: frame.sop_uid.clone(),
            }),
            None => referenced.push(sr::SrReferencedSeries {
                series_uid: frame.series_uid.clone(),
                images: vec![sr::SrReferencedImage {
                    sop_class_uid: frame.sop_class_uid.clone(),
                    sop_instance_uid: frame.sop_uid.clone(),
                }],
            }),
        }
    }

    let series_uid = sr_series_uid_of_model(study_uid, model, profile_version);
    let identity = sr::SrIdentity {
        sop_instance_uid: sr_uid_of_model(study_uid, model, profile_version),
        study_uid: study_uid.to_string(),
        series_uid: series_uid.clone(),
        referenced,
        // Appendix V: workitem first, the study it names second.
        patient_name: target.identity.patient_name.clone(),
        patient_id: target.identity.patient_id.clone(),
        patient_id_issuer: target.identity.patient_id_issuer.clone(),
        referring_physician: target.identity.referring_physician.clone(),
        study_id: target.identity.study_id.clone(),
        accession_number: target.identity.accession_number.clone(),
        // (0008,1010) from (0040,4028), with no fallback.
        station_name: target.identity.station_name.clone(),
        scheduled_protocol_code: target.identity.scheduled_workitem_code.clone(),
        requested_procedure_id: target.identity.requested_procedure_id.clone(),
        requested_procedure_description: target.identity.requested_procedure_description.clone(),
        requested_procedure_code: target.identity.requested_procedure_code.clone(),
        reason_for_requested_procedure: target.identity.reason_for_requested_procedure.clone(),
        reason_for_requested_procedure_code: target
            .identity
            .reason_for_requested_procedure_code
            .clone(),
    };
    let profile = sr::SrProfile::of(&rule.result);
    let serialize_started = Instant::now();
    let bytes = sr::serialize(&identity, &profile, text);
    counters.cycle.observe(
        Stage::SerializeSr,
        serialize_started.elapsed().as_secs_f64(),
    );
    origin.store_instance(&bytes).map_err(|error| {
        format!(
            "report {} could not be stored: {error}",
            identity.sop_instance_uid
        )
    })?;
    println!(
        "external: {study_uid} model={} stored SR {} ({} char(s))",
        model.id,
        identity.sop_instance_uid,
        text.len()
    );
    // The report IS a Basic Text SR (sr.rs writes (0008,0016)), so the
    // reference names that class, plus the SR series it was written into.
    Ok(Some(OutputRef::new(
        sr::BASIC_TEXT_SR_SOP_CLASS_UID,
        series_uid,
        identity.sop_instance_uid,
    )))
}

/// Store the numeric findings as an Enhanced SR Measurement Report (TID 1500).
///
/// Mirrors [`store_report`] in identity and discipline: the same study and
/// workitem determine the document, the frame set is the evidence, and the
/// write goes through the same origin call. It differs in exactly one thing -
/// the UID namespace - so an SR and a measurements report of the same model
/// can never collide.
fn store_measurements(
    origin: &Dicomweb,
    counters: &Counters,
    target: &DerivedTarget<'_>,
    meta: &[FrameMeta],
    result: &crate::abi::schema::ResultFile,
) -> Result<Option<OutputRef>, String> {
    let study_uid = target.study_uid;
    let rule = target.rule;
    let model = target.model;
    let profile_version = rule.result.profile_version;
    let Some(measurements) = &result.measurements else {
        return Ok(None);
    };
    if !result.has_measurements() {
        return Ok(None);
    }
    if meta.is_empty() {
        // Same rule as the text report: a document that measures nothing is
        // not a document.
        return Err(format!(
            "model '{}' produced measurements but no frame was analyzed",
            model.id
        ));
    }

    let images = meta
        .iter()
        .map(|frame| measurements_writer::MeasurementsImage {
            sop_class_uid: frame.sop_class_uid.clone(),
            sop_instance_uid: frame.sop_uid.clone(),
            series_uid: frame.series_uid.clone(),
        })
        .collect();

    // TID 4019 identity: the producer's own model or script name, copied.
    // `Producer` is the ABI's own record of what produced the result, so
    // there is no fallback to invent - a processor that names neither is a
    // processor the schema already refused.
    let producer = &result.producer;
    let algorithm_name = producer
        .model
        .clone()
        .or_else(|| Some(producer.script.clone()))
        .unwrap_or_default();
    let algorithm_version = producer
        .model_version
        .clone()
        .or_else(|| producer.script_version.clone());

    let series_uid = measurements_series_uid_of_model(study_uid, model, profile_version);
    let identity = measurements_writer::MeasurementsIdentity {
        sop_instance_uid: measurements_uid_of_model(study_uid, model, profile_version),
        study_uid: study_uid.to_string(),
        series_uid: series_uid.clone(),
        images,
        patient_name: target.identity.patient_name.clone(),
        patient_id: target.identity.patient_id.clone(),
        patient_id_issuer: target.identity.patient_id_issuer.clone(),
        referring_physician: target.identity.referring_physician.clone(),
        study_id: target.identity.study_id.clone(),
        accession_number: target.identity.accession_number.clone(),
        station_name: target.identity.station_name.clone(),
        scheduled_protocol_code: target.identity.scheduled_workitem_code.clone(),
        requested_procedure_id: target.identity.requested_procedure_id.clone(),
        requested_procedure_description: target.identity.requested_procedure_description.clone(),
        requested_procedure_code: target.identity.requested_procedure_code.clone(),
        reason_for_requested_procedure: target.identity.reason_for_requested_procedure.clone(),
        reason_for_requested_procedure_code: target
            .identity
            .reason_for_requested_procedure_code
            .clone(),
        algorithm_name,
        algorithm_version,
        algorithm_manufacturer: None,
    };
    let profile = measurements_writer::MeasurementsProfile::of(&rule.result);
    let serialize_started = Instant::now();
    let bytes = measurements_writer::serialize(&identity, &profile, measurements);
    counters.cycle.observe(
        Stage::SerializeMeasurements,
        serialize_started.elapsed().as_secs_f64(),
    );
    origin.store_instance(&bytes).map_err(|error| {
        format!(
            "measurement report {} could not be stored: {error}",
            identity.sop_instance_uid
        )
    })?;
    let value_count: usize = measurements
        .groups
        .iter()
        .map(|group| group.measurements.len())
        .sum();
    println!(
        "external: {study_uid} model={} stored measurement report {} \
         ({} group(s), {value_count} value(s), unit {})",
        model.id,
        identity.sop_instance_uid,
        measurements.groups.len(),
        measurements.unit.code
    );
    // The document IS an Enhanced SR Measurement Report (measurements_writer.rs
    // writes (0008,0016)), so the reference names that class, plus the
    // measurements series it was written into.
    Ok(Some(OutputRef::new(
        measurements_writer::ENHANCED_SR_SOP_CLASS_UID,
        series_uid,
        identity.sop_instance_uid,
    )))
}

/// The GSPS SOP Instance UID of one frame's overlay.
///
/// Derived from the study, the model, the profile and the frame's POSITION,
/// so a re-run at the same profile reproduces the same UID (the origin then
/// treats it as the same object) and a bumped profile produces a genuinely
/// new one. The position rather than the SOP UID because the frame's UID is
/// used by the derived-image UIDs' neighbours and the two must not collide.
fn pr_uid_of_model(
    study_uid: &str,
    model: &crate::conf::Model,
    profile: u32,
    frame_number: u32,
) -> String {
    digest128(&format!(
        "external-pr:{study_uid}:{}:{profile}:{frame_number}",
        model.id
    ))
}

/// The series the overlays of one model and profile go into.
fn pr_series_of_model(study_uid: &str, model: &crate::conf::Model, profile: u32) -> String {
    digest128(&format!(
        "external-pr-series:{study_uid}:{}:{profile}",
        model.id
    ))
}

/// The SOP Instance UID of one external model's report.
fn sr_uid_of_model(study_uid: &str, model: &crate::conf::Model, profile: u32) -> String {
    digest128(&format!("external-sr:{study_uid}:{}:{profile}", model.id))
}

/// The series one external model's report goes into.
fn sr_series_uid_of_model(study_uid: &str, model: &crate::conf::Model, profile: u32) -> String {
    digest128(&format!(
        "external-sr-series:{study_uid}:{}:{profile}",
        model.id
    ))
}

/// The SOP Instance UID of one external model's measurement report.
///
/// A namespace of its own, not a suffix of the SR one: an SR and a
/// measurements report of the SAME model and profile describe two different
/// documents with different SOP classes, and deriving them from one string
/// would make them collide the moment both blocks are present.
fn measurements_uid_of_model(study_uid: &str, model: &crate::conf::Model, profile: u32) -> String {
    digest128(&format!(
        "external-measurements:{study_uid}:{}:{profile}",
        model.id
    ))
}

/// The series one external model's measurement report goes into.
fn measurements_series_uid_of_model(
    study_uid: &str,
    model: &crate::conf::Model,
    profile: u32,
) -> String {
    digest128(&format!(
        "external-measurements-series:{study_uid}:{}:{profile}",
        model.id
    ))
}

/// Claim a workitem: SCHEDULED -> IN PROGRESS with a freshly generated
/// Transaction UID, which the caller must echo on every later transition.
///
/// The 4xx family is NOT one thing here (PS3.18 Table 11.7.3-1):
///
/// - 409 Conflict is the state conflict - the workitem moved on, so another
///   claimant owns it and nothing went wrong. That is a conflict, and it is
///   what `clinfer_conflicts_total` counts.
/// - any other 4xx (400, 404, 410, ...) is the origin refusing the request
///   itself: a malformed body, a UID it does not have, a workitem that is
///   gone. Nothing will change on the next pass, so it is a failure and gets
///   its own counter and a log line naming the status.
///
/// Both leave the workitem alone; only the classification differs. A
/// transport failure or a 5xx is a failure for the same reason.
fn claim(origin: &Dicomweb, uid: &str, counters: &Counters) -> Result<String, Processed> {
    let transaction_uid = transaction_uid();
    match origin.update_state(
        uid,
        State::InProgress.as_str(),
        Some(&transaction_uid),
        None,
    ) {
        Ok(()) => {
            counters.claims.fetch_add(1, Ordering::Relaxed);
            Ok(transaction_uid)
        }
        Err(crate::dicomweb::Error::Status(status)) if status.status == 409 => {
            counters.conflicts.fetch_add(1, Ordering::Relaxed);
            // The same operator-facing line as the state check: this is
            // the path a RESTARTED sidecar takes when its predecessor
            // died mid-claim, and the UID is what has to be reset.
            eprintln!(
                "clinfer: workitem {uid} was refused by the origin ({status}); \
                 another claimant holds it, or an interrupted run left it IN \
                 PROGRESS. Reset its state through the origin's administration \
                 API if no other sidecar is running."
            );
            Err(Processed::Conflict)
        }
        Err(crate::dicomweb::Error::Status(status)) if (400..500).contains(&status.status) => {
            let message = status.to_string();
            counters.claim_failures.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "clinfer: workitem {uid}: the origin refused the claim with {message}; \
                 this is a defect signal, not a lost race - the request or the workitem \
                 itself is the problem"
            );
            Err(Processed::Failed(message))
        }
        Err(err) => Err(Processed::Failed(err.to_string())),
    }
}

/// (0010,0020) and (0010,0010) of the study's first SearchForStudies row.
///
/// The ID is an LO and arrives as a bare string, but the name is a PN and
/// DICOM JSON encodes it as `{"Value":[{"Alphabetic":".."}]}`; the
/// PN-aware reader handles that shape and a bare string alike.
fn patient_identity(study: &crate::json::Json) -> (Option<String>, Option<String>) {
    let id = study
        .attribute(crate::dicomweb::TAG_PATIENT_ID)
        .map(str::to_string);
    let name = study
        .attribute_pn(crate::dicomweb::TAG_PATIENT_NAME)
        .map(str::to_string);
    (id, name)
}

/// The identity the result writers put into the objects they produce.
///
/// AIW-I Appendix V Table V-1 lists these as values copied UNALTERED from
/// the UPS Workitem, and the text around the table adds that where the
/// workitem does not carry one, the implementation may take the value from
/// an instance in the Input Information Sequence instead. So each field is
/// resolved workitem-first and falls back to the study the workitem names -
/// which is what the writers did before Appendix V existed, and what keeps a
/// workitem that states nothing working exactly as it did.
///
/// A field the workitem carries as EMPTY wins over the study: "present, but
/// empty" is a statement ("this is unknown"), and Appendix V says the value
/// is copied unaltered rather than repaired.
#[derive(Debug, Clone, Default)]
struct ResultIdentity {
    patient_name: Option<String>,
    patient_id: Option<String>,
    /// (0010,0021) Issuer of Patient ID, workitem-only.
    ///
    /// Appendix V's "Issuer of Patient ID Macro" row is typed Source Copy,
    /// and the study the writers fall back on does not carry an issuer in
    /// the co-located flow, so this is read from the workitem alone:
    /// `Some("")` is the workitem stating "no issuer at this site" (the
    /// zero-length form Note 3 describes for the accession number), while
    /// `None` means the workitem said nothing and the element is left out.
    patient_id_issuer: Option<String>,
    patient_birth_date: Option<String>,
    patient_sex: Option<String>,
    study_id: Option<String>,
    accession_number: Option<String>,
    referring_physician: Option<String>,
    /// (0008,1010) Station Name, already resolved from (0040,4028). There
    /// is no study fallback for it: Appendix V maps it FROM the workitem's
    /// Performed Station Name Code Sequence, so a workitem without one
    /// leaves the attribute out.
    station_name: Option<String>,
    /// (0040,4018) Scheduled Workitem Code Sequence, workitem-only, for the
    /// result's Scheduled Protocol Code Sequence (0040,0008). Appendix V
    /// Table V-1 types the mapping `n.a.` on the UPS side and `Copy from:
    /// Scheduled Workitem Code Sequence (0040,4018)` on the result side, so
    /// it is a mapping FROM the workitem and has no study fallback.
    scheduled_workitem_code: Option<crate::dicomweb::WorkitemCode>,
    /// (0040,1001) Requested Procedure ID - workitem-first, no study
    /// fallback (the fallback source is the Input Information Sequence
    /// instance, which is the study the writers already hold; see
    /// [`ResultIdentity::resolve`]).
    requested_procedure_id: Option<String>,
    /// (0032,1060) Requested Procedure Description.
    requested_procedure_description: Option<String>,
    /// (0032,1064) Requested Procedure Code Sequence, one item.
    requested_procedure_code: Option<crate::dicomweb::WorkitemCode>,
    /// (0040,1002) Reason for the Requested Procedure.
    reason_for_requested_procedure: Option<String>,
    /// (0040,100A) Reason for Requested Procedure Code Sequence, one item.
    reason_for_requested_procedure_code: Option<crate::dicomweb::WorkitemCode>,
}

impl ResultIdentity {
    /// Resolve one workitem's identity against the study it operates on.
    fn resolve(workitem: &WorkItem, study: &crate::json::Json) -> Self {
        let wi = &workitem.identity;
        Self {
            patient_name: wi
                .patient_name
                .clone()
                .or_else(|| study.attribute_pn(TAG_PATIENT_NAME).map(str::to_string)),
            patient_id: wi
                .patient_id
                .clone()
                .or_else(|| study_text(study, TAG_PATIENT_ID)),
            // Workitem-only: no study fallback, because the co-located
            // study does not carry an issuer and inventing one would be a
            // false statement about the patient's identity domain.
            patient_id_issuer: wi.patient_id_issuer.clone(),
            patient_birth_date: wi
                .patient_birth_date
                .clone()
                .or_else(|| study_text(study, TAG_PATIENT_BIRTH_DATE)),
            patient_sex: wi
                .patient_sex
                .clone()
                .or_else(|| study_text(study, TAG_PATIENT_SEX)),
            study_id: wi
                .study_id
                .clone()
                .or_else(|| study_text(study, TAG_STUDY_ID)),
            accession_number: wi
                .accession_number
                .clone()
                .or_else(|| study_text(study, TAG_ACCESSION_NUMBER)),
            referring_physician: wi.referring_physician.clone().or_else(|| {
                study
                    .attribute_pn(TAG_REFERRING_PHYSICIAN)
                    .map(str::to_string)
            }),
            station_name: wi.performed_station_name.clone(),
            scheduled_workitem_code: wi.scheduled_workitem_code.clone(),
            // The Requested Procedure rows are workitem-only. Appendix V's
            // fallback licence points at "an appropriate instance listed in
            // the Input Information Sequence (0040,4021)", which in the
            // Clarus co-located flow IS the study the writers already hold -
            // and that study's requested-procedure attributes are not
            // captured by the parser (they are not manifest attributes), so
            // there is nothing to fall back TO. Emitting only what the
            // workitem states is the honest shape; see the report.
            requested_procedure_id: wi.requested_procedure_id.clone(),
            requested_procedure_description: wi.requested_procedure_description.clone(),
            requested_procedure_code: wi.requested_procedure_code.clone(),
            reason_for_requested_procedure: wi.reason_for_requested_procedure.clone(),
            reason_for_requested_procedure_code: wi.reason_for_requested_procedure_code.clone(),
        }
    }
}

/// A plain (non-PN) study attribute as text, for the Type-2 identity
/// attributes the result objects copy: an empty value on the origin reads
/// as "unknown" rather than as a value, so it becomes `None` here and the
/// writer emits a zero-length element.
fn study_text(study: &crate::json::Json, tag: &str) -> Option<String> {
    study
        .attribute(tag)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// The series of the study the matched rule runs on.
///
/// The rule's own modality and description wildcards are applied to the
/// series level: which series belong to this model is clinfer's internal
/// business, not a second workitem's. An empty pattern list means "every
/// series", the same semantics the study matcher has.
///
/// A non-array answer is an Err rather than an empty list: PS3.18 F.2.1
/// makes the result of a search a top-level array, so anything else is a
/// protocol violation, and reporting it as "no series matched" would fail
/// the workitem for the wrong reason - or, downstream, cancel it with "no
/// matching frames" while the origin held frames.
fn matching_series(
    document: &crate::json::Json,
    rule: &crate::conf::Rule,
) -> Result<Vec<String>, String> {
    use crate::dicomweb::{
        TAG_BODY_PART, TAG_MODALITY, TAG_SERIES_DESCRIPTION, TAG_SERIES_INSTANCE_UID,
    };
    use crate::json::Json;
    let Json::Arr(rows) = document else {
        return Err("series search answered a non-array; PS3.18 F.2.1 makes \
                    the result of a search a top-level JSON array"
            .to_string());
    };
    Ok(rows
        .iter()
        .filter_map(|row| {
            let uid = row.attribute(TAG_SERIES_INSTANCE_UID)?;
            if !rule.r#match.modality.is_empty()
                && !rule.r#match.modality.iter().any(|modality| {
                    modality.eq_ignore_ascii_case(row.attribute(TAG_MODALITY).unwrap_or_default())
                })
            {
                return None;
            }
            if !rule.r#match.study_description_contains.is_empty()
                && !rule
                    .r#match
                    .study_description_contains
                    .iter()
                    .any(|pattern| {
                        crate::matcher::glob(
                            pattern,
                            row.attribute(TAG_SERIES_DESCRIPTION).unwrap_or_default(),
                        )
                    })
            {
                return None;
            }
            // BodyPartExamined lives at the series level (PS3.3 C.7.3.1), so
            // this is where the rule's body-part wildcards apply. A series
            // that does not carry the attribute has an empty value, which no
            // positive pattern matches: the row is dropped, the study ends up
            // with no matching frame and the workitem is canceled on the
            // existing empty-series path.
            if !rule.r#match.body_part_contains.is_empty()
                && !rule.r#match.body_part_contains.iter().any(|pattern| {
                    crate::matcher::glob(pattern, row.attribute(TAG_BODY_PART).unwrap_or_default())
                })
            {
                return None;
            }
            Some(uid.to_string())
        })
        .collect())
}

/// Every `(0008,0018)` in a QIDO instance result, in order.
///
/// Non-array is an Err, for the reason [`matching_series`] gives: an empty
/// list here becomes a cancel with "no matching frames", which is a silent
/// wrong outcome when the origin answered an object.
fn sop_instance_uids(document: &crate::json::Json) -> Result<Vec<String>, String> {
    use crate::dicomweb::TAG_SOP_INSTANCE_UID;
    use crate::json::Json;
    let Json::Arr(rows) = document else {
        return Err("instance search answered a non-array; PS3.18 F.2.1 makes \
                    the result of a search a top-level JSON array"
            .to_string());
    };
    Ok(rows
        .iter()
        .filter_map(|row| row.attribute(TAG_SOP_INSTANCE_UID).map(str::to_string))
        .collect())
}

/// A 128-bit digest rendered as a decimal integer, from the crate's own
/// FNV-1a-based mixer.
///
/// A cryptographic hash is not needed and is not used: nothing here is a
/// security boundary. The requirement is that two different inputs give two
/// different UIDs with overwhelming probability across a site's studies,
/// which these 128 mixed bits do. The same mixer the Transaction UID uses
/// is reused so the crate keeps one hash, not two.
fn digest128(seed: &str) -> String {
    // Two independent 64-bit lanes over the same input with different
    // offsets, concatenated as one decimal number.
    let low = fnv1a64(seed.as_bytes(), 0xcbf2_9ce4_8422_2325);
    let high = fnv1a64(seed.as_bytes(), 0x8422_2325_cbf2_9ce4);
    let value = (u128::from(high) << 64) | u128::from(low);
    // Rooted under 2.25, like `transaction_uid`. A bare decimal digest is a
    // syntactically valid UID (PS3.5 9.1) but sits in no registered root, so
    // an origin that validates the org root - which is what "2.25" tells it
    // to expect - has no way to accept it, and two sites' digests share one
    // namespace. 2.25 is the UUID-derived arc reserved for exactly this.
    //
    // A u128 is at most 39 decimal digits, so the result is at most 44
    // characters: inside the 64 the standard allows.
    format!("2.25.{value}")
}

/// FNV-1a over `bytes`, with the given offset basis.
fn fnv1a64(bytes: &[u8], basis: u64) -> u64 {
    let mut hash = basis;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// Cancel a claimed workitem, echoing the claim's Transaction UID.
///
/// PS3.4 CC.2.1.2: the SCU records the UID it generated and uses it in every
/// later request. A cancel that sends none - or a different one - is refused
/// by a conformant origin, and the workitem stays IN PROGRESS with nobody
/// able to end it.
fn cancel(
    origin: &Dicomweb,
    uid: &str,
    transaction_uid: &str,
    reason: &str,
    counters: &Counters,
) -> Result<(), String> {
    match origin.update_state(
        uid,
        State::Canceled.as_str(),
        Some(transaction_uid),
        Some(reason),
    ) {
        Ok(()) => {
            counters.canceled.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        // The cancel did not reach the origin. The failure is reported as
        // such: counting it as a cancel would tell the operator a workitem
        // was closed when it is still IN PROGRESS.
        Err(err) => {
            eprintln!("clinfer: cancel of {uid} failed: {err}");
            Err(err.to_string())
        }
    }
}

/// A fresh Transaction UID for one claim.
///
/// PS3.4 CC.2.1.2 requires the SCU to generate it; a value derived from the
/// workitem UID would be the same on every sidecar and every restart, so it
/// would not identify the claimant at all. The digits come from process-local
/// entropy - the clock at nanosecond resolution and a counter, which keeps
/// two claims in the same nanosecond apart - and the shape is the `2.25` arc
/// the standard's digits-and-dots requirement allows.
fn transaction_uid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    // Mixing the counter into the low bits keeps two claims made in the same
    // nanosecond distinct without pulling in a random-number dependency.
    let mixed = nanos.wrapping_mul(0x100000001b3) ^ count.wrapping_mul(0x9e3779b97f4a7c15);
    format!("2.25.{mixed}")
}

/// Whether deciding this rule needs the study at all.
///
/// A rule that constrains ONLY the code is answered from the workitem, so
/// fetching the study would be pure cost. A rule that constrains the modality
/// or a study-level optional key (description, coded meaning, tag binding)
/// needs the study's attributes. The catch-all - a rule that constrains
/// nothing - needs nothing either, and matches regardless.
///
/// `body_part_contains` is deliberately absent: BodyPartExamined is applied at
/// the series level by [`matching_series`], so a rule that constrains only the
/// body part still matches at the study level and needs no study fetch.
///
/// When NO rule needs the study, the fetch is skipped entirely. That is the
/// point of making the code the first key: the whole read disappears on a
/// deployment whose rules route by the task's intent alone.
fn rule_needs_the_study(r#match: &crate::conf::Match) -> bool {
    !r#match.modality.is_empty()
        || !r#match.study_description_contains.is_empty()
        || !r#match.code_meaning_contains.is_empty()
        || !r#match.tags.is_empty()
}

/// The study attributes a rule matches on, kept from the study itself.
///
/// The workitem carries the step's own attributes and a reference to its
/// input; the clinical attributes belong to the study, so they are read from
/// there. A study that cannot be read is an `Err`: an empty attribute set
/// would satisfy an unconstrained rule and hide the origin's refusal, which
/// is the one thing the caller must not do.
fn study_attrs(
    origin: &Dicomweb,
    study_uid: &str,
    conf: &crate::conf::Conf,
) -> Result<StudyAttrs, String> {
    let document = match origin.study(study_uid) {
        Ok(document) => document,
        Err(err) => {
            eprintln!("clinfer: study {study_uid} could not be read: {err}");
            return Err(format!("study {study_uid} could not be read: {err}"));
        }
    };
    // The retrieve may answer with the study object or with a one-element
    // list (QIDO shape); both occur, so both are read.
    let study = document
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(&document);
    let modalities = study_modalities(study);
    if modalities.is_empty() {
        // A study row that declared no modality at all satisfies no
        // modality-constrained rule. Naming the study is what turns a
        // silent non-match into something an operator can act on: an
        // origin that omits (0008,0061) looks identical to a study that
        // genuinely has no modality.
        eprintln!(
            "clinfer: study {study_uid} declares no modality ((0008,0061) or \
             (0008,0060)); every modality-constrained rule will decline it"
        );
    }
    Ok(StudyAttrs {
        modalities,
        study_description: study
            .attribute(TAG_STUDY_DESCRIPTION)
            .unwrap_or("")
            .to_string(),
        body_part: study.attribute(TAG_BODY_PART).unwrap_or("").to_string(),
        code_meanings: study_code_meanings(study),
        tags: study_tag_bindings(study, conf),
        code: None,
    })
}

/// The (0008,0104) CodeMeaning values the study carries.
///
/// A rule keyed on `code_meaning_contains` asks "is this study about a
/// thorax", and the answer lives in whichever coded sequence the origin
/// returned - ProcedureCodeSequence, RequestedProcedureCodeSequence, a
/// series-level PerformedProtocolCodeSequence, or the study's own primary
/// code. Rather than name a sequence per site, every nested object is
/// walked and each (0008,0104) is collected: a coded sequence is an object
/// under a `Value` array, which is the only shape this walk descends into.
///
/// Depth is bounded so a malformed document cannot drive the walk deep, and
/// duplicates are dropped so one sequence repeated across series does not
/// weight a rule.
fn study_code_meanings(study: &crate::json::Json) -> Vec<String> {
    fn walk(node: &crate::json::Json, depth: usize, out: &mut Vec<String>) {
        // A coded sequence nests maybe four levels in practice; ten leaves
        // room for a private sequence without letting a crafted document
        // recurse.
        if depth > 10 {
            return;
        }
        if let Some(meaning) = node.attribute(TAG_CODE_MEANING) {
            let trimmed = meaning.trim();
            if !trimmed.is_empty() && !out.iter().any(|have| have == trimmed) {
                out.push(trimmed.to_string());
            }
        }
        let Some(entries) = node.object_entries() else {
            return;
        };
        for (_, value) in entries {
            match value {
                crate::json::Json::Arr(items) => {
                    for item in items {
                        walk(item, depth + 1, out);
                    }
                }
                crate::json::Json::Obj(_) => walk(value, depth + 1, out),
                _ => {}
            }
        }
    }

    let mut out = Vec::new();
    walk(study, 0, &mut out);
    out
}

/// The values of the tags the configured rules bind, as `(GGGGEEEE, value)`.
///
/// Only tags some rule actually binds are read: the study row is not
/// mined for everything it carries. A binding whose tag the study does not
/// return simply yields no entry, and `binding_matches` then reports no
/// match - which is the documented behaviour ("a study that does not carry
/// the tag does not match that binding"), not a crash and not a wildcard.
///
/// A sequence-valued tag contributes its first item's CodeMeaning when it
/// has one, because that is what a site means when it binds a code
/// sequence; otherwise its first string value.
fn study_tag_bindings(
    study: &crate::json::Json,
    conf: &crate::conf::Conf,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for rule in &conf.rules {
        for binding in &rule.r#match.tags {
            let tag = binding.tag.to_ascii_uppercase();
            if out.iter().any(|(have, _)| have.eq_ignore_ascii_case(&tag)) {
                continue;
            }
            let Some(value) = study_tag_value(study, &tag) else {
                continue;
            };
            out.push((tag, value));
        }
    }
    out
}

/// One study tag's matchable value, in the form a binding pattern expects.
fn study_tag_value(study: &crate::json::Json, tag: &str) -> Option<String> {
    if let Some(value) = study.attribute(tag) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    // A coded or other item-valued entry: prefer its meaning, then its
    // first bare string, so a binding can name either.
    let item = study.attribute_item(tag)?;
    if let Some(meaning) = item.attribute(TAG_CODE_MEANING) {
        let trimmed = meaning.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let first = item.first_str()?.trim();
    (!first.is_empty()).then(|| first.to_string())
}

/// The study's modalities, upper-cased.
///
/// (0008,0061) ModalitiesInStudy is the study-level matching attribute for
/// modality (PS3.18 Table 10.6.1-5, 2026c); (0008,0060) Modality is
/// SERIES-level and a conformant origin need not return it on a study row
/// at all.
///
/// (0008,0061) is therefore the AUTHORITY when it is present, and
/// (0008,0060) is read only as a fallback for an origin that does not
/// return the study-level attribute. The two are not unioned: a row that
/// declares both would otherwise let a series-level value add a modality
/// the study itself does not claim, which is the level confusion this fix
/// exists to remove.
///
/// A row that carries neither yields an empty list, and every
/// modality-constrained rule then declines the study - an unknown modality
/// is not a match.
fn study_modalities(study: &crate::json::Json) -> Vec<String> {
    let values_of = |tag: &str| -> Vec<String> {
        let Some(value) = study.get(tag).and_then(|entry| entry.get("Value")) else {
            return Vec::new();
        };
        let Some(items) = value.as_array() else {
            return Vec::new();
        };
        items
            .iter()
            .filter_map(|item| item.as_str())
            .map(str::to_ascii_uppercase)
            .filter(|value| !value.is_empty())
            .collect()
    };
    let study_level = values_of(TAG_MODALITIES_IN_STUDY);
    if study_level.is_empty() {
        values_of(TAG_MODALITY)
    } else {
        study_level
    }
}

/// The startup catch-up: read every workitem the origin still offers.
///
/// This is what makes a push an optimisation rather than a dependency. It
/// runs once, before the worker starts taking pushes, so a study stored
/// while the sidecar was down is not waiting behind a push that will never
/// come.
///
/// The return value is the real tally per outcome. Counting one number and
/// calling it "claimed" reported zero for a run that had claimed and then
/// cancelled, which is how the first live run looked like it did nothing.
/// The per-outcome counts of one catch-up pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tallies {
    /// Workitems driven to COMPLETED.
    pub completed: usize,
    /// Workitems claimed and then CANCELED.
    pub canceled: usize,
    /// Workitems another claimant held (IN PROGRESS, not offered).
    pub conflicts: usize,
    /// Workitems that could not be processed.
    pub failed: usize,
    /// Workitems left alone because the rules asked for nothing.
    pub left_alone: usize,
    /// Workitems already in a terminal state, so there was nothing to do.
    /// Telemetry only: the standard has no notion of a conflict for a
    /// workitem someone else already finished.
    pub skipped: usize,
}

/// What one catch-up pass did, folded into a tally.
fn tally(tallies: &mut Tallies, outcome: &Processed) {
    match outcome {
        Processed::Claimed => tallies.completed += 1,
        Processed::Canceled(_) => tallies.canceled += 1,
        Processed::Conflict => tallies.conflicts += 1,
        Processed::Skipped => tallies.skipped += 1,
        Processed::Failed(_) => tallies.failed += 1,
        Processed::LeftAlone => tallies.left_alone += 1,
    }
}

/// Process one workitem and report the result.
///
/// The one place a `Processed::Failed` becomes a log line, so the
/// coordinator and the catch-up cannot phrase the same failure differently.
fn process_and_report(
    origin: &Dicomweb,
    conf: &Conf,
    uid: &str,
    counters: &Counters,
    channels: &Topology,
) -> Processed {
    let outcome = process(origin, conf, uid, counters, channels);
    // A cancel carries a REASON, and the reason is the operator's only
    // clue about what a model script did wrong (the origin keeps it in
    // (0074,1238), but nobody debugs a script by querying the origin).
    // Every outcome that is not a clean claim is logged here, phrased
    // once, so the coordinator and the catch-up cannot differ.
    match &outcome {
        Processed::Failed(message) => eprintln!("clinfer: workitem {uid}: {message}"),
        Processed::Canceled(reason) => eprintln!("clinfer: workitem {uid} canceled: {reason}"),
        _ => {}
    }
    outcome
}

/// The workitem UIDs the origin currently offers this station.
///
/// `None` when even the unfiltered search failed, which is the only case
/// where the caller has nothing to do.
fn offered_workitems(origin: &Dicomweb, conf: &Conf) -> Option<Vec<String>> {
    // The station-oriented search is what AIW-I 4.81.4.1.2.2 asks for; it
    // is not a precondition for doing the work. The unfiltered search is
    // the fallback and the local rules are what decide.
    let fallback = |reason: &str| -> Option<Vec<String>> {
        eprintln!(
            "clinfer: station-oriented search {reason}; falling back to the \
             unfiltered workitem search"
        );
        match origin.search_workitems() {
            Ok(uids) => Some(uids),
            Err(err) => {
                eprintln!("clinfer: startup search failed: {err}");
                None
            }
        }
    };
    match origin.search_workitems_for_station(&conf.server.station_aet) {
        // Only a non-empty queue proves the narrowing ran. An EMPTY answer
        // is not "no work": the key names this station, and a workitem that
        // carries no assignment at all (an offer to whoever can serve it)
        // cannot appear in a queue narrowed to our station, so an origin
        // that accepts the key still hides unassigned work behind an empty
        // list. A refusal is the same shape of dead end.
        Ok(uids) if !uids.is_empty() => Some(uids),
        Ok(_) => fallback("returned an empty queue"),
        Err(err) => fallback(&format!("failed ({err})")),
    }
}

pub fn catch_up_with(
    origin: &Dicomweb,
    conf: &Conf,
    counters: &Counters,
    channels: &Topology,
) -> Tallies {
    let Some(uids) = offered_workitems(origin, conf) else {
        return Tallies::default();
    };
    let mut tallies = Tallies::default();
    for uid in uids {
        let outcome = process_and_report(origin, conf, &uid, counters, channels);
        tally(&mut tallies, &outcome);
    }
    tallies
}

/// Run the coordinator loop until `shutdown` is set.
pub fn run_with(
    origin: Arc<Dicomweb>,
    shared: Arc<Shared>,
    queue: Receiver<String>,
    counters: Arc<Counters>,
    channels: Topology,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Relaxed) {
        match queue.recv_timeout(IDLE_TIMEOUT) {
            Ok(uid) => {
                let conf = shared.current();
                process_and_report(&origin, &conf, &uid, &counters, &channels);
            }
            Err(RecvTimeoutError::Timeout) => continue,
            // The sender lives in the listener; it ending means the
            // process is winding down.
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Spawn the coordinator on its own thread.
pub fn spawn_with(
    origin: Arc<Dicomweb>,
    shared: Arc<Shared>,
    queue: Receiver<String>,
    counters: Arc<Counters>,
    channels: Topology,
) -> (thread::JoinHandle<()>, Arc<AtomicBool>) {
    let shutdown = Arc::new(AtomicBool::new(false));
    let handle = thread::spawn({
        let shutdown = Arc::clone(&shutdown);
        move || run_with(origin, shared, queue, counters, channels, shutdown)
    });
    (handle, shutdown)
}

/// The closure the notification channel calls after a reconnect.
///
/// It ENQUEUES the UIDs the search found instead of processing them, which
/// is what makes the coordinator the only claimer by construction. The
/// claim is arbitration at the origin (409), so a second thread claiming in
/// parallel would leave "one claim per workitem" true only by timing.
///
/// A blocking send is honest backpressure: a UID that was queued will be
/// claimed, while a dropped one would be a silently lost workitem - exactly
/// the case this search exists to cover. An enqueue can only block while
/// the queue is full, and the coordinator is still draining it (the notifier
/// is joined before the coordinator is told to stop), so the wait is bounded
/// by the coordinator's current workitem.
#[must_use = "the returned closure is the redeliver hook; dropping it means the channel never re-searches"]
pub fn redeliver_into(
    origin: Arc<Dicomweb>,
    conf: Arc<Conf>,
    queue: SyncSender<String>,
) -> impl Fn() -> usize {
    move || {
        let Some(uids) = offered_workitems(&origin, &conf) else {
            return 0;
        };
        let mut sent = 0usize;
        for uid in uids {
            // A disconnected queue means the coordinator is gone and the
            // process is winding down: stop rather than spin, and let the
            // next start's catch-up find the rest.
            if queue.send(uid).is_err() {
                break;
            }
            sent += 1;
        }
        sent
    }
}

/// The workitem UID of a pushed body, for tests and the receiver.
///
/// The pushed body is an UPS Create Workitem, whose identity is the SOP
/// Instance UID (0008,0018) - the same tag the retrieve returns.
#[must_use]
pub fn pushed_uid(workitem: &crate::json::Json) -> Option<String> {
    workitem.attribute(TAG_SOP_INSTANCE_UID).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a workitem from a DICOM JSON body, the way the origin's
    /// retrieve route hands one over.
    fn workitem(body: &str) -> WorkItem {
        let parsed = crate::json::parse(body.as_bytes()).expect("parseable workitem");
        WorkItem::from_json(&parsed).expect("a workitem has a SOP Instance UID")
    }

    /// The study row the writers fall back on, second in the resolution.
    fn study(body: &str) -> crate::json::Json {
        crate::json::parse(body.as_bytes()).expect("parseable study")
    }

    #[test]
    fn the_workitem_identity_wins_over_the_study() {
        // AIW-I Appendix V Table V-1 types these rows "Source Copy" from
        // the UPS Workitem, so a workitem that states them decides. The
        // two sources deliberately DISAGREE: a resolver that read the
        // study first would still pass a test where they agree.
        let wi = workitem(
            r#"{"00080018":{"vr":"UI","Value":["2.25.1"]},
                "00100010":{"vr":"PN","Value":[{"Alphabetic":"WORKITEM^PAT"}]},
                "00100020":{"vr":"LO","Value":["WI-ID"]},
                "00100030":{"vr":"DA","Value":["19800101"]},
                "00100040":{"vr":"CS","Value":["M"]},
                "00200010":{"vr":"SH","Value":["WI-STUDY"]},
                "00080050":{"vr":"SH","Value":["WI-ACC"]},
                "00080090":{"vr":"PN","Value":[{"Alphabetic":"WI^REF"}]}}"#,
        );
        let st = study(
            r#"{"00100010":{"vr":"PN","Value":[{"Alphabetic":"STUDY^PAT"}]},
                "00100020":{"vr":"LO","Value":["ST-ID"]},
                "00100030":{"vr":"DA","Value":["19900101"]},
                "00100040":{"vr":"CS","Value":["F"]},
                "00200010":{"vr":"SH","Value":["ST-STUDY"]},
                "00080050":{"vr":"SH","Value":["ST-ACC"]},
                "00080090":{"vr":"PN","Value":[{"Alphabetic":"ST^REF"}]}}"#,
        );

        let resolved = ResultIdentity::resolve(&wi, &st);
        assert_eq!(resolved.patient_name.as_deref(), Some("WORKITEM^PAT"));
        assert_eq!(resolved.patient_id.as_deref(), Some("WI-ID"));
        assert_eq!(resolved.patient_birth_date.as_deref(), Some("19800101"));
        assert_eq!(resolved.patient_sex.as_deref(), Some("M"));
        assert_eq!(resolved.study_id.as_deref(), Some("WI-STUDY"));
        assert_eq!(resolved.accession_number.as_deref(), Some("WI-ACC"));
        assert_eq!(resolved.referring_physician.as_deref(), Some("WI^REF"));
    }

    #[test]
    fn a_silent_workitem_falls_back_to_the_study() {
        // The regression pin. A workitem that states no identity is the
        // shape every deployment that never populates these attributes
        // runs in, and it must behave exactly as it did before Appendix V:
        // the study supplies the values.
        let wi = workitem(
            r#"{"00080018":{"vr":"UI","Value":["2.25.1"]},
                "00404021":{"vr":"SQ","Value":[{"0020000D":{"vr":"UI","Value":["2.25.9"]}}]}}"#,
        );
        let st = study(
            r#"{"00100010":{"vr":"PN","Value":[{"Alphabetic":"STUDY^PAT"}]},
                "00100020":{"vr":"LO","Value":["ST-ID"]},
                "00100030":{"vr":"DA","Value":["19900101"]},
                "00100040":{"vr":"CS","Value":["F"]},
                "00200010":{"vr":"SH","Value":["ST-STUDY"]},
                "00080050":{"vr":"SH","Value":["ST-ACC"]}}"#,
        );

        let resolved = ResultIdentity::resolve(&wi, &st);
        assert_eq!(resolved.patient_name.as_deref(), Some("STUDY^PAT"));
        assert_eq!(resolved.patient_id.as_deref(), Some("ST-ID"));
        assert_eq!(resolved.patient_birth_date.as_deref(), Some("19900101"));
        assert_eq!(resolved.patient_sex.as_deref(), Some("F"));
        assert_eq!(resolved.study_id.as_deref(), Some("ST-STUDY"));
        assert_eq!(resolved.accession_number.as_deref(), Some("ST-ACC"));
        assert_eq!(resolved.referring_physician, None, "no fallback value");
    }

    #[test]
    fn the_station_name_comes_only_from_the_workitem() {
        // Appendix V Note 4 maps (0008,1010) FROM the workitem's
        // (0040,4028), so there is no study fallback. The study's own
        // institution/station data must not leak in.
        let wi = workitem(
            r#"{"00080018":{"vr":"UI","Value":["2.25.1"]},
                "00404028":{"vr":"SQ","Value":[{"00080100":{"vr":"SH","Value":["CLINFER"]},
                                                "00080102":{"vr":"SH","Value":["99CLARUS"]},
                                                "00080104":{"vr":"LO","Value":["Inference station"]}}]}}"#,
        );
        let st = study(r#"{"00081010":{"vr":"SH","Value":["SOMETHING ELSE"]}}"#);
        assert_eq!(
            ResultIdentity::resolve(&wi, &st).station_name.as_deref(),
            Some("Inference station"),
            "the Code Meaning is what maps into (0008,1010)"
        );

        // And a workitem without the sequence gets nothing, not the study.
        let bare = workitem(r#"{"00080018":{"vr":"UI","Value":["2.25.1"]}}"#);
        assert_eq!(ResultIdentity::resolve(&bare, &st).station_name, None);
    }

    #[test]
    fn a_configuration_hole_is_found_before_anything_is_dispatched() {
        // Rule A is dispatchable, rule B names a model that is in the file
        // but has no thread. The hole is knowable before the first send, so
        // the chain must fail with NOTHING stored: an object written for a
        // workitem that is then cancelled is a result for a step the
        // archive reports as not done.
        let origin = Arc::new(Dicomweb::new(
            crate::http::Client::new("http://127.0.0.1:1", "user").expect("a parseable base URL"),
        ));
        let counters = Arc::new(Counters::default());

        // A real channel for model A. A background thread answers whatever
        // arrives, so a dispatch of rule A would produce a REAL object and
        // fill `produced` - which is exactly what the pre-pass must prevent.
        // The timeout is short: with the pre-pass nothing ever arrives, and
        // the test should not spend seconds proving an absence.
        let (sender_a, receiver_a) = std::sync::mpsc::sync_channel::<Job>(1);
        let answerer = std::thread::spawn(move || {
            if let Ok(job) = receiver_a.recv_timeout(std::time::Duration::from_millis(250)) {
                let sent = job
                    .completion
                    .send((
                        job.slot,
                        Ok(vec![OutputRef {
                            sop_class_uid: "1.2.840.10008.5.1.4.1.1.11.1".to_string(),
                            series_instance_uid: "1.2.3.99".to_string(),
                            sop_instance_uid: "1.2.3.99.1".to_string(),
                        }]),
                    ))
                    .is_ok();
                (true, sent)
            } else {
                (false, false)
            }
        });
        let mut senders = std::collections::HashMap::new();
        senders.insert("a".to_string(), sender_a);
        let channels = Topology {
            senders,
            semaphore: Arc::new(Semaphore::new(1)),
            origin: Arc::clone(&origin),
            counters: Arc::clone(&counters),
        };

        let mut conf = Conf {
            server: crate::conf::ServerConf {
                base_url: "http://origin".to_string(),
                identity_user: "user".to_string(),
                listen: "127.0.0.1:0".to_string(),
                trusted_proxies: Vec::new(),
                require_proxy_identity: false,
                station_aet: "CLINFER".to_string(),
                station_name: "CLINFER".to_string(),
                poll_interval_secs: 0,
            },
            engine: crate::conf::EngineConf {
                cache_dir: std::path::PathBuf::from("."),
                workers: None,
            },
            models: vec![model("a"), model("b")],
            rules: Vec::new(),
            default_rule: crate::conf::DefaultRule::Off,
        };
        for (index, id) in ["a", "b"].iter().enumerate() {
            conf.rules.push(crate::conf::Rule {
                r#match: crate::conf::Match {
                    modality: vec![],
                    code: vec![],
                    study_description_contains: vec![],
                    code_meaning_contains: vec![],
                    body_part_contains: vec![],
                    tags: vec![],
                },
                model: (*id).to_string(),
                result: crate::conf::RuleResult {
                    kind: crate::conf::ResultKind::Pr,
                    color: None,
                    profile_version: 1,
                    series_description: None,
                    label: crate::conf::LabelProfile::default(),
                    sr: None,
                    measurements: None,
                },
                index,
            });
        }

        let wi = workitem(r#"{"00080018":{"vr":"UI","Value":["2.25.1"]}}"#);
        let mut produced: Vec<OutputRef> = Vec::new();
        let outcome = run_chain(&channels, &conf, &wi, &[0, 1], &mut produced);

        let reason = outcome.expect_err("rule B names a model with no thread");
        // The load-bearing assertion: nothing may be stored. Rule A would
        // produce an object if it were dispatched before the hole at rule B
        // was found, and that object would belong to a workitem the caller
        // is about to cancel.
        assert!(
            produced.is_empty(),
            "no rule may store before the configuration hole is found: {} object(s)",
            produced.len()
        );
        assert!(
            reason.contains("b") && reason.contains(NO_THREAD),
            "the failure names the model hole: {reason}"
        );
        // The answerer thread reports whether rule A was ever dispatched.
        // With the pre-pass it never is, so it times out; without it, it
        // answers a job and `produced` above would not be empty.
        let (dispatched, answered) = answerer.join().expect("the answerer thread");
        assert!(
            !dispatched,
            "rule A must not have been dispatched (reason: {reason}, answered: {answered})"
        );
    }

    #[test]
    fn a_zero_length_workitem_value_is_not_replaced_by_the_study() {
        // Appendix V copies values UNALTERED, and DICOM JSON distinguishes
        // "absent" from "present, empty". A workitem that states an empty
        // Patient ID is saying "unknown"; overwriting it with the study's
        // value would contradict the source it is told to copy from.
        let wi = workitem(
            r#"{"00080018":{"vr":"UI","Value":["2.25.1"]},
                "00100020":{"vr":"LO","Value":[""]}}"#,
        );
        let st = study(r#"{"00100020":{"vr":"LO","Value":["ST-ID"]}}"#);
        assert_eq!(
            ResultIdentity::resolve(&wi, &st).patient_id.as_deref(),
            Some(""),
            "an empty value is copied, not repaired"
        );
    }

    #[test]
    fn a_derived_frames_bytes_round_the_bit_depth_up_to_whole_bytes() {
        // A 12-bit sample occupies two bytes, not one and a half. Truncating
        // the division makes a 12-bit frame look half its real size, and
        // every 12-bit derived image is then refused as too large.
        let geometry = |bits: u16| Geometry {
            rows: 4,
            cols: 4,
            samples: 1,
            bits_allocated: bits,
            bits_stored: bits as u8,
            frames: 1,
            inverted: false,
            signed: false,
        };
        assert_eq!(derived_bytes(&geometry(8)), 16, "4x4 at 8 bits");
        assert_eq!(derived_bytes(&geometry(16)), 32, "4x4 at 16 bits");
        assert_eq!(
            derived_bytes(&geometry(12)),
            32,
            "4x4 at 12 bits is still two bytes a sample"
        );
        // Three samples a pixel is the colour case.
        let mut rgb = geometry(8);
        rgb.samples = 3;
        assert_eq!(derived_bytes(&rgb), 48);
    }

    #[test]
    fn each_claim_generates_a_fresh_transaction_uid() {
        // The UID identifies the claimant, so two claims never present the
        // same one - not even back to back in the same nanosecond.
        let first = transaction_uid();
        let second = transaction_uid();
        assert_ne!(first, second, "a claim must not reuse another's UID");
        // A UID is digits and dots, and short enough for the 64-character
        // limit.
        for uid in [&first, &second] {
            assert!(uid.starts_with("2.25."));
            assert!(uid.chars().all(|c| c.is_ascii_digit() || c == '.'));
            assert!(uid.len() <= 64, "{uid}");
        }
    }

    #[test]
    fn a_finished_chain_completes_only_when_it_produced_something() {
        // D-7, the rule itself. COMPLETED carries (0040,4033), so it is
        // only correct when there is something to name. A chain that
        // succeeded and stored nothing - a detector that found nothing, or
        // a result the cache marker already recorded - cancels instead.
        assert_eq!(
            transition(
                None,
                &[OutputRef::new(
                    "1.2.840.10008.5.1.4.1.1.11.1",
                    "2.25.100",
                    "2.25.1"
                )]
            ),
            Transition::Complete,
            "an output means COMPLETED"
        );
        assert_eq!(
            transition(None, &[]),
            Transition::CancelEmpty,
            "no output means CANCELED, not COMPLETED with an empty list"
        );
        assert_eq!(
            transition(Some("engine is not available"), &[]),
            Transition::CancelFailed,
            "a failed chain cancels with its own reason"
        );
        // A failed chain that somehow stored something still cancels: the
        // failure is the workitem's outcome, and a half-delivered study
        // must not be reported as complete.
        assert_eq!(
            transition(
                Some("store refused"),
                &[OutputRef::new(
                    "1.2.840.10008.5.1.4.1.1.11.1",
                    "2.25.100",
                    "2.25.1"
                )]
            ),
            Transition::CancelFailed
        );
    }

    #[test]
    fn an_empty_completion_is_never_chosen() {
        // The property the counterexample would break: there is no input
        // for which the choice is "complete with nothing to name".
        for produced in [
            Vec::<OutputRef>::new(),
            vec![OutputRef::new(
                "1.2.840.10008.5.1.4.1.1.11.1",
                "2.25.100",
                "2.25.1",
            )],
            vec![
                OutputRef::new("1.2.840.10008.5.1.4.1.1.11.1", "2.25.100", "2.25.1"),
                OutputRef::new("1.2.840.10008.5.1.4.1.1.11.1", "2.25.101", "2.25.2"),
            ],
        ] {
            for reason in [None, Some("failed")] {
                let chosen = transition(reason, &produced);
                if chosen == Transition::Complete {
                    assert!(
                        !produced.is_empty(),
                        "COMPLETED was chosen with an empty output list"
                    );
                }
            }
        }
    }

    #[test]
    fn a_terminal_state_is_skipped_and_an_in_progress_state_is_a_conflict() {
        // N-13: the two non-SCHEDULED states mean different things. A
        // COMPLETED/CANCELED workitem is finished - nothing to do, and not
        // contention. IN PROGRESS is held by someone else, which IS.
        for state in ["COMPLETED", "CANCELED"] {
            assert!(
                matches!(
                    State::parse(state),
                    Some(State::Completed) | Some(State::Canceled)
                ),
                "{state} must be recognised as terminal"
            );
        }
        assert_eq!(
            State::parse("IN PROGRESS"),
            Some(State::InProgress),
            "which is the conflict case"
        );
    }

    #[test]
    fn a_transaction_uid_is_not_derived_from_the_workitem() {
        // A derived value would be the same on every sidecar and after a
        // restart, which would not identify the claimant at all. Two sidecars
        // claiming the same workitem must present different UIDs.
        let a = transaction_uid();
        let b = transaction_uid();
        assert_ne!(a, b);
    }

    #[test]
    fn the_shared_configuration_can_be_replaced() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("clinfer.conf.example.en");
        let conf = crate::conf::load(&path).expect("parses");
        let shared = Shared::new(Arc::new(conf));
        assert_eq!(shared.current().rules.len(), 1);

        let mut other = (*shared.current()).clone();
        other.default_rule = crate::conf::DefaultRule::Off;
        shared.replace(Arc::new(other));
        assert_eq!(shared.current().default_rule, crate::conf::DefaultRule::Off);
    }

    #[test]
    fn a_pushed_workitem_yields_its_uid() {
        // The pushed body is an UPS Create Workitem: identity in (0008,0018).
        let document = crate::json::parse(br#"{"00080018":{"Value":["9.9"]}}"#).expect("parses");
        assert_eq!(pushed_uid(&document).as_deref(), Some("9.9"));
    }

    #[test]
    fn the_patient_type_2_attributes_read_the_source_row() {
        let filled = crate::json::parse(
            br#"{"00100030":{"Value":["19700101"]},"00100040":{"Value":["O"]}}"#,
        )
        .expect("parses");
        assert_eq!(
            study_text(&filled, crate::dicomweb::TAG_PATIENT_BIRTH_DATE).as_deref(),
            Some("19700101")
        );
        assert_eq!(
            study_text(&filled, crate::dicomweb::TAG_PATIENT_SEX).as_deref(),
            Some("O")
        );

        let absent = crate::json::parse(br#"{}"#).expect("parses");
        assert_eq!(
            study_text(&absent, crate::dicomweb::TAG_PATIENT_BIRTH_DATE),
            None
        );
    }

    #[test]
    fn the_patient_identity_reads_the_pn_object_shape() {
        // A conformant origin encodes PN as an object with Alphabetic, not
        // a bare string: reading it with the plain string reader would drop
        // the name and Weasis would group the PR under "unknown".
        let study = crate::json::parse(
            br#"{"00100020":{"Value":["CLIN-DEMO-001"],"vr":"LO"},
                 "00100010":{"Value":[{"Alphabetic":"Clinfer^Transport^Demo"}],"vr":"PN"}}"#,
        )
        .expect("parses");
        let (id, name) = patient_identity(&study);
        assert_eq!(id.as_deref(), Some("CLIN-DEMO-001"));
        assert_eq!(name.as_deref(), Some("Clinfer^Transport^Demo"));
    }

    #[test]
    fn the_patient_identity_accepts_a_bare_string_name() {
        // Older or simplified origins return the PN value as a bare string;
        // the PN-aware reader must not reject it.
        let study = crate::json::parse(
            br#"{"00100020":{"Value":["CLIN-DEMO-001"]},
                 "00100010":{"Value":["Fixture^Bare"]}}"#,
        )
        .expect("parses");
        let (id, name) = patient_identity(&study);
        assert_eq!(id.as_deref(), Some("CLIN-DEMO-001"));
        assert_eq!(name.as_deref(), Some("Fixture^Bare"));
    }

    #[test]
    fn an_instance_row_reads_its_own_uid_not_the_series() {
        // A conformant origin may carry (0020,000E) on the instance rows;
        // the instance URL's UID must come from (0008,0018) regardless.
        let rows = crate::json::parse(
            br#"[{"00080018":{"Value":["1.1.1"]},"0020000E":{"Value":["2.2"]}},
                 {"00080018":{"Value":["1.1.2"]},"0020000E":{"Value":["2.2"]}}]"#,
        )
        .expect("parses");
        assert_eq!(
            sop_instance_uids(&rows).expect("an array"),
            vec!["1.1.1".to_string(), "1.1.2".to_string()],
            "the SOP Instance UID wins over the series UID on an instance row"
        );
    }

    #[test]
    fn an_instance_row_without_its_own_uid_yields_nothing() {
        // A row that names only the series is not an instance: the worker
        // must report "series has no instances" rather than fetch the
        // series UID as an instance.
        let rows = crate::json::parse(br#"[{"0020000E":{"Value":["2.2"]}}]"#).expect("parses");
        assert!(sop_instance_uids(&rows).expect("an array").is_empty());
    }

    #[test]
    fn a_non_array_instance_search_is_an_error_not_an_empty_list() {
        // Owner decision A: a search answers an array (PS3.18 F.2.1), so an
        // object is a protocol violation. Returning an empty list would
        // become a cancel with "no matching frames" while the origin held
        // frames - a silent wrong outcome, which is what Err prevents.
        let object = crate::json::parse(br#"{"00080018":{"Value":["1.1.1"]}}"#).expect("parses");
        let error = sop_instance_uids(&object).expect_err("an object is refused");
        assert!(error.contains("non-array"), "the reason: {error}");

        let scalar = crate::json::parse(br#""#).unwrap_or(crate::json::Json::Null);
        assert!(
            sop_instance_uids(&scalar).is_err(),
            "a scalar is refused too"
        );
    }

    #[test]
    fn an_empty_instance_search_is_a_real_answer() {
        // An EMPTY array is conformant and means "no instances"; only a
        // non-array is an error. This is the boundary the change must not
        // move.
        let rows = crate::json::parse(b"[]").expect("parses");
        assert_eq!(sop_instance_uids(&rows), Ok(Vec::new()));
    }

    #[test]
    fn the_rule_filters_series_by_its_own_wildcards() {
        // Which series of a matching study belong to this model is
        // clinfer's internal business: the rule's modality and description
        // wildcards are applied to the series level.
        let rule = crate::conf::Rule {
            r#match: crate::conf::Match {
                modality: vec!["DX".to_string()],
                code: vec![],
                study_description_contains: vec!["*chest*".to_string()],
                code_meaning_contains: vec![],
                body_part_contains: vec![],
                tags: vec![],
            },
            model: "cxr10".to_string(),
            result: crate::conf::RuleResult {
                kind: crate::conf::ResultKind::Pr,
                color: None,
                profile_version: 1,
                series_description: None,
                label: crate::conf::LabelProfile::default(),
                sr: None,
                measurements: None,
            },
            index: 0,
        };
        let rows = crate::json::parse(
            br#"[{"0020000E":{"Value":["s1"]},"00080060":{"Value":["DX"]},"0008103E":{"Value":["ChestX pathology"]}},
                 {"0020000E":{"Value":["s2"]},"00080060":{"Value":["DX"]},"0008103E":{"Value":["ChestX normal"]}},
                 {"0020000E":{"Value":["s3"]},"00080060":{"Value":["US"]},"0008103E":{"Value":["ChestX echo"]}}]"#,
        )
        .expect("parses");
        assert_eq!(
            matching_series(&rows, &rule).expect("an array"),
            vec!["s1".to_string(), "s2".to_string()],
            "DX chest series pass; a US series does not"
        );
    }

    #[test]
    fn the_rule_filters_series_by_body_part() {
        // BodyPartExamined (0018,0015) is a SERIES attribute (PS3.3 C.7.3.1),
        // so the rule's body-part wildcards apply here: a LUNG series passes a
        // "*lung*" rule, and a series that does not carry the attribute is
        // dropped (its empty value matches no positive pattern).
        let mut rule = series_rule(vec![]);
        rule.r#match.body_part_contains = vec!["*lung*".to_string()];
        let rows = crate::json::parse(
            br#"[{"0020000E":{"Value":["s1"]},"00080060":{"Value":["DX"]},"00180015":{"Value":["LUNG"]}},
                 {"0020000E":{"Value":["s2"]},"00080060":{"Value":["DX"]},"00180015":{"Value":["KNEE"]}},
                 {"0020000E":{"Value":["s3"]},"00080060":{"Value":["DX"]}}]"#,
        )
        .expect("parses");
        assert_eq!(
            matching_series(&rows, &rule).expect("an array"),
            vec!["s1".to_string()],
            "the LUNG series passes; KNEE and the absent attribute do not"
        );
    }

    #[test]
    fn a_series_without_a_body_part_satisfies_no_positive_pattern() {
        // The knee study carries no BodyPartExamined: a "*lung*" rule drops
        // every series, which is the empty-series path that cancels the
        // workitem with "no matching frames".
        let mut rule = series_rule(vec![]);
        rule.r#match.body_part_contains = vec!["*lung*".to_string()];
        let rows = crate::json::parse(
            br#"[{"0020000E":{"Value":["s1"]},"00080060":{"Value":["DX"]}},
                 {"0020000E":{"Value":["s2"]},"00080060":{"Value":["DX"]},"00180015":{"Value":["KNEE"]}}]"#,
        )
        .expect("parses");
        assert_eq!(
            matching_series(&rows, &rule).expect("an array"),
            Vec::<String>::new(),
            "no series carries LUNG, so none is selected"
        );
    }

    /// A minimal rule for the `matching_series` tests: a modality list and no
    /// other constraint unless a case adds one.
    fn series_rule(modality: Vec<String>) -> crate::conf::Rule {
        crate::conf::Rule {
            r#match: crate::conf::Match {
                modality,
                code: vec![],
                study_description_contains: vec![],
                code_meaning_contains: vec![],
                body_part_contains: vec![],
                tags: vec![],
            },
            model: "cxr10".to_string(),
            result: crate::conf::RuleResult {
                kind: crate::conf::ResultKind::Pr,
                color: None,
                profile_version: 1,
                series_description: None,
                label: crate::conf::LabelProfile::default(),
                sr: None,
                measurements: None,
            },
            index: 0,
        }
    }

    #[test]
    fn a_non_array_series_search_is_an_error_not_an_empty_list() {
        // The same rule as the instance reader: a search answers an array,
        // and a non-array must fail the workitem with the reason rather than
        // read as "the study has no series".
        let rule = crate::conf::Rule {
            r#match: crate::conf::Match {
                modality: vec![],
                code: vec![],
                study_description_contains: vec![],
                code_meaning_contains: vec![],
                body_part_contains: vec![],
                tags: vec![],
            },
            model: "cxr10".to_string(),
            result: crate::conf::RuleResult {
                kind: crate::conf::ResultKind::Pr,
                color: None,
                profile_version: 1,
                series_description: None,
                label: crate::conf::LabelProfile::default(),
                sr: None,
                measurements: None,
            },
            index: 0,
        };
        let object = crate::json::parse(br#"{"0020000E":{"Value":["s1"]}}"#).expect("parses");
        let error = matching_series(&object, &rule).expect_err("an object is refused");
        assert!(error.contains("non-array"), "the reason: {error}");
    }

    #[test]
    fn an_empty_series_search_is_a_real_answer() {
        let rule = crate::conf::Rule {
            r#match: crate::conf::Match {
                modality: vec![],
                code: vec![],
                study_description_contains: vec![],
                code_meaning_contains: vec![],
                body_part_contains: vec![],
                tags: vec![],
            },
            model: "cxr10".to_string(),
            result: crate::conf::RuleResult {
                kind: crate::conf::ResultKind::Pr,
                color: None,
                profile_version: 1,
                series_description: None,
                label: crate::conf::LabelProfile::default(),
                sr: None,
                measurements: None,
            },
            index: 0,
        };
        let rows = crate::json::parse(b"[]").expect("parses");
        assert_eq!(matching_series(&rows, &rule), Ok(Vec::new()));
    }

    /// A model literal for the UID-derivation tests.
    fn model(id: &str) -> crate::conf::Model {
        crate::conf::Model {
            id: id.to_string(),
            script: Some(std::path::PathBuf::from("proc.py")),
            interpreter: None,
            timeout_secs: 300,
            max_result_bytes: crate::conf::DEFAULT_MAX_RESULT_BYTES,
        }
    }

    #[test]
    fn a_result_uid_is_stable_for_the_same_study_and_version() {
        // Derived, not random: a rerun at the same profile must mint the
        // SAME UID, so the origin sees a duplicate rather than a second
        // result series.
        assert_eq!(
            pr_uid_of_model("1.2.3", &model("m"), 1, 1),
            pr_uid_of_model("1.2.3", &model("m"), 1, 1)
        );
        assert_eq!(
            pr_series_of_model("1.2.3", &model("m"), 1),
            pr_series_of_model("1.2.3", &model("m"), 1)
        );
    }

    #[test]
    fn a_result_uid_changes_with_the_study_and_with_the_version() {
        assert_ne!(
            pr_uid_of_model("1.2.3", &model("m"), 1, 1),
            pr_uid_of_model("1.2.4", &model("m"), 1, 1)
        );
        assert_ne!(
            pr_uid_of_model("1.2.3", &model("m"), 1, 1),
            pr_uid_of_model("1.2.3", &model("m"), 2, 1),
            "a bumped profile version must not collide with the old object"
        );
    }

    #[test]
    fn the_result_uid_lives_in_a_different_root_arc_from_its_series() {
        // The instance UID and the series UID use different seeds, so a
        // presentation state can never be its own series.
        assert_ne!(
            pr_uid_of_model("1.2.3", &model("m"), 1, 1),
            pr_series_of_model("1.2.3", &model("m"), 1)
        );
    }

    #[test]
    fn a_result_uid_is_a_valid_dicom_uid() {
        for uid in [
            pr_uid_of_model("1.2.3", &model("m"), 1, 1),
            pr_series_of_model("1.2.3", &model("m"), 1),
            derived_uid("1.2.3", &model("m"), 1, 0),
            derived_series_uid("1.2.3", &model("m"), 1),
        ] {
            assert!(
                uid.len() <= 64,
                "DICOM UIDs are at most 64 characters: {uid}"
            );
            assert!(
                uid.chars().all(|c| c.is_ascii_digit() || c == '.'),
                "the UID is decimal digits in dot-separated components: {uid}"
            );
            // PS3.5 9.1: no leading zero in a component (except "0" itself).
            for component in uid.split('.') {
                assert!(!component.is_empty(), "empty component in {uid}");
                assert!(
                    component == "0" || !component.starts_with('0'),
                    "leading zero in a component of {uid}"
                );
            }
            assert!(
                uid.starts_with("2.25."),
                "the UID is rooted under the 2.25 arc: {uid}"
            );
        }
    }

    /// A study row carrying a coded sequence, as an origin would return it.
    fn study_with_a_code_sequence() -> crate::json::Json {
        crate::json::parse(
            br#"{
              "0020000D":{"vr":"UI","Value":["1.2.3"]},
              "00081030":{"vr":"LO","Value":["Chest study"]},
              "00080061":{"vr":"CS","Value":["CT"]},
              "00321064":{"vr":"SQ","Value":[{
                 "00080100":{"vr":"SH","Value":["RID5011"]},
                 "00080104":{"vr":"LO","Value":["Thorax"]}
              }]},
              "00081115":{"vr":"SQ","Value":[{
                 "00081140":{"vr":"SQ","Value":[{
                    "00080104":{"vr":"LO","Value":["Abdomen"]}
                 }]}
              }]}
            }"#,
        )
        .expect("the fixture parses")
    }

    #[test]
    fn a_code_meaning_rule_can_see_the_studys_codes() {
        // The defect this pins: `study_attrs` hardcoded empty vectors, so a
        // `code_meaning_contains` rule could never match and the workitem
        // silently cancelled. The meanings are nested at two different
        // depths on purpose - a coded sequence can hang under any tag.
        let meanings = study_code_meanings(&study_with_a_code_sequence());
        assert!(
            meanings.iter().any(|m| m == "Thorax"),
            "a top-level coded sequence yields its meaning: {meanings:?}"
        );
        assert!(
            meanings.iter().any(|m| m == "Abdomen"),
            "a nested (two levels deep) sequence is walked too: {meanings:?}"
        );
    }

    #[test]
    fn a_code_meaning_makes_a_code_meaning_rule_match() {
        // End to end through the matcher, with the attributes this fix
        // produces: a rule that names a meaning the study carries must match
        // instead of falling through to the default (cancel) rule. The rule
        // is taken from the shipped example so its shape is the real one.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("clinfer.conf.example.en");
        let conf = crate::conf::load(&path).expect("parses");
        let mut rule = conf.rules[0].clone();
        rule.r#match.modality = vec!["CT".into()];
        rule.r#match.code_meaning_contains = vec!["thorax".into()];

        let study = study_with_a_code_sequence();
        let attrs = crate::matcher::StudyAttrs {
            modalities: vec!["CT".into()],
            code_meanings: study_code_meanings(&study),
            ..crate::matcher::StudyAttrs::default()
        };
        assert!(
            crate::matcher::matches_rule(&rule, &attrs),
            "the rule names a meaning the study carries"
        );

        // And the same rule does NOT match a meaning the study lacks, so the
        // positive case is not an "everything matches" artefact.
        let mut other = rule.clone();
        other.r#match.code_meaning_contains = vec!["pelvis".into()];
        assert!(!crate::matcher::matches_rule(&other, &attrs));
    }

    #[test]
    fn a_tag_the_study_does_not_carry_binds_to_nothing() {
        // Documented behaviour: the binding does not match, and nothing
        // panics. The alternative (a wildcard) would silently route studies
        // the site did not mean to route. `study_tag_value` is the reader,
        // and the study below carries no (0021,1109).
        let study = study_with_a_code_sequence();
        assert!(
            study_tag_value(&study, "00211109").is_none(),
            "an absent tag has no value"
        );
        assert_eq!(
            study_tag_value(&study, "00081030").as_deref(),
            Some("Chest study"),
            "a present tag yields its value"
        );
    }

    #[test]
    fn a_coded_tag_yields_its_meaning_first() {
        // A site that binds a code sequence means the meaning, not the raw
        // code value, so the meaning wins when the entry carries one.
        let study = study_with_a_code_sequence();
        assert_eq!(
            study_tag_value(&study, "00321064").as_deref(),
            Some("Thorax"),
            "a sequence entry is read through its CodeMeaning"
        );
    }

    /// `study_attrs` END TO END against a one-shot origin.
    ///
    /// The three tests above call the readers directly, which proves the
    /// readers work but not that the production path uses them. This one
    /// goes through the function the worker actually calls, so reverting
    /// `study_attrs` to empty vectors fails it.
    #[test]
    fn study_attrs_carries_the_codes_and_tags_the_rules_need() {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a test origin");
        let addr = listener.local_addr().expect("addr");
        let body = br#"{
          "0020000D":{"vr":"UI","Value":["1.2.3"]},
          "00081030":{"vr":"LO","Value":["Chest study"]},
          "00080061":{"vr":"CS","Value":["CT"]},
          "00321064":{"vr":"SQ","Value":[{
             "00080104":{"vr":"LO","Value":["Thorax"]}
          }]}
        }"#;
        let origin_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut head = [0u8; 4096];
            let _ = stream.read(&mut head);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/dicom+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(body);
        });

        let origin = Dicomweb::new(
            crate::http::Client::new(&format!("http://{addr}/dicomweb"), "clinfer")
                .expect("client"),
        );
        // The config carries one rule that binds (0008,1030), which is what
        // makes `study_tag_bindings` read it.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("clinfer.conf.example.en");
        let mut conf = crate::conf::load(&path).expect("parses");
        conf.rules[0].r#match.tags = vec![crate::conf::TagBinding {
            tag: "00081030".into(),
            patterns: vec!["*chest*".into()],
        }];

        let attrs = study_attrs(&origin, "1.2.3", &conf).expect("the origin answers");
        let _ = origin_thread.join();

        assert_eq!(
            attrs.modalities,
            vec!["CT".to_string()],
            "the study's modality is read"
        );
        assert!(
            attrs.code_meanings.iter().any(|m| m == "Thorax"),
            "the study's coded meaning reaches the matcher: {:?}",
            attrs.code_meanings
        );
        assert_eq!(
            attrs.tags,
            vec![("00081030".to_string(), "Chest study".to_string())],
            "the bound tag's value reaches the matcher"
        );
    }

    /// B1: a study the origin refuses is an error, not an empty study.
    ///
    /// Returning empty attributes on a failed read made a rule that constrains
    /// nothing match, so the workitem was driven forward against a study that
    /// was never retrieved. The caller turns this `Err` into a FAILED workitem,
    /// which is what the operator needs to see.
    #[test]
    fn a_study_the_origin_refuses_is_an_error() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let origin_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut head = [0u8; 4096];
            let _ = stream.read(&mut head);
            // 404: the study does not exist at this origin.
            let body = b"{\"error\":\"not found\"}";
            let response = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(body);
        });

        let origin = Dicomweb::new(
            crate::http::Client::new(&format!("http://{addr}/dicomweb"), "clinfer")
                .expect("client"),
        );
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("clinfer.conf.example.en");
        let conf = crate::conf::load(&path).expect("parses");

        let result = study_attrs(&origin, "9.9.9", &conf);
        let _ = origin_thread.join();

        assert!(
            result.is_err(),
            "a refused study must not read as a study with no attributes"
        );
    }
}
