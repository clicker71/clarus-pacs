// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The notification channel loop: open, read Event Reports, reconnect.
//!
//! This module is the clinfer half of the AIW-I Triggered Pull model. The
//! origin no longer POSTs work into the sidecar; the sidecar holds a
//! WebSocket Notification Connection (PS3.18 8.10.4) and claims when an
//! Event Report says a workitem it cares about reached SCHEDULED.
//!
//! # What this loop does NOT do
//!
//! It never claims. Claiming is the worker's job, and the worker is a
//! separate thread: a claim performs inference, which can take as long as
//! a processor's timeout budget, and blocking the channel read on it would
//! let the origin's send buffer fill while the sidecar was busy. The loop's
//! only output is a UID enqueued for the worker, so the read path stays
//! O(1) in the size of the work.
//!
//! # Ordering: subscribe, then connect
//!
//! The server gates channel delivery on a subscription existing for the
//! requester (PS3.18 8.10.4 opens the connection; 11.10 is what decides
//! which events the requester wants). Connecting first yields a channel
//! that is open and permanently silent - indistinguishable from a broken
//! notify path - so the caller MUST subscribe first. [`run_with`] performs
//! the subscribe itself, immediately before each connect, so the ordering
//! holds on the first attempt and on every reconnect without the caller
//! having to remember it twice.
//!
//! # Reconnection
//!
//! A dropped channel reconnects with exponential backoff, 1s doubling to
//! a 30s cap. The backoff is internal constants rather than configuration:
//! it is not a policy any deployment has reason to differ on, and a knob
//! here would be a knob that is always left alone.
//!
//! Before EVERY connection attempt the loop re-subscribes, and after every
//! successful connect it re-runs the caller's search. A channel that was
//! down for a minute missed an unknown number of Event Reports, and the
//! search is the only thing that can say what is still SCHEDULED;
//! reconnecting without it would silently drop that window. The subscription
//! is the other half: the origin holds subscriptions in process memory, so a
//! restarted origin has forgotten this performer and would deliver nothing
//! even though the channel reads as open.

use crate::receiver::Counters;
use crate::ups::State;
use crate::ws::{Channel, ChannelEvent};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::Duration;

/// First reconnect delay.
pub const BACKOFF_FIRST: Duration = Duration::from_secs(1);

/// Ceiling for the reconnect delay.
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// The longest a single blocking step may hold the loop.
///
/// A step is one `connect`/handshake attempt, bounded by the handshake
/// timeout on both the connect and the response read. Kept here so the
/// shutdown reaction of the loop is a stated bound rather than a property of
/// whichever timeout the transport happens to use.
pub const STEP_MAX: Duration = Duration::from_secs(2 * 10);

/// Settling delay between two failed connects that returned at once.
///
/// A closed port refuses in milliseconds, and the closing read timeout is
/// what would otherwise pace the loop. 250 ms keeps a dead origin from being
/// redialled in a tight loop without adding anything an operator can
/// perceive on top of a 1 s first backoff.
const REDIAL_DELAY: Duration = Duration::from_millis(250);

/// The Data Set attribute that names the UPS the Event Report is about.
///
/// PS3.18 8.10.5.1.1 makes the payload a DICOM JSON Data Set, and an Event
/// Report identifies the affected instance through the COMMAND attribute
/// (0000,1000) Affected SOP Instance UID - the same value the origin writes
/// into the report alongside the affected SOP Class UID, Message ID and Event
/// Type ID. It is NOT (0008,0018): that is the SOP Instance UID of a workitem
/// DOCUMENT, which only appears in the create/update bodies.
const TAG_AFFECTED_SOP_INSTANCE_UID: &str = "00001000";

/// The Data Set attribute that carries the state the workitem is now in
/// (PS3.4 CC.2.4.3: Procedure Step State).
const TAG_PROCEDURE_STEP_STATE: &str = "00741000";

/// The next backoff after `current`, doubling to [`BACKOFF_MAX`].
///
/// Pure, and separated from the loop so the growth rule is pinned by a test
/// rather than inferred from a sleep that a test cannot observe.
#[must_use]
pub fn next_backoff(current: Duration) -> Duration {
    let doubled = current.saturating_mul(2);
    if doubled > BACKOFF_MAX {
        BACKOFF_MAX
    } else {
        doubled
    }
}

