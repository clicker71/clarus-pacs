// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Worklist subscriptions and the outbound workitem push (PS3.18 11.10/11.11).
//!
//! A subscriber registers a URL; when a workitem is created the server POSTs
//! that workitem to the registered URL. That is the standard's own shape: the
//! origin server notifies the subscriber, and the canonical record stays on
//! this server - the push copy is a wake-up, not the record.
//!
//! # Best-effort by design
//! A push that cannot be delivered is DROPPED after a bounded number of
//! attempts, counted, and forgotten. Durability comes from the subscriber's
//! catch-up search (`GET /dicomweb/workitems`), not from unbounded retries:
//! a subscriber that was down simply finds the workitem on reconnect. Holding
//! the queue open would punish every healthy subscriber for one broken one.
//!
//! # Off the hot path
//! `push_to_subscribers` runs inside STOW/CREATE and does exactly one thing:
//! a non-blocking `try_send` into a bounded channel. The HTTP POST, the
//! retries and the backoff all happen on one background thread, so a slow or
//! dead subscriber can never slow a STOW down.
//!
//! # Volatile registry
//! Subscriptions live in memory only. A restart forgets them; a subscriber
//! re-subscribes. This is a deliberate S1 boundary (no persistence), recorded
//! in CONFORMANCE.

#![forbid(unsafe_code)]

use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Mutex, OnceLock};

/// Bounded queue depth. A full queue drops the newest push (counted) rather
/// than blocking the request thread - the hot path must never wait on a
/// subscriber.
const QUEUE_CAPACITY: usize = 256;

/// Per-request write/read timeout for the push POST. Short and fixed: the
/// background thread has one job and must not park on a host that accepts a
/// connection and then says nothing.
const PUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Backoff between attempts, multiplied by the attempt number. Fixed, not a
/// knob: the operator dial is `push_attempts`, this is just the pacing.
const PUSH_BACKOFF_STEP: std::time::Duration = std::time::Duration::from_secs(2);

/// Outbound identity the server presents to a subscriber.
///
/// A constant: the subscriber is a peer the operator configured, and inventing
/// a per-deployment knob for one header would be a knob nobody turns.
const PUSH_USER: &str = "clarus";

/// One registered subscriber of the worklist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Subscription {
    /// The subscriber's AE Title (the `{subscriber}` URI template variable).
    pub subscriber: String,
    /// Where the workitem is POSTed.
    ///
    /// `None` means the subscription is CHANNEL-ONLY: it has no push URI, so
    /// the POST-push fan-out skips it entirely and its notifications arrive
    /// over its open WebSocket Notification Connection (PS3.18 8.10.4)
    /// instead. PS3.18 11.10 makes the subscription URI optional, and a
    /// subscriber that opens a channel has no need for the origin to be able
    /// to reach it inbound.
    ///
    /// UPS Search remains the durable path for such a subscriber (the AIW-I
    /// Reliability statement): the channel is best-effort, so a subscriber
    /// that misses an Event Report catches up by searching.
    pub endpoint: Option<String>,
    /// Restrict pushes to workitems carrying this Procedure Step Label
    /// (0074,1204). `None` = match every workitem (the unfiltered
    /// subscription, 34.5).
    pub step_label: Option<String>,
    /// Restrict pushes to workitems whose Scheduled Workitem Code Sequence
    /// (0040,4018) carries this Code Value (0008,0100).
    ///
    /// A matching attribute INSIDE a sequence, written `00404018.00080100`
    /// per PS3.18 8.3.4.1 and matched with PS3.4 C.2.2.2.6 Sequence Matching:
    /// the workitem matches when its sequence holds at least one item whose
    /// named attribute equals this value. `None` = unconstrained.
    pub code_value: Option<String>,
    /// The same for the Coding Scheme Designator (0008,0102),
    /// `00404018.00080102`.
    pub code_scheme: Option<String>,
    /// When the Deletion Lock was taken, in seconds since the Unix epoch.
    ///
    /// `Some(_)` means the subscription carries a Deletion Lock: PS3.18 11.10
    /// defines the `deletionlock` query parameter, and "if present with a
    /// value of true the Subscription will be created with a Deletion Lock"
    /// (see PS3.4 CC.2.3.1). AIW-I 4.86.4.1.2 makes supporting them a
    /// requirement on the Manager.
    ///
    /// The INSTANT is kept, not just the flag, because the retention sweep
    /// compares it against the configured lock lifetime (AIW-I 4.85.4.1.3:
    /// "it shall be capable of waiting at least 24 hours before such
    /// deletions"). Unsubscribing clears the lock, which is PS3.4 CC.2.3.1's
    /// own state machine: "AE Unsubscribes from Specific UPS: Go to Not
    /// Subscribed" (Table CC.2.3-2).
    pub deletion_lock_at: Option<u64>,
    /// Restrict this subscription to ONE workitem (PS3.18 11.10.1.1's
    /// Workitem Subscription template).
    ///
    /// `None` is the Worklist and Filtered Worklist forms, which are
    /// subscription-SOP-Class-scoped and unchanged. `Some(uid)` means the
    /// subscriber registered under `/workitems/{workitem}/subscribers/...`
    /// and must receive events for that workitem only - a workitem
    /// subscription that received its neighbours' events would be
    /// indistinguishable from a worklist subscription with no filter.
    pub workitem_uid: Option<String>,
    /// (0020,000D) Study Instance UID the workitem's input study must carry.
    ///
    /// One field for BOTH the top-level spelling and
    /// `ReferencedRequestSequence.StudyInstanceUID`, because both name the same
    /// fact (PS3.4 Table CC.2.5-3 types Study Instance UID O in both places).
    /// Two fields would let one subscription mean two different things.
    pub study_uid: Option<String>,
    /// (0010,0020) Patient ID - a REQUIRED matching key (CC.2.5-3, R).
    pub patient_id: Option<String>,
    /// (0074,1000) Procedure Step State.
    pub workflow_state: Option<String>,
    /// (0040,4041) Input Readiness State.
    pub input_readiness: Option<String>,
    /// (0074,1200) Scheduled Procedure Step Priority.
    pub sps_priority: Option<String>,
    /// (0040,4005) Scheduled Procedure Step Start DateTime.
    pub sps_start_datetime: Option<String>,
    /// (0010,0010) Patient's Name.
    pub patient_name: Option<String>,
    /// (0010,0021) Issuer of Patient ID.
    pub patient_id_issuer: Option<String>,
    /// (0008,0050) Accession Number.
    pub accession_number: Option<String>,
    /// (0040,1001) Requested Procedure ID.
    pub requested_procedure_id: Option<String>,
    /// (0040,4025) > (0008,0100) station code value, matched against the
    /// stored Scheduled Station AE Title (the vendor aliasing the search uses).
    pub station_aet: Option<String>,
}

