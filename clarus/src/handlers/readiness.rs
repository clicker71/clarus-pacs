// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Input Readiness debounce for generated CAD workitems.
//!
//! # The race this exists for
//!
//! A CAD workitem is created on the FIRST durable STOW of a study
//! (`cad_workitem::after_durable_stow`). Per-instance ingest is legal and
//! unbounded in time, so at that moment the study is usually INCOMPLETE: a
//! live run offered a workitem after 2 of 53 instances and another after 3 of
//! 36, and the performer - woken instantly by its WebSocket Notification
//! Connection - claimed them and produced structure sets over the few slices
//! that happened to be stored. The workitems then COMPLETED, so nothing
//! downstream could tell those results from results over the whole study.
//!
//! # Why readiness and not the state machine
//!
//! `InputReadinessState` (0040,4041) is Type 1 in PS3.4 Table CC.2.5-3 with
//! values INCOMPLETE and READY, and readiness is OUTSIDE the Procedure Step
//! State machine (CC.1.1 does not touch it). PS3.4 CC.2.4 obliges the SCP to
//! REFUSE a claim while the input is INCOMPLETE, with its own failure status -
//! and the performer already implements that refusal (AIW-I 4.82.4.1.1). So
//! the standard half that says "do not claim an unfinished study" is in place,
//! and this module supplies the other half: offering the workitem as
//! INCOMPLETE in the first place, and flipping it to READY once the study has
//! settled.
//!
//! # Why a debounce
//!
//! There is no signal that says "the study is complete" - a device may keep
//! adding instances for as long as it likes. The pragmatic bound is QUIET: a
//! study is READY once nothing has been stored for it for
//! [`READINESS_DEBOUNCE`]. Every new STOW re-arms the timer, so a slow upload
//! never flips early.
//!
//! The value is a constant rather than a setting. DIMSE ingest sends several
//! instances per second, so 10 seconds is many instance times longer than the
//! largest inter-instance gap a healthy sender produces, and far shorter than
//! the minutes a claim used to wait for the catch-up search. It is the kind of
//! number a deployment has no reason to differ on, and a knob here would be a
//! knob that is always left alone.
//!
//! # One sweeper, not a timer per study
//!
//! A thread per study would be a thread per study. One sweeper thread wakes on
//! a fixed tick, takes the studies whose deadline has passed, and flips them.
//! The registry is a plain `Mutex<HashMap<..>>` - a few short critical
//! sections over a map small enough to scan - so no lock is held across a
//! store write or an event publish.

use clarus_core::ups::WorkItemStore;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::handlers::ups_pure::EventSink as _;
use crate::state::AppState;

/// How long a study must be QUIET before its workitem becomes READY.
///
/// See the module documentation for why this is a constant. The range the
/// owner allowed is 5-30 s; 10 s is the middle of it.
pub const READINESS_DEBOUNCE: Duration = Duration::from_secs(10);

/// How often the sweeper looks for studies whose quiet period has elapsed.
///
/// Smaller than the debounce, so a study becomes READY within one tick of its
/// deadline rather than up to a whole debounce late.
pub const SWEEP_TICK: Duration = Duration::from_millis(500);

/// The default readiness of a workitem offered while its study is still
/// being stored.
pub const READINESS_INCOMPLETE: &str = "INCOMPLETE";

/// The readiness that invites a claim.
pub const READINESS_READY: &str = "READY";

/// Studies awaiting their quiet period, and the instant each becomes READY.
///
/// Keyed by STUDY UID rather than workitem UID: the debounce is a property of
/// the ingest, and the workitem UID is derived from the study anyway.
///
/// `Clone` shares the ONE registry rather than copying it: the map sits behind
/// an `Arc`, so a cloned handle arms and drains the same pending set. A deep
/// copy would give each holder its own timer and a study could then be
/// promoted twice.
#[derive(Clone, Default)]
pub struct Readiness {
    pending: Arc<Mutex<HashMap<String, Instant>>>,
}

impl Readiness {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Arm or re-arm the quiet period for one study.
    ///
    /// Every STOW calls this, which is what makes a long upload safe: the
    /// deadline moves forward with each instance instead of expiring under a
    /// device that is still sending.
    ///
    /// `now` is a parameter rather than a call to [`Instant::now`] inside,
    /// so the MOVEMENT of the deadline is testable without sleeping. That is
    /// not a nicety: the first version of this function was tested only for
    /// "a second STOW keeps the study pending", and a re-arm that did nothing
    /// at all passed that test. An untestable re-arm is an unverified re-arm,
    /// so the clock is an input.
    pub fn arm(&self, study_instance_uid: &str, now: Instant) {
        let deadline = now + READINESS_DEBOUNCE;
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        pending.insert(study_instance_uid.to_string(), deadline);
    }