/// Which workitem an Event Report is about, when it is one this sidecar
/// should act on.
///
/// The payload is a DICOM JSON Data Set (PS3.18 8.10.5.1.1), so each
/// attribute is an object with a `vr` and a `Value` array:
/// `{"00001000":{"vr":"UI","Value":[".."]},"00741000":{"vr":"CS",
/// "Value":["SCHEDULED"]},..}`. Reading it as a flat object (the retired
/// envelope) would find no state at all and the channel would never wake.
///
/// Returns `None` for anything that is not an actionable report: a
/// non-SCHEDULED state change (the origin also notifies IN PROGRESS, COMPLETED
/// and CANCELED), a body that is not a DICOM JSON Data Set, or a report with no
/// UID. All three are ordinary traffic on a shared channel, not errors.
///
/// The state check is what keeps the channel from waking the worker for
/// work this performer cannot do: only SCHEDULED means "available to
/// claim". It is the same predicate the worker applies before claiming, so
/// a report that slips past this check is still refused there.
#[must_use]
pub fn actionable_uid(payload: &[u8]) -> Option<String> {
    let document = crate::json::parse(payload).ok()?;
    let state = document.attribute(TAG_PROCEDURE_STEP_STATE)?;
    if State::parse(state) != Some(State::Scheduled) {
        return None;
    }
    let uid = document.attribute(TAG_AFFECTED_SOP_INSTANCE_UID)?;
    if uid.trim().is_empty() {
        return None;
    }
    Some(uid.to_string())
}

/// What one pass of the loop needs from its caller.
pub struct Wiring {
    /// Host of the origin, without a port.
    pub host: String,
    /// Port of the origin.
    pub port: u16,
    /// Base path, e.g. `/dicomweb`.
    pub base_path: String,
    /// The requester whose subscription authorises this channel.
    pub requester: String,
    /// The identity presented on the handshake.
    pub identity_user: String,
    /// Seconds between catch-up searches while the channel is DOWN; `0`
    /// disables the fallback.
    ///
    /// The interval belongs to the deployment, not to the loop: an origin
    /// that never accepts a Notification Connection is a property of that
    /// origin, and the same sidecar pointed at a conformant origin must not
    /// pay for it.
    pub poll_interval_secs: u32,
}