impl Subscription {
    /// A subscription with only a subscriber name and an endpoint.
    ///
    /// No filter keys, no lock - the unfiltered form (PS3.18 11.10). A
    /// constructor exists so that adding a filter key does not ripple through
    /// every literal in the tests: a test that is about, say, replacement
    /// semantics says nothing about filters and must not have to name them.
    #[must_use]
    pub fn unfiltered(subscriber: &str, endpoint: Option<&str>) -> Self {
        Self {
            subscriber: subscriber.to_string(),
            endpoint: endpoint.map(str::to_string),
            step_label: None,
            code_value: None,
            code_scheme: None,
            deletion_lock_at: None,
            workitem_uid: None,
            study_uid: None,
            patient_id: None,
            workflow_state: None,
            input_readiness: None,
            sps_priority: None,
            sps_start_datetime: None,
            patient_name: None,
            patient_id_issuer: None,
            accession_number: None,
            requested_procedure_id: None,
            station_aet: None,
        }
    }
}

/// The workitem values a subscription filter is matched against.
///
/// Borrowed `&str` throughout so the match path allocates nothing: the
/// subscription's keys are compared against these slices directly, with no
/// `String` built per event (the ap101 rule for `subscription_accepts`).
///
/// It is one struct rather than a positional argument per key because the key
/// set is now fourteen wide: eight positional `&str`s would be a call site
/// nobody can read, and every new key would change the signature of every
/// caller. `None` means "the caller does not have this value", which is a
/// non-match for a subscription that constrains on it - never a guess.
///
/// The values come from the record the caller just wrote (the fan-out sites
/// hold it), so the match needs no second store read.
#[derive(Debug, Clone, Copy, Default)]
pub struct FilterValues<'a> {
    /// (0074,1204) Procedure Step Label, `""` when absent.
    pub step_label: &'a str,
    /// (0040,4018) Code Value and Coding Scheme Designator.
    pub code: Option<(&'a str, &'a str)>,
    /// (0020,000D) Study Instance UID of the input study.
    pub study_uid: Option<&'a str>,
    /// (0010,0020) Patient ID.
    pub patient_id: Option<&'a str>,
    /// (0074,1000) Procedure Step State.
    pub workflow_state: Option<&'a str>,
    /// (0040,4041) Input Readiness State.
    pub input_readiness: Option<&'a str>,
    /// (0074,1200) Scheduled Procedure Step Priority.
    pub sps_priority: Option<&'a str>,
    /// (0040,4005) Scheduled Procedure Step Start DateTime.
    pub sps_start_datetime: Option<&'a str>,
    /// (0010,0010) Patient's Name.
    pub patient_name: Option<&'a str>,
    /// (0010,0021) Issuer of Patient ID.
    pub patient_id_issuer: Option<&'a str>,
    /// (0008,0050) Accession Number.
    pub accession_number: Option<&'a str>,
    /// (0040,1001) Requested Procedure ID.
    pub requested_procedure_id: Option<&'a str>,
    /// (0040,0001) Scheduled Station AE Title - the value a Scheduled Station
    /// Name Code Sequence Code Value matches on this deployment.
    pub station_aet: Option<&'a str>,
}

impl FilterValues<'_> {
    /// The value a subscription key reads, as a slice, `None` when absent.
    ///
    /// `step_label` is a bare `&str` (empty when the workitem has none), so it
    /// is mapped to `None` here: an EMPTY label is "the workitem carries no
    /// label", and a subscription filtering on a non-empty label must not match
    /// it by comparing against `""`.
    fn get(&self, key: FilterKeyRef) -> Option<&str> {
        match key {
            FilterKeyRef::StepLabel => (!self.step_label.is_empty()).then_some(self.step_label),
            FilterKeyRef::CodeValue => self.code.map(|(value, _)| value),
            FilterKeyRef::CodeScheme => self.code.map(|(_, scheme)| scheme),
            FilterKeyRef::StudyUid => self.study_uid,
            FilterKeyRef::PatientId => self.patient_id,
            FilterKeyRef::WorkflowState => self.workflow_state,
            FilterKeyRef::InputReadiness => self.input_readiness,
            FilterKeyRef::SpsPriority => self.sps_priority,
            FilterKeyRef::SpsStartDatetime => self.sps_start_datetime,
            FilterKeyRef::PatientName => self.patient_name,
            FilterKeyRef::PatientIdIssuer => self.patient_id_issuer,
            FilterKeyRef::AccessionNumber => self.accession_number,
            FilterKeyRef::RequestedProcedureId => self.requested_procedure_id,
            FilterKeyRef::StationAet => self.station_aet,
        }
    }
}

impl<'a> FilterValues<'a> {
    /// Borrow every filter value off a stored record.
    ///
    /// The fan-out sites hold the record they just wrote, so this is the
    /// normal construction: one line per call site, and a new filter key
    /// reaches every fan-out without touching any of them.
    #[must_use]
    pub fn from_record(record: &'a clarus_core::ups::WorkItemRecord) -> Self {
        Self {
            step_label: record.step_label.as_deref().unwrap_or(""),
            code: record
                .scheduled_workitem_code
                .as_ref()
                .map(|code| (code.value.as_str(), code.scheme.as_str())),
            study_uid: record.input_study_uid.as_deref(),
            patient_id: record.patient_id.as_deref(),
            workflow_state: Some(record.workflow_state.as_str()),
            input_readiness: record.input_readiness.as_deref(),
            sps_priority: record.sps_priority.as_deref(),
            sps_start_datetime: record.sps_start_datetime.as_deref(),
            patient_name: record.patient_name.as_deref(),
            patient_id_issuer: record.patient_id_issuer.as_deref(),
            accession_number: record.accession_number.as_deref(),
            requested_procedure_id: record.requested_procedure_id.as_deref(),
            station_aet: record.station_aet.as_deref(),
        }
    }