    /// The instant a study becomes READY, if it is still waiting.
    ///
    /// For the tests and the status line: it lets a caller assert that a
    /// re-arm MOVED the deadline rather than merely kept the study pending.
    #[must_use]
    pub fn deadline(&self, study_instance_uid: &str) -> Option<Instant> {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(study_instance_uid)
            .copied()
    }

    /// Remove every study whose deadline has passed, returning them.
    ///
    /// The lock is released before the caller touches the store, so a slow
    /// write cannot block a concurrent STOW from arming its own study.
    pub fn take_due(&self, now: Instant) -> Vec<String> {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let due: Vec<String> = pending
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(uid, _)| uid.clone())
            .collect();
        for uid in &due {
            pending.remove(uid);
        }
        due
    }

    /// Whether any study is still waiting. For tests and the status line.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// What one promotion attempt achieved.
///
/// The sweeper needs to tell these apart: an ENVIRONMENTAL failure is worth
/// retrying, a TERMINAL outcome is not. With a bare `bool` the sweep re-armed
/// on every `false`, so a study whose promotion was complete - no workitem
/// left, every workitem claimed or cancelled, nothing minted - was retried
/// every tick for the rest of the process lifetime: a store search plus a
/// WARN every time, and a registry entry that never went away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Promotion {
    /// At least one workitem is READY (or was already).
    Ready,
    /// The study has nothing that can be promoted and nothing that can be
    /// fixed by trying again: no workitems, or every one of them has left
    /// SCHEDULED. This is a FINAL state.
    NothingToPromote,
    /// The store could not be read or written. Environmental, retryable.
    StoreError,
}

/// Flip every workitem of one study to READY and publish its offer.
///
/// Returns [`Promotion::Ready`] when at least one workitem is now READY.
/// A workitem that was ALREADY READY also counts as `Ready` - the performer
/// may act, which is what the caller cares about.
///
/// A workitem that has since been CLAIMED or CANCELLED does NOT count: it has
/// left SCHEDULED, so the readiness flip no longer applies to it and no offer
/// is published for it. When every workitem of the study is in that state the
/// answer is [`Promotion::NothingToPromote`], not `Ready` - the sweeper must
/// stop retrying a study it can no longer influence.
///
/// The event is published AFTER the record is durably READY, so a performer
/// woken by it cannot race the flip and see INCOMPLETE. A claim that arrives
/// before the flip is refused by the performer's own INCOMPLETE guard, which
/// is the standard's arrangement rather than a gap.
pub fn promote(state: &AppState, study_instance_uid: &str) -> Promotion {
    let store: WorkItemStore = super::ups_pure::workitem_store(state);
    // ONE study can carry SEVERAL workitems - one per catalog entry that
    // matched its modality (see `config::UpsConfig::catalog_for_modality`) -
    // so this walks the study's OWN stored workitems rather than deriving the
    // single UID it used to.
    //
    // Finding them by their `input_study_uid` rather than by RECOMPUTING the
    // catalog is deliberate: the catalog is selected by modality, the
    // modality lives in the study manifest, and the readiness path has no
    // manifest in hand (it runs on a debounce timer, long after the STOW).
    // Recomputing from a modality this path would have to fetch is a second
    // source of truth that can disagree with what the mint actually created -
    // and the failure mode would be silent, a workitem nobody promotes. The
    // stored records ARE the catalog as it was realised, so reading them back
    // is both simpler and impossible to desynchronise.
    let uids = match workitem_uids_for_study(&store, study_instance_uid) {
        Ok(uids) => uids,
        Err(()) => return Promotion::StoreError,
    };
    if uids.is_empty() {
        // Nothing was ever minted for this study, so nothing will become
        // ready no matter how often the sweep runs.
        return Promotion::NothingToPromote;
    }

    let mut any_ready = false;
    let mut any_store_error = false;
    for workitem_uid in &uids {
        match promote_one(state, &store, workitem_uid) {
            Promotion::Ready => any_ready = true,
            Promotion::NothingToPromote => {}
            Promotion::StoreError => any_store_error = true,
        }
    }
    if any_store_error {
        // A failure outranks a success: the workitems that could be flipped
        // were, and the sweep must come back for the rest.
        return Promotion::StoreError;
    }
    if any_ready {
        return Promotion::Ready;
    }
    // Every workitem left SCHEDULED on its own: claimed, cancelled or
    // completed while the study settled. Retrying cannot change that.
    Promotion::NothingToPromote
}