/// Run the channel loop until `shutdown` is set.
///
/// `subscribe` is called BEFORE every connection attempt, including the
/// first. The origin keeps subscriptions in process memory, so a restarted
/// origin has forgotten this performer while the channel reconnects happily:
/// the channel is then open and permanently silent. Re-subscribing first is
/// what restores the delivery path, and it is idempotent on the origin, so
/// the call the caller already made at startup costs nothing to repeat.
///
/// `redeliver` is called after EVERY successful connect, including the
/// first. A channel that was down - or that had never been up - missed an
/// unknown number of Event Reports, and the search is the only thing that
/// can say what is still SCHEDULED. On the first connect it repeats the
/// caller's startup catch-up on purpose: the window between that search's
/// snapshot and the channel becoming readable is exactly the window nothing
/// else covers, and a repeated search costs one query whose claims the
/// origin arbitrates. It returns the number of workitems it saw, which is
/// logged and nothing more.
///
/// Never returns an error: a channel failure is a reconnect, and a
/// permanent failure would be a reason to keep trying rather than to exit
/// a sidecar whose catch-up search still finds work.
pub fn run_with<F, S>(
    wiring: &Wiring,
    queue: SyncSender<String>,
    counters: Arc<Counters>,
    shutdown: Arc<AtomicBool>,
    mut redeliver: F,
    mut subscribe: S,
) where
    F: FnMut() -> usize,
    S: FnMut() -> Result<(), String>,
{
    let mut backoff = BACKOFF_FIRST;
    // Both clocks OUTSIDE the iteration. Declared inside, every reconnect
    // started the poll over and the search cadence became "once per
    // reconnect delay" - the very coupling this fallback exists to break.
    let poll = Duration::from_secs(u64::from(wiring.poll_interval_secs));
    let mut since_poll = Duration::ZERO;

    while !shutdown.load(Ordering::Relaxed) {
        // Subscribe BEFORE the channel opens. ORDER IS LOAD-BEARING: the
        // origin gates delivery on a subscription existing for the
        // requester, so connecting first yields an open, silent channel.
        // A failure is a warning and not a refusal to connect: the
        // catch-up search below still finds everything that is SCHEDULED,
        // so a performer without a subscription loses latency, not work.
        match subscribe() {
            Ok(()) => println!("notify:    re-subscribed before the reconnect"),
            Err(err) => {
                eprintln!("clinfer: WARNING: re-subscribe failed: {err}");
                eprintln!(
                    "clinfer: the origin will not filter for this performer; only the searches run"
                );
            }
        }

        match Channel::connect(
            &wiring.host,
            wiring.port,
            &wiring.base_path,
            &wiring.requester,
            &wiring.identity_user,
        ) {
            Ok(mut channel) => {
                // The search runs on EVERY connect, first included, and
                // BEFORE the channel starts delivering: an event that
                // arrived first would be claimed alongside a search result
                // for the same UID. The worker's claim is what arbitrates,
                // but running the search first keeps the log order the
                // order things happened in.
                let seen = redeliver();
                println!(
                    "notify:    channel open as {} at {}:{}{}; catch-up saw {seen} workitem(s)",
                    wiring.requester, wiring.host, wiring.port, wiring.base_path
                );
                counters.ws_connects.fetch_add(1, Ordering::Relaxed);
                if counters.ws_connects.load(Ordering::Relaxed) > 1 {
                    counters.ws_reconnects.fetch_add(1, Ordering::Relaxed);
                }

                if serve_channel(&mut channel, &queue, &counters, &shutdown) {
                    // A clean shutdown, not a drop: do not reconnect.
                    return;
                }
                println!("notify:    channel closed; reconnecting");
                // A channel that lived a while is not a failure of the
                // origin's capacity: start the delay small again. The two
                // upper bounds below are the ones that matter, not the
                // reset - without them a peer that accepts and drops at
                // once would hold the loop in a loop of instant reconnects.
                backoff = BACKOFF_FIRST;
            }
            Err(err) => {
                eprintln!("clinfer: WARNING: channel open failed: {err}");
            }
        }

        if shutdown.load(Ordering::Relaxed) {
            return;
        }

        // FALLBACK while there is no channel: run the catch-up search on its
        // own fixed interval. Without this the Err arm did nothing but
        // sleep, so on an origin that never opens the channel a workitem
        // created after startup was invisible until a restart - the push
        // path was the only trigger, and it was the path that was missing.
        //
        // The poll clock is NOT kept from the last search time. It
        // accumulates every slice waited on, so it tracks time and not
        // iterations, and it is declared OUTSIDE the loop: measured per
        // iteration it started over on every reconnect, and the search then
        // came once per reconnect delay instead of once per interval.
        //
        // The delay is the whole backoff, not what is left of it after the
        // failed step: a step that took a second would otherwise subtract
        // that second from the delay and from the poll's credit at the same
        // time, and the search slipped one slice past its deadline on every
        // iteration.
        let wait = backoff.max(REDIAL_DELAY);
        wait_before_reconnect(wait, poll, &mut since_poll, &mut redeliver, &shutdown);
        backoff = next_backoff(backoff);
    }
}