    /// Borrow the values off an event the fan-out is carrying.
    ///
    /// The event already holds every routing value as an owned `String`, so
    /// this is a borrow, not a copy: the match path stays allocation-free even
    /// though the event's fields are owned.
    #[must_use]
    pub fn from_event(event: &'a crate::handlers::ups::event_bus::UpsEvent) -> Self {
        let routing = event.filter.as_ref();
        Self {
            step_label: routing.step_label.as_str(),
            code: routing
                .scheduled_code
                .as_ref()
                .map(|(value, scheme)| (value.as_str(), scheme.as_str())),
            study_uid: routing.study_uid.as_deref(),
            patient_id: routing.patient_id.as_deref(),
            workflow_state: routing.workflow_state.as_deref(),
            input_readiness: routing.input_readiness.as_deref(),
            sps_priority: routing.sps_priority.as_deref(),
            sps_start_datetime: routing.sps_start_datetime.as_deref(),
            patient_name: routing.patient_name.as_deref(),
            patient_id_issuer: routing.patient_id_issuer.as_deref(),
            accession_number: routing.accession_number.as_deref(),
            requested_procedure_id: routing.requested_procedure_id.as_deref(),
            station_aet: routing.station_aet.as_deref(),
        }
    }
}

/// One filterable attribute, so `subscription_accepts` is a loop over a fixed
/// list rather than fourteen near-identical `if let` blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterKeyRef {
    StepLabel,
    CodeValue,
    CodeScheme,
    StudyUid,
    PatientId,
    WorkflowState,
    InputReadiness,
    SpsPriority,
    SpsStartDatetime,
    PatientName,
    PatientIdIssuer,
    AccessionNumber,
    RequestedProcedureId,
    StationAet,
}

/// A reader for one key's slot on a `Subscription`.
type FilterSlot = fn(&Subscription) -> &Option<String>;

/// Every key a subscription can carry, paired with its slot on both sides.
///
/// The single place the key set is enumerated for matching: adding a key means
/// one row here plus its `Subscription` field. Written as a `const` table so
/// the matcher loops with no allocation and no per-key function call.
const FILTER_MATCH_ORDER: &[(FilterKeyRef, FilterSlot)] = &[
    (FilterKeyRef::StepLabel, |s| &s.step_label),
    (FilterKeyRef::CodeValue, |s| &s.code_value),
    (FilterKeyRef::CodeScheme, |s| &s.code_scheme),
    (FilterKeyRef::StudyUid, |s| &s.study_uid),
    (FilterKeyRef::PatientId, |s| &s.patient_id),
    (FilterKeyRef::WorkflowState, |s| &s.workflow_state),
    (FilterKeyRef::InputReadiness, |s| &s.input_readiness),
    (FilterKeyRef::SpsPriority, |s| &s.sps_priority),
    (FilterKeyRef::SpsStartDatetime, |s| &s.sps_start_datetime),
    (FilterKeyRef::PatientName, |s| &s.patient_name),
    (FilterKeyRef::PatientIdIssuer, |s| &s.patient_id_issuer),
    (FilterKeyRef::AccessionNumber, |s| &s.accession_number),
    (FilterKeyRef::RequestedProcedureId, |s| {
        &s.requested_procedure_id
    }),
    (FilterKeyRef::StationAet, |s| &s.station_aet),
];

/// The registry of active subscriptions.
///
/// A plain `Mutex<Vec<_>>`: the read path is one short lock per created
/// workitem and the write path is one lock per subscribe call. Subscriptions
/// are operator-scale (a handful), so a map keyed by name would add machinery
/// without buying anything.
#[derive(Default)]
pub struct SubscriptionRegistry {
    subs: Mutex<Vec<Subscription>>,
}

impl SubscriptionRegistry {
    /// Register (or replace) a subscriber by AE Title AND scope.
    ///
    /// The replacement key is `(subscriber, workitem_uid)`, not the AE Title
    /// alone. One AE legitimately holds several subscriptions at once: PS3.18
    /// 11.10 offers a Worklist (and Filtered Worklist) subscription as well as
    /// a per-workitem one, and PS3.4 CC.2.3 tracks subscription state PER AE
    /// AND PER UPS. Keying on the name alone meant the second subscription
    /// silently replaced the first: a workitem-scoped one destroyed the
    /// subscriber's Worklist subscription (and its Deletion Lock), and a
    /// subscriber with two workitem subscriptions lost one of them.
    ///
    /// Replacing the SAME scope is still what makes re-subscription after a
    /// restart idempotent: a subscriber that comes back gets one entry per
    /// scope, not a second copy that would double every push.
    pub fn subscribe(&self, sub: Subscription) {
        let mut subs = self.subs.lock().unwrap_or_else(|p| p.into_inner());
        // AE Titles are compared case-insensitively here for the same reason
        // `accepts_for_with_values` does: the fan-out asks whether a
        // subscription matches by name, and a subscriber that spelled its own
        // title differently on re-subscribe would otherwise own two entries -
        // the second one shadowing nothing, so every event is pushed twice.
        let slot = subs.iter_mut().find(|s| {
            s.subscriber.eq_ignore_ascii_case(&sub.subscriber) && s.workitem_uid == sub.workitem_uid
        });
        if let Some(slot) = slot {
            *slot = sub;
            return;
        }
        subs.push(sub);
    }

    /// Remove every subscription of `subscriber`. Returns `true` when one was
    /// registered.
    ///
    /// This is the whole-AE form: PS3.18 11.10 addresses the subscriber, not
    /// one of its scopes, so a DELETE on that URI drops all of them.
    pub fn unsubscribe(&self, subscriber: &str) -> bool {
        self.unsubscribe_scoped(subscriber, None)
    }

    /// Remove one scope of `subscriber`, or every scope when `workitem_uid` is
    /// `None`.
    ///
    /// Scoped removal exists so unsubscribing from one workitem leaves the
    /// subscriber's Worklist subscription (and any other workitem's) alone.
    pub fn unsubscribe_scoped(&self, subscriber: &str, workitem_uid: Option<&str>) -> bool {
        let mut subs = self.subs.lock().unwrap_or_else(|p| p.into_inner());
        let before = subs.len();
        // Same case-insensitive identity as `subscribe`: a DELETE that does
        // not drop the entry `subscribe` would have replaced leaves a
        // subscription the requester believes it removed.
        subs.retain(|s| {
            !(s.subscriber.eq_ignore_ascii_case(subscriber)
                && workitem_uid.is_none_or(|w| s.workitem_uid.as_deref() == Some(w)))
        });
        subs.len() != before
    }