/// Every workitem of one study, by UID.
///
/// A linear scan over the store directory, which is the same full-scan
/// precedent [`WorkItemStore::search`] documents: workitem counts are small,
/// and this runs once per settled study rather than per request.
///
/// A study with no workitems yields an EMPTY vector, which is not an error:
/// that is a study never minted into one (the `[ups]` `task_producer` switch
/// is off, or an operator deleted them). A store that cannot be READ is
/// different, and the caller must be able to tell - hence `Result`.
///
/// `pub(crate)` because the delete cascade needs the same answer: a study's
/// workitems are the records whose `input_study_uid` names it, and the study
/// UID alone no longer determines the workitem UID (the catalog code is part
/// of it). Both callers therefore have to ask the store rather than recompute
/// a name, which keeps "how a workitem is found" defined once.
pub(crate) fn workitem_uids_for_study(
    store: &WorkItemStore,
    study_instance_uid: &str,
) -> Result<Vec<String>, ()> {
    let Ok((records, _total)) = store.search(&clarus_core::ups::store::StoreFilter::default())
    else {
        // A store that cannot be read is an environmental failure: the sweep
        // retries, and the caller keeps the study armed.
        crate::clog_warn!(
            study_instance_uid = %study_instance_uid,
            "readiness: cannot list the workitem store to promote the study"
        );
        return Err(());
    };
    let mut uids: Vec<String> = records
        .into_iter()
        .filter(|record| record.input_study_uid.as_deref() == Some(study_instance_uid))
        .map(|record| record.workitem_uid)
        .collect();
    // Canonical order, so the promotion is deterministic and a log or a test
    // reads the same every run. The store's own scan order is directory
    // order, which the filesystem does not promise.
    uids.sort_unstable();
    Ok(uids)
}

/// Flip ONE workitem to READY and publish its offer.
///
/// The per-workitem half of [`promote`]; the study-level function finds the
/// study's workitems and calls this for each.
fn promote_one(state: &AppState, store: &WorkItemStore, workitem_uid: &str) -> Promotion {
    let existing = match store.find_by_uid(workitem_uid) {
        Ok(Some(record)) => record,
        // No workitem: the study was never made durable into one, or an
        // operator deleted it while the timer ran. Neither is an error, and
        // neither becomes true by retrying.
        Ok(None) => return Promotion::NothingToPromote,
        Err(error) => {
            crate::clog_warn!(
                workitem_uid = %workitem_uid,
                error = %error,
                "readiness: cannot read the workitem to promote it"
            );
            return Promotion::StoreError;
        }
    };

    // A workitem that is no longer SCHEDULED has been claimed, cancelled or
    // completed while the study was settling. Rewriting its readiness would
    // change what an in-flight or finished claim was about, so it is left
    // exactly as the performer left it - and it will never be SCHEDULED
    // again, so this is final.
    if existing.workflow_state != super::cad_workitem::STATE_SCHEDULED {
        return Promotion::NothingToPromote;
    }
    if existing.input_readiness.as_deref() == Some(READINESS_READY) {
        return Promotion::Ready;
    }

    let mut record = existing;
    record.input_readiness = Some(READINESS_READY.to_string());
    record.updated_at = Some(super::ups_pure::now_stamp());
    // `None` for the expected transaction UID: this is a readiness flip, not
    // a state transition, and no performer can hold the workitem (it is
    // still SCHEDULED, which is exactly what no-performer-held means).
    if let Err(error) = store.cas_update(&record, None) {
        crate::clog_warn!(
            workitem_uid = %workitem_uid,
            error = %error,
            "readiness: cannot write the READY flip"
        );
        return Promotion::StoreError;
    }

    crate::clog_info!(
        workitem_uid = %workitem_uid,
        "CAD workitem input is READY; the study has settled"
    );
    // The SAME envelope the create path used to publish, from the same sink,
    // so the SSE fan-out and every subscribed Notification Connection see one
    // shape. This is now the ONLY place a generated workitem is announced,
    // which is what keeps a performer from being woken before the study is
    // whole. The filter keys travel with it too: a channel subscriber
    // narrowed by Procedure Step Label or Scheduled Workitem Code is answered
    // here exactly as it would be on the create path.
    state.ups_events.notify_created(
        workitem_uid,
        super::ups::event_bus::FilterRouting::from_record(&record),
    );
    // The SAME envelope the create path publishes, and now the same FILTER:
    // the code travels with the label, so a subscription filtered on the
    // scheduled code is answered here exactly as it would be on the create
    // path. Passing only the label would silently drop a code-filtered
    // subscriber from every generated workitem.
    crate::push::push_to_subscribers_with_values(
        workitem_uid,
        &crate::push::FilterValues::from_record(&record),
    );
    Promotion::Ready
}