/// Sleep `at_most` in slices, running the fallback search on `interval`.
///
/// Two independent clocks share one sleep: the RECONNECT delay (which
/// doubles, so a broken origin is not hammered) and the POLL interval
/// (fixed, because how often work appears is not a property of how many
/// times the handshake failed). Both are measured in the same slice so a
/// shutdown is noticed within a slice either way. `since_poll` is carried
/// across calls so a tick that lands inside this wait is not lost.
///
/// The search only ENQUEUES; the worker remains the only claimer, so a poll
/// that races a later connect cannot double-claim.
fn wait_before_reconnect<F>(
    at_most: Duration,
    interval: Duration,
    since_poll: &mut Duration,
    mut redeliver: F,
    shutdown: &AtomicBool,
) where
    F: FnMut() -> usize,
{
    let polling = !interval.is_zero();
    let mut slept = Duration::ZERO;
    while slept < at_most && !shutdown.load(Ordering::Relaxed) {
        // A slice long enough to matter but short enough that a long
        // reconnect delay still notices a shutdown promptly.
        let slice = POLL_SLICE.min(at_most - slept);
        std::thread::sleep(slice);
        slept += slice;
        if !polling {
            continue;
        }
        *since_poll += slice;
        // `>=` rather than `==`: a slice that overshoots the boundary
        // exactly once must still fire, and with a fixed slice the boundary
        // is reachable, not approximate.
        if *since_poll >= interval {
            *since_poll = Duration::ZERO;
            let seen = redeliver();
            println!("notify:    no channel; catch-up saw {seen} workitem(s)");
        }
    }
}

/// The longest a sleep slice may last, which is both the shutdown-reaction
/// granularity and the resolution of the fallback poll.
///
/// 100 ms: a sidecar that took half a minute to react to SIGTERM would look
/// wedged to a supervisor, and a poll boundary is only as precise as the
/// slice it is measured in.
const POLL_SLICE: Duration = Duration::from_millis(100);