    /// Every subscription whose filter accepts the workitem.
    ///
    /// `step_label` and `code` are the workitem's own (0074,1204) and its
    /// (0040,4018) Code Value / Coding Scheme Designator - the fan-out sites
    /// pass what they just wrote, so the match needs no second store read.
    ///
    /// A subscription matches when EVERY filter key it carries holds: an
    /// absent key constrains nothing (the unfiltered subscription matches
    /// everything), and several keys together narrow the match, which is what
    /// PS3.18 8.3.4.1's AND-of-attributes semantics says. Cloned out under the
    /// lock so the network work happens with no lock held.
    fn matching(&self, workitem_uid: &str, values: &FilterValues<'_>) -> Vec<Subscription> {
        let subs = self.subs.lock().unwrap_or_else(|p| p.into_inner());
        subs.iter()
            .filter(|s| subscription_accepts(s, workitem_uid, values))
            .cloned()
            .collect()
    }

    /// Whether `subscriber` holds a subscription accepting the workitem.
    ///
    /// The WebSocket Notification Connection asks this to decide whether a
    /// given `{requester}` should receive an event: PS3.18 8.10.4 opens the
    /// channel, but PS3.18 11.10 subscriptions decide WHICH events that
    /// requester wants. The same filter governs both transports, so a
    /// subscriber cannot receive over the WebSocket something its
    /// subscription would not have pushed to it.
    #[must_use]
    pub fn accepts_for(
        &self,
        subscriber: &str,
        workitem_uid: &str,
        values: &FilterValues<'_>,
    ) -> bool {
        self.accepts_for_with_values(subscriber, workitem_uid, values)
    }

    /// The same question, with the workitem's code available.
    ///
    /// `code` is `(Code Value, Coding Scheme Designator)`; `None` means the
    /// caller does not have it, in which case a code-filtered subscription
    /// does NOT match - the honest answer, because assuming a match would
    /// deliver an event the subscriber asked not to receive.
    #[must_use]
    pub fn accepts_for_with_values(
        &self,
        subscriber: &str,
        workitem_uid: &str,
        values: &FilterValues<'_>,
    ) -> bool {
        let subs = self.subs.lock().unwrap_or_else(|p| p.into_inner());
        subs.iter().any(|s| {
            s.subscriber.eq_ignore_ascii_case(subscriber)
                && subscription_accepts(s, workitem_uid, values)
        })
    }

    /// Number of registered subscribers (test/observability).
    pub fn len(&self) -> usize {
        self.subs.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// The newest Deletion Lock time among all subscriptions, if any lock is
    /// held.
    ///
    /// The retention sweep asks whether ANY subscription still holds a lock
    /// it must honour (AIW-I 4.86.4.1.2: the Manager "shall support the use
    /// of Deletion Locks"), so this returns the most recent lock instant -
    /// the one that keeps the workitem for the longest. Unsubscribing
    /// removes the subscription (PS3.4 CC.2.3.1 Table CC.2.3-2), so a
    /// released lock simply stops appearing here.
    #[must_use]
    pub fn newest_deletion_lock(&self) -> Option<u64> {
        self.subs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter_map(|s| s.deletion_lock_at)
            .max()
    }

    /// Whether no subscriber is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every subscription currently registered, cloned out.
    ///
    /// For tests that must run against a known registry: the registry is
    /// process-wide, so a test that seeds it has to be able to put it back.
    #[must_use]
    pub fn snapshot(&self) -> Vec<Subscription> {
        self.subs.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Drop every subscription. Test support only - the wire never empties
    /// the registry except by unsubscribing one name at a time.
    pub fn clear(&self) {
        self.subs.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }
}

/// Process-wide registry. One per server process, like the log writer.
fn registry() -> &'static SubscriptionRegistry {
    static REGISTRY: OnceLock<SubscriptionRegistry> = OnceLock::new();
    REGISTRY.get_or_init(SubscriptionRegistry::default)
}

/// The registry accessor used by the HTTP handlers.
pub fn subscriptions() -> &'static SubscriptionRegistry {
    registry()
}

/// One unit of work on the push queue: a workitem and the subscriber to
/// notify. Kept separate from [`Subscription`] so a job cannot be mistaken for
/// a registration.
struct PushJob {
    subscriber: String,
    endpoint: String,
    workitem_uid: String,
    /// The attributes the Create body must state, captured at fan-out time.
    snapshot: WorkitemSnapshot,
}

/// The bounded push queue sender, created on first use.
fn queue() -> &'static SyncSender<PushJob> {
    static QUEUE: OnceLock<SyncSender<PushJob>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE_CAPACITY);
        start_worker(rx);
        tx
    })
}

/// Hand a workitem to every matching subscriber.
///
/// Called from the workitem-creation paths. It NEVER blocks and never fails
/// the caller: a full queue or a subscriber-free server is a drop, counted.
///
/// `step_label` is the workitem's Procedure Step Label, which is what a
/// filtered subscriber matches on.
///
/// # Channel-only subscribers
///
/// A subscription without an endpoint is skipped here. That is a normal
/// configuration, not an error: the subscriber receives its Event Reports
/// over its WebSocket channel, which the notification path publishes
/// independently of this fan-out. Nothing is counted for it - no attempt, no
/// drop, no retry - because there was never a push to make.
pub fn push_to_subscribers(workitem_uid: &str, values: &FilterValues<'_>) {
    push_to_subscribers_with_values(workitem_uid, values);
}

/// The same fan-out, with the workitem's other filter values available.
///
/// Both fan-out sites pass what they just wrote (the HTTP create
/// `ups_pure::ups_create` and the readiness promotion `readiness::promote`),
/// so a filtered subscription is answered from the record in hand rather than
/// from a second store read.
pub fn push_to_subscribers_with_values(workitem_uid: &str, values: &FilterValues<'_>) {
    let targets = registry().matching(workitem_uid, values);
    if targets.is_empty() {
        return;
    }
    // Snapshot the attributes the pushed workitem must carry BEFORE the queue,
    // because the job outlives this borrow: a subscriber's copy has to state
    // the same readiness, label, priority and start time the canonical record
    // holds, and those are exactly the fields `values` already carries.
    let snapshot = WorkitemSnapshot::from_values(values);
    for sub in targets {
        if sub.endpoint.is_none() {
            continue;
        }
        enqueue(workitem_uid, sub, snapshot.clone());
    }
}