/// Start the sweeper thread. Runs until the process exits.
///
/// Deliberately not shutdown-aware: it holds no resources a clean exit needs
/// to release, and a process that is going away takes its threads with it.
/// A join handle would have to be stored in `AppState` and joined on the
/// shutdown path, which is more surface than the behaviour is worth.
pub fn spawn_sweeper(state: Arc<AppState>) {
    std::thread::Builder::new()
        .name("readiness".to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(SWEEP_TICK);
                sweep_once(&state, Instant::now());
            }
        })
        .expect("the readiness sweeper thread starts");
}

/// One sweeper pass: promote every study whose quiet period has expired.
///
/// Split out of the thread loop so the re-arm rule is testable with a
/// controllable clock instead of a sleep.
pub fn sweep_once(state: &AppState, now: Instant) {
    for study_uid in state.readiness.take_due(now) {
        // `take_due` REMOVED the study, so the re-arm decision is what decides
        // whether it ever comes back. Only an ENVIRONMENTAL failure earns a
        // retry:
        //
        // - `StoreError`: the store could not be read or written. The study
        //   was removed by `take_due`, so without a re-arm nothing would ever
        //   bring it back - nothing re-arms it except a later STOW, and a
        //   study that has stopped arriving never gets one. Re-arming is
        //   safe because the flip and its notification are idempotent.
        //
        // - `NothingToPromote`: the study has nothing to promote and nothing
        //   that trying again can fix. Re-arming HERE was the defect: a
        //   terminal study was retried every tick for the life of the
        //   process, costing a store search and a WARN each time, and pinning
        //   its registry entry forever. A study whose workitems were all
        //   claimed or cancelled, or which was never minted into one, never
        //   becomes promotable again.
        //
        // - `Ready`: done, nothing to retry.
        match promote(state, &study_uid) {
            Promotion::Ready => {}
            Promotion::NothingToPromote => {}
            Promotion::StoreError => {
                state.readiness.arm(&study_uid, now);
                crate::clog_warn!(
                    study_instance_uid = %study_uid,
                    "readiness: the workitem store failed; the study is re-armed"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{READINESS_DEBOUNCE, Readiness};
    use std::time::{Duration, Instant};

    /// The STUDIES table must contain exactly the studies that were armed.
    fn due_at(registry: &Readiness, now: Instant) -> Vec<String> {
        let mut due = registry.take_due(now);
        due.sort();
        due
    }

    /// A study is due at its own deadline and not one tick before it.
    #[test]
    fn a_study_becomes_due_exactly_at_its_deadline() {
        let registry = Readiness::new();
        let t0 = Instant::now();
        registry.arm("1.2.3", t0);

        // Just short of the debounce: still waiting.
        let just_short = t0 + READINESS_DEBOUNCE - Duration::from_millis(1);
        assert_eq!(
            due_at(&registry, just_short),
            Vec::<String>::new(),
            "the quiet period has not elapsed"
        );
        // The boundary is inclusive, which is what makes the sweeper's
        // `deadline <= now` test and this assertion the same rule.
        assert_eq!(due_at(&registry, t0 + READINESS_DEBOUNCE), vec!["1.2.3"]);
    }

    /// Re-arming pushes the deadline FORWARD, so a study still being written
    /// to never becomes due on the strength of an earlier STOW.
    ///
    /// This is the property the smoke test could not see: it asserted the
    /// study stayed pending, and a re-arm that did nothing also stayed
    /// pending. Here the clock is an input, so no sleep is needed and the
    /// assertion is about the MOVEMENT itself.
    #[test]
    fn re_arming_moves_the_deadline_and_keeps_the_study_pending() {
        let registry = Readiness::new();
        let t0 = Instant::now();
        registry.arm("1.2.3", t0);
        let first_deadline = registry.deadline("1.2.3").expect("armed");
        assert_eq!(first_deadline, t0 + READINESS_DEBOUNCE);

        // A second STOW arrives inside the window, at t1 > t0.
        let t1 = t0 + Duration::from_secs(1);
        registry.arm("1.2.3", t1);
        let second_deadline = registry.deadline("1.2.3").expect("re-armed");
        assert!(
            second_deadline > first_deadline,
            "the deadline must move forward: {first_deadline:?} then {second_deadline:?}"
        );
        assert_eq!(second_deadline, t1 + READINESS_DEBOUNCE);

        // The instant that WOULD have been due under the first arm is not
        // due under the second - the whole point of re-arming.
        let would_have_been = t0 + READINESS_DEBOUNCE - Duration::from_millis(1);
        assert_eq!(
            due_at(&registry, would_have_been),
            Vec::<String>::new(),
            "the re-arm is what keeps the study from being promoted here"
        );
        // And the new deadline does release it.
        assert_eq!(due_at(&registry, second_deadline), vec!["1.2.3"]);
    }

    /// One study's re-arm does not disturb another's countdown.
    #[test]
    fn re_arming_one_study_leaves_another_alone() {
        let registry = Readiness::new();
        let t0 = Instant::now();
        registry.arm("1.2.3", t0);
        registry.arm("4.5.6", t0);

        // Only the first study sees another STOW.
        registry.arm("1.2.3", t0 + Duration::from_secs(4));
        assert_eq!(registry.deadline("4.5.6"), Some(t0 + READINESS_DEBOUNCE));

        // So at the original deadline only the untouched study is due.
        assert_eq!(due_at(&registry, t0 + READINESS_DEBOUNCE), vec!["4.5.6"]);
        assert_eq!(registry.pending_len(), 1);
    }

    /// Taking a due study removes it, so it cannot be promoted twice.
    #[test]
    fn a_due_study_is_removed_from_the_registry() {
        let registry = Readiness::new();
        let t0 = Instant::now();
        registry.arm("1.2.3", t0);

        assert_eq!(due_at(&registry, t0 + READINESS_DEBOUNCE), vec!["1.2.3"]);
        assert_eq!(
            due_at(&registry, t0 + READINESS_DEBOUNCE),
            Vec::<String>::new()
        );
        assert_eq!(registry.pending_len(), 0);
        assert_eq!(registry.deadline("1.2.3"), None);
    }

    /// m17: a promotion that does not complete must put the study BACK.
    ///
    /// `take_due` removes a due study, and nothing re-arms it except a later
    /// STOW - which a settled study never gets. So a sweeper that ignored the
    /// promotion's result dropped the study out of the registry permanently and
    /// its workitem stayed INCOMPLETE forever, offered to nobody. This models
    /// exactly the sweeper's sequence (take due, promote, re-arm on failure)
    /// without a store: the assertion is that the study is still pending and
    /// has a LATER deadline than the one that just expired.
    #[test]
    fn a_failed_promotion_is_re_armed_rather_than_dropped() {
        let registry = Readiness::new();
        let t0 = Instant::now();
        registry.arm("1.2.3", t0);
        let expired = t0 + READINESS_DEBOUNCE;

        let due = registry.take_due(expired);
        assert_eq!(due, vec!["1.2.3"], "the study came due");
        assert_eq!(
            registry.pending_len(),
            0,
            "taking it due removed it - which is why the failure must re-arm"
        );

        // The sweeper's failure branch.
        let retry_now = expired;
        registry.arm(&due[0], retry_now);

        assert_eq!(
            registry.pending_len(),
            1,
            "a failed promotion must leave the study pending, not forgotten"
        );
        let next = registry
            .deadline("1.2.3")
            .expect("the study has a fresh deadline");
        assert!(
            next > expired,
            "the retry must wait a fresh quiet period, not fire immediately"
        );
        assert_eq!(next, retry_now + READINESS_DEBOUNCE);
        // And it will come due again, so the retry is real rather than a
        // registry entry nothing will ever select.
        assert_eq!(due_at(&registry, next), vec!["1.2.3"]);
    }
}