/// Read from an open channel until it closes or shutdown begins.
///
/// Returns `true` when the channel ended because of shutdown, `false` when
/// the peer closed it or the read failed, which the caller treats as a
/// reason to reconnect.
fn serve_channel(
    channel: &mut Channel,
    queue: &SyncSender<String>,
    counters: &Counters,
    shutdown: &AtomicBool,
) -> bool {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            channel.close();
            return true;
        }
        match channel.read_event() {
            Ok(ChannelEvent::Message(payload)) => {
                counters.ws_events.fetch_add(1, Ordering::Relaxed);
                let Some(uid) = actionable_uid(&payload) else {
                    continue;
                };
                // A full queue is the worker being busy, not a failure:
                // the catch-up search on the next reconnect is what
                // recovers the UID. Dropping is counted so an operator can
                // see it happened rather than inferring it from a missing
                // claim.
                if queue.try_send(uid.clone()).is_err() {
                    counters.ws_dropped.fetch_add(1, Ordering::Relaxed);
                    eprintln!(
                        "clinfer: WARNING: notification for {uid} dropped: the worker queue is full"
                    );
                }
            }
            // An idle tick: the loop's chance to notice shutdown.
            Ok(ChannelEvent::Idle) => continue,
            Ok(ChannelEvent::Closed) => return false,
            Err(err) => {
                eprintln!("clinfer: WARNING: channel read failed: {err}");
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_doubles_and_then_stops() {
        let mut delay = BACKOFF_FIRST;
        let mut seen = vec![delay];
        for _ in 0..8 {
            delay = next_backoff(delay);
            seen.push(delay);
        }
        assert_eq!(
            seen,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
            ],
            "1s doubling to a 30s cap, and it never exceeds the cap"
        );
    }

    #[test]
    fn the_cap_is_not_overshot_by_a_single_doubling_step() {
        // 16 -> 32 would overshoot; the rule must clamp instead. Pinned
        // separately because a `min` applied at the wrong point would
        // still satisfy the sequence test above if the cap were reached
        // exactly by doubling.
        assert_eq!(next_backoff(Duration::from_secs(16)), BACKOFF_MAX);
        assert_eq!(next_backoff(Duration::from_secs(30)), BACKOFF_MAX);
        assert_eq!(next_backoff(Duration::from_secs(3600)), BACKOFF_MAX);
    }

    #[test]
    fn a_scheduled_report_yields_its_uid() {
        // The real payload shape the origin sends (ups_event_json): a DICOM
        // JSON Data Set, so each attribute is an object with a `vr` and a
        // `Value` array.
        let payload = br#"{"00001000":{"vr":"UI","Value":["1.2.3.4"]},"00404041":{"vr":"CS","Value":["READY"]},"00741000":{"vr":"CS","Value":["SCHEDULED"]}}"#;
        assert_eq!(actionable_uid(payload).as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn the_retired_envelope_is_not_actionable() {
        // A payload still carrying the old flat envelope has no DICOM
        // attribute entry, so there is no state to read: the channel must
        // stay quiet rather than guess that `new_state` means the same thing.
        let payload = br#"{"workitem_uid":"1.2.3.4","previous_state":"","new_state":"SCHEDULED","timestamp":"t","event_id":1}"#;
        assert_eq!(actionable_uid(payload), None);
    }

    #[test]
    fn the_sop_instance_uid_tag_is_not_the_report_identity() {
        // (0008,0018) is the SOP Instance UID of a workitem DOCUMENT; an
        // Event Report identifies the affected instance with (0000,1000). A
        // report carrying only the former must not wake the worker.
        let payload = br#"{"00080018":{"vr":"UI","Value":["1.2.3.4"]},"00741000":{"vr":"CS","Value":["SCHEDULED"]}}"#;
        assert_eq!(actionable_uid(payload), None);
    }

    #[test]
    fn a_lowercase_scheduled_report_is_still_actionable() {
        // The state value is a code string; the standard writes it in
        // upper case, but the parser is case-insensitive and the channel
        // must agree with it rather than be stricter than the worker.
        let payload = br#"{"00001000":{"vr":"UI","Value":["1.2.3.4"]},"00741000":{"vr":"CS","Value":["scheduled"]}}"#;
        assert_eq!(actionable_uid(payload).as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn an_in_progress_report_is_not_actionable() {
        // Someone else holds it. Waking the worker would only produce a
        // claim conflict, which is a wasted round trip and a misleading
        // counter.
        let payload = br#"{"00001000":{"vr":"UI","Value":["1.2.3.4"]},"00741000":{"vr":"CS","Value":["IN PROGRESS"]}}"#;
        assert_eq!(actionable_uid(payload), None);
    }

    #[test]
    fn a_completed_report_is_not_actionable() {
        let payload = br#"{"00001000":{"vr":"UI","Value":["1.2.3.4"]},"00741000":{"vr":"CS","Value":["COMPLETED"]}}"#;
        assert_eq!(actionable_uid(payload), None);
    }

    #[test]
    fn a_canceled_report_is_not_actionable() {
        let payload = br#"{"00001000":{"vr":"UI","Value":["1.2.3.4"]},"00741000":{"vr":"CS","Value":["CANCELED"]}}"#;
        assert_eq!(actionable_uid(payload), None);
    }

    #[test]
    fn a_scheduled_report_without_a_uid_is_not_actionable() {
        // There is nothing to claim, so there is nothing to enqueue.
        let payload = br#"{"00741000":{"vr":"CS","Value":["SCHEDULED"]}}"#;
        assert_eq!(actionable_uid(payload), None);
    }

    #[test]
    fn a_scheduled_report_with_a_blank_uid_is_not_actionable() {
        let payload = br#"{"00001000":{"vr":"UI","Value":["   "]},"00741000":{"vr":"CS","Value":["SCHEDULED"]}}"#;
        assert_eq!(actionable_uid(payload), None);
    }

    #[test]
    fn a_report_without_a_state_is_not_actionable() {
        // An event that carries no state is not a state change; the
        // channel has nothing to decide on, so it stays quiet.
        let payload = br#"{"00001000":{"vr":"UI","Value":["1.2.3.4"]}}"#;
        assert_eq!(actionable_uid(payload), None);
    }

    #[test]
    fn a_body_that_is_not_json_is_not_actionable() {
        // The handshake and the frame codec are what guard the channel;
        // by the time a payload reaches here it is whatever the origin
        // sent, and a non-JSON body must be ignored rather than panic.
        assert_eq!(actionable_uid(b"not json at all"), None);
        assert_eq!(actionable_uid(b""), None);
        assert_eq!(actionable_uid(b"<xml/>"), None);
    }

    #[test]
    fn an_error_body_is_not_actionable() {
        // A DICOM JSON failure response is a valid JSON object with no
        // report identity. It must not be mistaken for a workitem.
        let payload = br#"{"error":"Bad Request","message":"no"}"#;
        assert_eq!(actionable_uid(payload), None);
    }
}