/// The workitem attributes a pushed Create body states.
///
/// Constants were the defect: a workitem held INCOMPLETE (the readiness
/// debounce has not expired) was pushed as READY, so a subscriber woken by the
/// push could claim and complete a study whose instances were still arriving -
/// the very race the debounce exists to prevent. The same reasoning applies to
/// the label, priority and start time: a push that states a different workitem
/// than the one the archive holds is not an alias of it.
#[derive(Clone, Debug, Default)]
struct WorkitemSnapshot {
    input_readiness: String,
    step_label: String,
    priority: String,
    start_datetime: String,
}

impl WorkitemSnapshot {
    fn from_values(values: &FilterValues<'_>) -> Self {
        Self {
            // AIW-I 4.84.4.1.2.1 requires the readiness the workitem actually
            // has; a missing value is not a licence to invent READY.
            input_readiness: values
                .input_readiness
                .filter(|v| !v.is_empty())
                .unwrap_or(READINESS_UNKNOWN)
                .to_string(),
            step_label: values.step_label.to_string(),
            priority: values.sps_priority.unwrap_or_default().to_string(),
            start_datetime: values.sps_start_datetime.unwrap_or_default().to_string(),
        }
    }
}

/// Readiness reported when the caller could not supply one.
///
/// The UPS state set has no "unknown" readiness, so this is the honest choice
/// only in the sense that it is NOT READY: a consumer that gates on READY
/// keeps waiting instead of proceeding on a guess.
const READINESS_UNKNOWN: &str = "INCOMPLETE";

/// Whether one subscription's filter accepts a workitem.
///
/// Every key the subscription carries must hold; a key it does not carry
/// constrains nothing. This is one function because the POST fan-out and the
/// WebSocket channel MUST agree - a subscriber that receives over the channel
/// what its subscription would not have pushed would be a second, quieter
/// filter, and two filters that can drift apart are worse than one.
///
/// A key the caller cannot supply (`None` in `values`) is a NON-match, not a
/// guess: PS3.18 8.3.4.1's AND-of-attributes semantics means the subscription
/// asked for workitems carrying that value, and one whose value is unknown
/// does not demonstrably carry it. Answering `true` there would deliver an
/// event the subscriber asked not to receive.
fn subscription_accepts(
    subscription: &Subscription,
    workitem_uid: &str,
    values: &FilterValues<'_>,
) -> bool {
    // A workitem-scoped subscription (PS3.18 11.10.1.1's Workitem
    // Subscription template) sees exactly one workitem. Checked first because
    // it is the narrowest bound and the cheapest to reject on.
    if let Some(want) = &subscription.workitem_uid
        && want != workitem_uid
    {
        return false;
    }
    // The remaining keys are one rule applied fourteen times: a key the
    // subscription carries must equal the workitem's value, and a key it does
    // not carry constrains nothing. Looping over the fixed table keeps the
    // matcher from growing a near-identical `if let` block per key, where a
    // copy-paste slip would compare the wrong field and be invisible.
    //
    // Code Value and Coding Scheme Designator are two attributes of ONE
    // sequence item (PS3.4 C.2.2.2.6 Sequence Matching), so they are decided
    // against the SAME item: `FilterValues::code` is one pair, and an item that
    // matched one but not the other is not a match.
    for (key, slot) in FILTER_MATCH_ORDER {
        let Some(want) = slot(subscription) else {
            continue;
        };
        if values.get(*key) != Some(want.as_str()) {
            return false;
        }
    }
    true
}

/// Enqueue one push, counting a queue overflow as a drop.
fn enqueue(workitem_uid: &str, sub: Subscription, snapshot: WorkitemSnapshot) {
    // The caller filtered out channel-only subscribers; the endpoint is read
    // here rather than early-returned so the skip stays in ONE place, where
    // the log line explains it.
    let Some(endpoint) = sub.endpoint else {
        return;
    };
    let job = PushJob {
        subscriber: sub.subscriber,
        endpoint,
        workitem_uid: workitem_uid.to_string(),
        snapshot,
    };
    match queue().try_send(job) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            crate::metrics::UPS_PUSH_DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::clog_warn!(
                workitem_uid = %workitem_uid,
                "UPS push queue full - notification dropped (subscriber catch-up will find it)"
            );
        }
        // The worker died; the notification is lost but the workitem is not.
        Err(TrySendError::Disconnected(_)) => {
            crate::metrics::UPS_PUSH_DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::clog_warn!(workitem_uid = %workitem_uid, "UPS push worker is gone");
        }
    }
}

/// The single background worker: drains the queue, pushing each workitem.
fn start_worker(rx: Receiver<PushJob>) {
    std::thread::Builder::new()
        .name("ups-push".into())
        .spawn(move || {
            while let Ok(job) = rx.recv() {
                deliver(
                    &job.subscriber,
                    &job.endpoint,
                    &job.workitem_uid,
                    &job.snapshot,
                );
            }
        })
        .expect("spawn ups-push worker");
}

/// Deliver one workitem to one subscriber, retrying per `[ups] push_attempts`.
///
/// The attempt count comes from the config snapshot captured at startup (the
/// worker is a free function and has no `AppState`); `push_attempts()` reads
/// it from the same process-wide place the config loader stored it.
fn deliver(subscriber: &str, endpoint: &str, workitem_uid: &str, snapshot: &WorkitemSnapshot) {
    let attempts = push_attempts();
    for attempt in 1..=attempts {
        match post_workitem(endpoint, workitem_uid, snapshot) {
            Ok(status) if (200..300).contains(&status) => {
                crate::metrics::UPS_PUSHED.fetch_add(1, Ordering::Relaxed);
                crate::clog_debug!(
                    workitem_uid = %workitem_uid,
                    subscriber = %subscriber,
                    status = status,
                    attempt = attempt,
                    "UPS push delivered"
                );
                return;
            }
            Ok(status) => {
                // 4xx means the subscriber refused this workitem; retrying the
                // same body cannot change that, but the attempt budget is the
                // contract, so we keep the uniform loop and count the retry.
                crate::clog_debug!(
                    workitem_uid = %workitem_uid,
                    subscriber = %subscriber,
                    status = status,
                    attempt = attempt,
                    "UPS push rejected"
                );
            }
            Err(e) => {
                crate::clog_debug!(
                    workitem_uid = %workitem_uid,
                    subscriber = %subscriber,
                    error = %e,
                    attempt = attempt,
                    "UPS push attempt failed"
                );
            }
        }
        if attempt < attempts {
            crate::metrics::UPS_PUSH_RETRIES.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(PUSH_BACKOFF_STEP * attempt);
        }
    }
    crate::metrics::UPS_PUSH_DROPPED.fetch_add(1, Ordering::Relaxed);
    crate::clog_warn!(
        workitem_uid = %workitem_uid,
        subscriber = %subscriber,
        attempts = attempts,
        "UPS push dropped (subscriber catch-up will find it)"
    );
}

/// POST one workitem to a subscriber, returning the status code.
///
/// A hand-rolled HTTP/1.1 request on `TcpStream`: the server already owns an
/// HTTP stack, and pulling in a client crate for one POST would add a
/// dependency tree for a request this simple. `Connection: close` keeps it to
/// one request per connection, so no pool or keep-alive state is needed.
fn post_workitem(
    endpoint: &str,
    workitem_uid: &str,
    snapshot: &WorkitemSnapshot,
) -> Result<u16, String> {
    let (host, port, _) = parse_endpoint(endpoint)?;
    let path = workitem_path(endpoint, workitem_uid)?;
    let body = create_body(workitem_uid, snapshot);

    let addr = format!("{host}:{port}");
    // `connect_timeout`, not `connect`: the write and read timeouts below bound
    // the conversation but say nothing about the handshake, which an
    // unreachable host leaves to the OS (a minute or more on a blackholed
    // route). The push queue has ONE worker thread, so a single such endpoint
    // stalled every other subscriber behind it for that whole window.
    let target = std::net::ToSocketAddrs::to_socket_addrs(&addr.as_str())
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| format!("cannot resolve {addr}"))?;
    let mut stream =
        std::net::TcpStream::connect_timeout(&target, PUSH_TIMEOUT).map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(PUSH_TIMEOUT))
        .map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(PUSH_TIMEOUT))
        .map_err(|e| e.to_string())?;

    let head = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Content-Type: application/dicom+json\r\n\
         Content-Length: {}\r\n\
         X-Authenticated-User: {PUSH_USER}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    use std::io::{Read, Write};
    stream
        .write_all(head.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.write_all(&body).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;

    let mut buf = Vec::with_capacity(256);
    // Only the status line matters; the body is the subscriber's business.
    let mut chunk = [0u8; 256];
    // `set_read_timeout` bounds ONE `read`, so a subscriber that dribbles a
    // byte just inside the per-read window held the single push worker for as
    // long as it liked - every other subscriber queued behind it. The overall
    // deadline is what the caller actually promised.
    let deadline = std::time::Instant::now() + PUSH_TIMEOUT;
    while buf.len() < 64 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err("response timeout".to_string());
        }
        stream
            .set_read_timeout(Some(left))
            .map_err(|e| e.to_string())?;
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(e.to_string()),
        }
    }
    parse_status(&buf).ok_or_else(|| "malformed response".to_string())
}

/// Split `http://host:port/path` into `(host, port, path)`.
///
/// Deliberately minimal: the subscriber endpoint is operator-configured, so
/// only the shape an operator writes needs to work. Anything else is an error
/// the subscribe call can report, not a silent misroute.
fn parse_endpoint(endpoint: &str) -> Result<(String, u16, String), String> {
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| format!("subscriber endpoint must start with http://: {endpoint}"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| format!("bad port: {p}"))?),
        None => (authority, 80u16),
    };
    if host.is_empty() {
        return Err(format!("empty host in endpoint: {endpoint}"));
    }
    Ok((host.to_string(), port, path.to_string()))
}

/// Parse the status code out of an HTTP/1.1 status line.
fn parse_status(resp: &[u8]) -> Option<u16> {
    let text = String::from_utf8_lossy(resp);
    let line = text.lines().next()?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// The DICOMweb JSON body: an UPS Create Workitem for `workitem_uid`.
///
/// The shape is what `POST /dicomweb/workitems/{uid}` accepts here and what
/// PS3.4 Table CC.2.5-3 requires at N-CREATE: identity in (0008,0018) SOP
/// Instance UID and (0008,0016) SOP Class UID, the state SCHEDULED, and the
/// type-1 set (Procedure Step Label, Priority, Start DateTime, Input Readiness
/// State). The subscriber receives the SAME UID the canonical store holds, so
/// its copy is an alias, not a second workitem.
///
/// The type-1 values come from the STORED RECORD, never from literals. A push
/// that stated READY for a workitem held INCOMPLETE handed the subscriber a
/// workitem the archive would refuse it: the subscriber could claim and finish
/// a study whose instances were still arriving, which is exactly the race the
/// readiness debounce exists to close. An attribute the record does not carry
/// is OMITTED rather than defaulted - a fabricated start time or priority is a
/// statement the requester never made.
fn create_body(workitem_uid: &str, snapshot: &WorkitemSnapshot) -> Vec<u8> {
    let mut body = Vec::with_capacity(420);
    body.extend_from_slice(
        b"{\"00080016\":{\"vr\":\"UI\",\"Value\":[\"1.2.840.10008.5.1.4.34.6.1\"]},",
    );
    body.extend_from_slice(b"\"00080018\":{\"vr\":\"UI\",\"Value\":[");
    crate::handlers::json_common::push_json_str_loose(&mut body, workitem_uid);
    body.extend_from_slice(b"]}");
    push_create_scalar(
        &mut body,
        "00404041",
        "CS",
        Some(snapshot.input_readiness.as_str()),
    );
    push_create_scalar(
        &mut body,
        "00741204",
        "SH",
        Some(snapshot.step_label.as_str()),
    );
    push_create_scalar(
        &mut body,
        "00741200",
        "CS",
        Some(snapshot.priority.as_str()),
    );
    push_create_scalar(
        &mut body,
        "00404005",
        "DT",
        Some(snapshot.start_datetime.as_str()),
    );
    body.extend_from_slice(b",\"00741000\":{\"vr\":\"CS\",\"Value\":[\"SCHEDULED\"]}}");
    body
}

/// Append `,"<tag>":{"vr":"<vr>","Value":["<value>"]}` for a non-empty value.
///
/// An absent or empty value appends NOTHING: the caller's attribute set is the
/// statement being relayed, and inventing a value here would attribute it to
/// the requester. The leading comma lives here so the first attribute stays
/// syntactically valid no matter which earlier ones were omitted.
fn push_create_scalar(body: &mut Vec<u8>, tag: &str, vr: &str, value: Option<&str>) {
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        return;
    };
    body.extend_from_slice(b",\"");
    body.extend_from_slice(tag.as_bytes());
    body.extend_from_slice(b"\":{\"vr\":\"");
    body.extend_from_slice(vr.as_bytes());
    body.extend_from_slice(b"\",\"Value\":[");
    crate::handlers::json_common::push_json_str_loose(body, value);
    body.extend_from_slice(b"]}");
}

/// The push target path for a subscriber endpoint and a workitem UID.
///
/// Exposed so the caller and the tests agree on where the workitem lands.
pub fn workitem_path(endpoint: &str, workitem_uid: &str) -> Result<String, String> {
    let (_, _, base) = parse_endpoint(endpoint)?;
    Ok(format!("{}/{}", base.trim_end_matches('/'), workitem_uid))
}

/// Attempts per push, snapshotted from `[ups] push_attempts` at startup.
///
/// The worker thread is a free function with no `AppState`; storing the
/// validated value once here keeps the config single-sourced (it is parsed and
/// range-checked in exactly one place) without threading state through the
/// queue job.
static PUSH_ATTEMPTS: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(crate::config::UpsConfig::DEFAULT_PUSH_ATTEMPTS);

/// Publish the configured attempt budget to the worker.
pub fn set_push_attempts(attempts: u32) {
    PUSH_ATTEMPTS.store(attempts, Ordering::Relaxed);
}

/// The attempt budget the worker will use.
fn push_attempts() -> u32 {
    PUSH_ATTEMPTS.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The match input for a test that cares about the label and the code.
    ///
    /// Every other key is absent, which constrains nothing for a subscription
    /// that does not carry it - so these fixtures answer exactly what the old
    /// `matching(uid, label, code)` signature did.
    fn fv<'a>(text: &'a str, code: Option<(&'a str, &'a str)>) -> FilterValues<'a> {
        FilterValues {
            step_label: text,
            code,
            ..Default::default()
        }
    }

    #[test]
    fn endpoint_parses_host_port_and_path() {
        assert_eq!(
            parse_endpoint("http://127.0.0.1:8124/ups").unwrap(),
            ("127.0.0.1".to_string(), 8124, "/ups".to_string())
        );
        // Default port and default path.
        assert_eq!(
            parse_endpoint("http://host").unwrap(),
            ("host".to_string(), 80, "/".to_string())
        );
        // https is refused rather than silently treated as http.
        assert!(parse_endpoint("https://host:443/x").is_err());
        assert!(parse_endpoint("host:80").is_err());
        assert!(parse_endpoint("http://host:notaport/x").is_err());
        assert!(parse_endpoint("http://:80/x").is_err());
    }

    #[test]
    fn status_line_parses() {
        assert_eq!(parse_status(b"HTTP/1.1 200 OK\r\n\r\n"), Some(200));
        assert_eq!(parse_status(b"HTTP/1.1 404 Not Found\r\n"), Some(404));
        assert_eq!(parse_status(b"garbage"), None);
        assert_eq!(parse_status(b""), None);
    }

    /// m21: the pushed Create body states the RECORD's readiness, not READY.
    ///
    /// A workitem held INCOMPLETE is pushed so the subscriber knows it exists,
    /// but the body must not claim it is ready to be worked: a subscriber that
    /// trusts the push would claim and complete a study whose instances are
    /// still arriving, which is the exact race the readiness debounce closes.
    #[test]
    fn create_body_states_the_records_readiness_not_ready() {
        let snapshot = WorkitemSnapshot {
            input_readiness: "INCOMPLETE".to_string(),
            step_label: "CAD".to_string(),
            priority: "HIGH".to_string(),
            start_datetime: "20261023T090000".to_string(),
        };
        let body = String::from_utf8(create_body("1.2.3.4", &snapshot)).expect("utf8");
        assert!(
            body.contains("\"00404041\":{\"vr\":\"CS\",\"Value\":[\"INCOMPLETE\"]}"),
            "the pushed body must state INCOMPLETE: {body}"
        );
        assert!(
            !body.contains("\"READY\""),
            "a push must not upgrade an INCOMPLETE workitem to READY: {body}"
        );
        // The other three type-1 values travel too, from the same record.
        assert!(body.contains("[\"CAD\"]"), "step label: {body}");
        assert!(body.contains("[\"HIGH\"]"), "priority: {body}");
        assert!(
            body.contains("[\"20261023T090000\"]"),
            "start datetime: {body}"
        );
        // And it is still a valid Create body: SCHEDULED state, one object.
        assert!(body.contains("\"00741000\":{\"vr\":\"CS\",\"Value\":[\"SCHEDULED\"]}"));
        assert!(body.starts_with('{') && body.ends_with('}'), "{body}");
    }

    /// An attribute the record does not carry is OMITTED, never invented.
    ///
    /// The previous body hardcoded `20000101T000000` and `MEDIUM`, so a
    /// subscriber was told a start time and a priority the requester never
    /// stated - and could not tell the difference from real ones.
    #[test]
    fn create_body_omits_attributes_the_record_does_not_carry() {
        let snapshot = WorkitemSnapshot {
            input_readiness: "READY".to_string(),
            step_label: String::new(),
            priority: String::new(),
            start_datetime: String::new(),
        };
        let body = String::from_utf8(create_body("1.2.3.4", &snapshot)).expect("utf8");
        for fabricated in ["20000101T000000", "MEDIUM", "\"00741200\"", "\"00404005\""] {
            assert!(
                !body.contains(fabricated),
                "the push must not fabricate {fabricated}: {body}"
            );
        }
        // The readiness that IS known is still stated.
        assert!(body.contains("[\"READY\"]"), "{body}");
    }

    /// A caller that cannot supply readiness must not be read as READY.
    #[test]
    fn a_missing_readiness_is_never_reported_as_ready() {
        let values = FilterValues::default();
        let snapshot = WorkitemSnapshot::from_values(&values);
        assert_eq!(snapshot.input_readiness, "INCOMPLETE");
        assert_ne!(snapshot.input_readiness, "READY");
    }

    #[test]
    fn unfiltered_matches_any_label_filtered_only_its_own() {
        let reg = SubscriptionRegistry::default();
        reg.subscribe(Subscription::unfiltered(
            "ALL",
            Some("http://127.0.0.1:1/ups"),
        ));
        let mut cad_only = Subscription::unfiltered("CAD_ONLY", Some("http://127.0.0.1:2/ups"));
        cad_only.step_label = Some("CAD".into());
        reg.subscribe(cad_only);

        let names = |m: &str| {
            let mut v: Vec<String> = reg
                .matching("1.2.3", &fv(m, None))
                .into_iter()
                .map(|s| s.subscriber)
                .collect();
            v.sort();
            v
        };
        assert_eq!(
            names("CAD"),
            vec!["ALL".to_string(), "CAD_ONLY".to_string()]
        );
        assert_eq!(names("OTHER"), vec!["ALL".to_string()]);
    }

    /// A code-filtered subscription is answered from the code the fan-out
    /// passes, and the two code attributes must agree on ONE sequence item.
    #[test]
    fn a_code_filter_matches_only_its_own_code_and_scheme() {
        let reg = SubscriptionRegistry::default();
        let mut by_value = Subscription::unfiltered("BY_VALUE", Some("http://127.0.0.1:1/ups"));
        by_value.step_label = Some("CAD".into());
        by_value.code_value = Some("CXR-AI".into());
        reg.subscribe(by_value);

        let mut by_scheme = Subscription::unfiltered("BY_SCHEME", Some("http://127.0.0.1:2/ups"));
        by_scheme.step_label = Some("CAD".into());
        by_scheme.code_scheme = Some("99CLARUS".into());
        reg.subscribe(by_scheme);

        let names = |code: Option<(&str, &str)>| {
            let mut v: Vec<String> = reg
                .matching("1.2.3", &fv("CAD", code))
                .into_iter()
                .map(|s| s.subscriber)
                .collect();
            v.sort();
            v
        };

        // The right value under the right scheme: both subscribers match.
        assert_eq!(
            names(Some(("CXR-AI", "99CLARUS"))),
            vec!["BY_SCHEME".to_string(), "BY_VALUE".to_string()]
        );
        // The right value under a FOREIGN scheme: only the value subscriber.
        assert_eq!(
            names(Some(("CXR-AI", "OTHER"))),
            vec!["BY_VALUE".to_string()]
        );
        // A different value: only the scheme subscriber.
        assert_eq!(
            names(Some(("CT-ORGAN-SEG", "99CLARUS"))),
            vec!["BY_SCHEME".to_string()]
        );
        // No code at hand: a code-filtered subscription does NOT match - the
        // honest answer, since assuming a match would deliver an event the
        // subscriber asked not to receive.
        assert!(names(None).is_empty());
        // A different label matches neither, whatever the code.
        assert!(
            reg.matching("1.2.3", &fv("OTHER", Some(("CXR-AI", "99CLARUS"))))
                .is_empty()
        );
    }

    /// A subscription with no code key accepts any code.
    #[test]
    fn a_subscription_without_a_code_key_ignores_the_code() {
        let reg = SubscriptionRegistry::default();
        reg.subscribe(Subscription::unfiltered(
            "ALL",
            Some("http://127.0.0.1:1/ups"),
        ));
        assert_eq!(reg.matching("1.2.3", &fv("CAD", None)).len(), 1);
        assert_eq!(
            reg.matching("1.2.3", &fv("CAD", Some(("ANY", "ANY"))))
                .len(),
            1
        );
    }

    #[test]
    fn subscribe_replaces_rather_than_duplicating() {
        let reg = SubscriptionRegistry::default();
        reg.subscribe(Subscription::unfiltered(
            "SUB",
            Some("http://127.0.0.1:1/ups"),
        ));
        reg.subscribe(Subscription::unfiltered(
            "SUB",
            Some("http://127.0.0.1:2/ups"),
        ));
        // Re-subscription is idempotent: one entry, the newest endpoint.
        assert_eq!(reg.len(), 1);
        assert_eq!(
            reg.matching("1.2.3", &fv("CAD", None))[0]
                .endpoint
                .as_deref(),
            Some("http://127.0.0.1:2/ups")
        );
    }

    /// PS3.4 CC.2.3 tracks subscription state per AE AND per UPS. Keying the
    /// replacement on the AE Title alone made a workitem subscription destroy
    /// the Worklist subscription the same AE already held (and with it the
    /// Deletion Lock that subscription carried).
    #[test]
    fn a_workitem_subscription_does_not_replace_the_worklist_one() {
        let reg = SubscriptionRegistry::default();
        reg.subscribe(Subscription::unfiltered(
            "SUB",
            Some("http://127.0.0.1:1/worklist"),
        ));
        let mut scoped = Subscription::unfiltered("SUB", Some("http://127.0.0.1:1/wi"));
        scoped.workitem_uid = Some("1.2.3.4".to_string());
        reg.subscribe(scoped);

        assert_eq!(reg.len(), 2, "both scopes are registered");
        // The Worklist subscription still sees an unrelated workitem.
        let other = reg.matching("9.9.9", &fv("CAD", None));
        assert!(
            other.iter().any(|s| s.workitem_uid.is_none()
                && s.endpoint.as_deref() == Some("http://127.0.0.1:1/worklist")),
            "the Worklist subscription must survive: {other:?}"
        );
    }

    /// Unsubscribing from one workitem leaves the other scopes alone.
    #[test]
    fn a_scoped_unsubscribe_keeps_the_other_subscriptions() {
        let reg = SubscriptionRegistry::default();
        reg.subscribe(Subscription::unfiltered("SUB", None));
        for uid in ["1.2.3.4", "5.6.7.8"] {
            let mut scoped = Subscription::unfiltered("SUB", None);
            scoped.workitem_uid = Some(uid.to_string());
            reg.subscribe(scoped);
        }
        assert_eq!(reg.len(), 3);

        assert!(reg.unsubscribe_scoped("SUB", Some("1.2.3.4")));
        assert_eq!(reg.len(), 2, "only the named scope was dropped");

        // The whole-AE form still removes everything, which is what the
        // subscriber-addressed DELETE means.
        assert!(reg.unsubscribe("SUB"));
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn unsubscribe_reports_whether_it_removed_anything() {
        let reg = SubscriptionRegistry::default();
        reg.subscribe(Subscription::unfiltered(
            "SUB",
            Some("http://127.0.0.1:1/ups"),
        ));
        assert!(reg.unsubscribe("SUB"));
        assert!(!reg.unsubscribe("SUB"));
        assert!(reg.is_empty());
    }
}
