// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! UPS event fan-out - receiver/transport.
//!
//! The `EventSink` abstraction lets the pure CRUD layer publish workitem
//! events without knowing which transport is listening. group 5 makes
//! the whole fan-out stack synchronous: both buses here are backed by
//! `std::sync::mpsc::sync_channel` (no `tokio::sync::broadcast`, no tokio
//! runtime - the sync dispatcher SSE wrapper drives `mpsc::Receiver` with a
//! blocking `recv_timeout`).
//!
//! This module provides two bus types:
//! - `MpscFanoutBus` - the synchronous fan-out driven by the sync dispatcher.
//! - `DualEventSink` - publishes to **both** the `UpsEventBus` (kept for the
//!   `AppState` field and test wiring) and the `MpscFanoutBus`. The publish
//!   side (`EventSink` call sites in `ups_pure`) is not touched - it keeps
//!   calling `publish`/`notify_*`; `AppState.ups_events` is a `DualEventSink`.
//!
//! # Overflow semantics
//!
//! Bounded `sync_channel(128)` cannot drop the oldest event, so on a full
//! channel the **newest** event is dropped and debug-logged (deliberate,
//! nginx-way - never block the publisher). When a receiver is dropped the
//! associated sender is garbage-collected under the same lock that publishes
//! (no separate GC task, no locks held across send).

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use crate::handlers::ups::event_bus::{FilterRouting, UpsEvent, UpsEventBus};
use crate::handlers::ups_pure::EventSink;

/// Per-workitem fan-out capacity, mirroring the event bus `CHANNEL_CAPACITY`.
const FANOUT_CAPACITY: usize = 128;

/// Maximum concurrent subscribers per workitem UID.
///
/// The workitem UID comes from the request path, so without a cap a client can
/// create an unbounded number of subscribers for distinct UIDs. Each entry
/// costs a `String` key, a `Vec`, and a `SyncSender` with its 128-event buffer.
/// 64 is far above any real workflow (one viewer per workitem, a handful of
/// operators); a request beyond it is refused at the SSE handler with 503.
pub const MAX_SUBSCRIBERS_PER_UID: usize = 64;

/// Why a subscription was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscribeError {
    /// The per-UID subscriber cap is reached.
    TooManySubscribers,
}

/// Fan-out registry: workitem UID -> list of bounded senders.
///
/// All access (subscribe, publish, disconnect-GC) happens under one mutex, so
/// there are no races and the sender list cannot grow unboundedly. Two
/// independent bounds keep the registry from being an amplifier, because the
/// workitem UID is attacker-chosen:
///
/// 1. at most [`MAX_SUBSCRIBERS_PER_UID`] senders per UID;
/// 2. a UID's map entry is removed as soon as its last sender is gone - on
///    publish AND on receiver drop (see [`Subscriber`]).
///
/// The second bound matters because the workitem UID is chosen by the client:
/// a client could subscribe once to a thousand distinct UIDs and never
/// disconnect cleanly. GC-on-publish alone only cleans a UID that someone
/// happens to publish to, so those UIDs would sit in the map forever. The
/// subscriber guard removes the entry the moment its receiver goes away, so
/// the registry tracks live subscribers rather than historical ones.
///
/// The registry lives in an `Arc` so the bus is cheaply cloneable (mirroring
/// `UpsEventBus`, which is also an `Arc`-backed `Clone`).
#[derive(Clone)]
pub struct MpscFanoutBus {
    inner: Arc<Mutex<HashMap<String, UidFanout>>>,
    /// Monotonic source of sender identities (see [`Slot`]).
    next_token: Arc<AtomicU64>,
}

/// One workitem's subscribers plus the event counter for that workitem.
///
/// The counter lives HERE, not in the bus, because an SSE `id:` is meaningful
/// only within one workitem's stream: a client watching workitem A must not
/// see its ids jump because workitem B was busy. It is per-UID and monotonic
/// for as long as the entry exists, so a gap tells a live subscriber that an
/// event was lost - which is the whole point of sending an id at all.
///
/// # Contract
///
/// - The counter increments on EVERY publish while at least one subscriber is
///   attached, including a publish whose send is dropped because a channel is
///   full. A dropped event must still advance the counter, otherwise the next
///   delivered id would be contiguous and the loss invisible.
/// - The entry - and with it the counter - is dropped when the last
///   subscriber goes, so a later subscriber starts from zero rather than
///   inheriting a number it never saw.
/// - Replay from `Last-Event-ID` is out of scope: catch-up after a reconnect
///   is a QIDO concern, not a fan-out concern.
#[derive(Default)]
struct UidFanout {
    slots: Vec<Slot>,
    next_event_id: u64,
}

/// Owns a subscriber's receiver and drops its registry entry on the way out.
///
/// The SSE handler already breaks its loop when the channel disconnects, but
/// that only happens on publish; a client that simply goes away (or a UID
/// nobody ever publishes to) leaves the entry behind. Wrapping the receiver
/// makes the cleanup unconditional: `Drop` runs whatever the exit path.
///
/// The removal is by IDENTITY, not by probing senders. `std::sync::mpsc` has
/// no "is this sender still connected?" query that does not also deliver a
/// message, and a probe would push a junk event into every live sibling's
/// stream. Instead each sender is paired with a unique id and the guard
/// removes exactly the one id it registered, then deletes the map entry only
/// if that left the list empty.
pub struct Subscriber {
    rx: Option<mpsc::Receiver<UpsEvent>>,
    bus: MpscFanoutBus,
    uid: String,
    /// Identity of the sender this guard owns within the uid's list.
    token: u64,
}

impl Subscriber {
    /// Block until the next event arrives (or the channel disconnects).
    pub fn recv(&self) -> Result<UpsEvent, std::sync::mpsc::RecvError> {
        match &self.rx {
            Some(rx) => rx.recv(),
            None => Err(std::sync::mpsc::RecvError),
        }
    }

    /// Block up to `timeout` for the next event.
    pub fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<UpsEvent, std::sync::mpsc::RecvTimeoutError> {
        match &self.rx {
            Some(rx) => rx.recv_timeout(timeout),
            // Unreachable in practice (the receiver is only cleared in Drop),
            // but reported as Disconnected rather than panicking.
            None => Err(std::sync::mpsc::RecvTimeoutError::Disconnected),
        }
    }
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        self.rx = None;
        let mut map = self.bus.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(fanout) = map.get_mut(&self.uid) else {
            return;
        };
        // Remove exactly our own sender (matched by token). A concurrent
        // publish may have already pruned it as disconnected; that is fine,
        // the retain simply matches nothing.
        fanout.slots.retain(|s| s.token != self.token);
        // The entry - and the event counter it carries - goes with the last
        // subscriber, so a later one starts a fresh stream at 0.
        if fanout.slots.is_empty() {
            map.remove(&self.uid);
        }
    }
}

/// One registered sender plus the identity its guard uses to remove it.
struct Slot {
    tx: SyncSender<UpsEvent>,
    token: u64,
}

impl MpscFanoutBus {
    /// Create an empty fan-out bus.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            next_token: Arc::new(AtomicU64::new(1)),
        }
    }

    /// How many workitem UIDs currently have at least one subscriber.
    ///
    /// Exposed for the orphan-GC tests and for diagnostics: this is the number
    /// that must fall back to zero when subscribers go away.
    pub fn tracked_uids(&self) -> usize {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// Number of live subscribers for one UID (0 when untracked).
    pub fn subscriber_count(&self, workitem_uid: &str) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(workitem_uid)
            .map(|fanout| fanout.slots.len())
            .unwrap_or(0)
    }

    /// Register a new subscriber for `workitem_uid`.
    ///
    /// Returns the guarded receiver plus the live-subscriber count AFTER this
    /// subscription, or [`SubscribeError::TooManySubscribers`] when the UID is
    /// already at [`MAX_SUBSCRIBERS_PER_UID`].
    ///
    /// The count is returned so the caller can log it without a second lock
    /// acquisition (and without racing a concurrent subscribe).
    pub fn try_subscribe(&self, workitem_uid: &str) -> Result<(Subscriber, usize), SubscribeError> {
        let (tx, rx) = mpsc::sync_channel(FANOUT_CAPACITY);
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let fanout = map.entry(workitem_uid.to_string()).or_default();
        if fanout.slots.len() >= MAX_SUBSCRIBERS_PER_UID {
            // Do not leave an empty entry behind for a UID we just refused -
            // otherwise a refused subscribe would itself grow the map.
            if fanout.slots.is_empty() {
                map.remove(workitem_uid);
            }
            return Err(SubscribeError::TooManySubscribers);
        }
        fanout.slots.push(Slot { tx, token });
        let count = fanout.slots.len();
        Ok((
            Subscriber {
                rx: Some(rx),
                bus: self.clone(),
                uid: workitem_uid.to_string(),
                token,
            },
            count,
        ))
    }

    /// Publish an event to every subscriber of its workitem.
    ///
    /// Non-blocking `try_send`:
    /// - `Ok` -> delivered.
    /// - `Full` -> the newest event is dropped and debug-logged (std::mpsc has
    ///   no drop-oldest; blocking the publisher is worse than dropping).
    /// - `Disconnected` -> the subscriber vanished; the sender is removed here,
    ///   under the same lock (no separate GC pass, no lock held across send).
    pub fn publish(&self, mut event: UpsEvent) {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(fanout) = map.get_mut(&event.workitem_uid) else {
            // Nobody subscribed to this workitem; nothing to fan out.
            return;
        };
        // Stamp BEFORE the retain: the id belongs to this publish whether or
        // not a full channel swallows the send, so a dropped event leaves a
        // gap a live subscriber can see.
        // Stamp BEFORE the retain: the id belongs to this publish whether or
        // not a full channel swallows the send, so a dropped event leaves a
        // gap a live subscriber can see.
        event.event_id = fanout.next_event_id;
        fanout.next_event_id = fanout.next_event_id.wrapping_add(1);
        fanout
            .slots
            .retain(|slot| match slot.tx.try_send(event.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    crate::clog_debug!(
                        workitem_uid = %event.workitem_uid,
                        "UPS fan-out channel full - dropped newest event"
                    );
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            });
        if fanout.slots.is_empty() {
            map.remove(&event.workitem_uid);
        }
    }
}

impl Default for MpscFanoutBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink for MpscFanoutBus {
    fn publish(&self, event: UpsEvent) {
        self.publish(event);
    }

    fn notify_created(&self, workitem_uid: &str, filter: FilterRouting) {
        let ts = timestamp_now();
        self.publish(UpsEvent {
            workitem_uid: workitem_uid.to_string(),
            previous_state: String::new(),
            new_state: "SCHEDULED".to_string(),
            timestamp: ts,
            event_id: 0,
            step_label: filter.step_label.clone(),
            scheduled_code: filter.scheduled_code.clone(),
            input_readiness: filter.input_readiness.clone().unwrap_or_default(),
            filter: Box::new(filter),
        });
    }

    fn notify_state_change(
        &self,
        workitem_uid: &str,
        previous: &str,
        new: &str,
        filter: FilterRouting,
    ) {
        if previous == new {
            return;
        }
        let ts = timestamp_now();
        self.publish(UpsEvent {
            workitem_uid: workitem_uid.to_string(),
            previous_state: previous.to_string(),
            new_state: new.to_string(),
            timestamp: ts,
            event_id: 0,
            step_label: filter.step_label.clone(),
            scheduled_code: filter.scheduled_code.clone(),
            input_readiness: filter.input_readiness.clone().unwrap_or_default(),
            filter: Box::new(filter),
        });
    }
}

/// Publish events to both the `UpsEventBus` and the `MpscFanoutBus`.
///
/// This is the concrete `EventSink` installed in `AppState`. It is cheaply
/// cloneable and `Send + Sync`, so it drops into the existing `AppState` field.
/// Both backing buses are synchronous `std::sync::mpsc` fan-outs.
#[derive(Clone)]
pub struct DualEventSink {
    /// `UpsEventBus` - kept for the `AppState` field and test wiring.
    pub broadcast: UpsEventBus,
    /// Synchronous fan-out - the sync dispatcher SSE path.
    pub fanout: MpscFanoutBus,
}

impl DualEventSink {
    /// Create an empty dual sink (both transports wired).
    pub fn new() -> Self {
        Self {
            broadcast: UpsEventBus::new(),
            fanout: MpscFanoutBus::new(),
        }
    }

    /// Event-bus subscription - returns a synchronous mpsc receiver.
    ///
    /// NOTE: nothing in production calls this. The only live SSE route
    /// (`GET /dicomweb/workitems/{uid}/events`) uses [`Self::fanout_try_subscribe`]
    /// below; this accessor exists for the unit tests that pin the broadcast
    /// bus. `UpsEventBus` is therefore TEST-ONLY wiring, and the orphan risk
    /// that matters in production lives in `MpscFanoutBus`.
    pub fn subscribe(&self, workitem_uid: &str) -> mpsc::Receiver<UpsEvent> {
        self.broadcast.subscribe(workitem_uid)
    }

    /// Fan-out subscription - used by the sync dispatcher SSE wrapper.
    ///
    /// Returns the guarded subscriber plus its live-subscriber count, or
    /// [`SubscribeError::TooManySubscribers`] when the UID is at the cap.
    pub fn fanout_try_subscribe(
        &self,
        workitem_uid: &str,
    ) -> Result<(Subscriber, usize), SubscribeError> {
        self.fanout.try_subscribe(workitem_uid)
    }
}

impl Default for DualEventSink {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink for DualEventSink {
    fn publish(&self, event: UpsEvent) {
        self.broadcast.publish(event.clone());
        // WebSocket Notification Connections (PS3.18 8.10.4, RAD-109) get the
        // SAME Event Report as the SSE stream: one publishing site, one
        // serialisation (`ups_event_json`), two transports. Serialising twice
        // would let the two drift, and a subscriber comparing them would be
        // right to complain.
        crate::dispatch_streams::publish_event_to_channels(&event);
        self.fanout.publish(event);
    }

    fn notify_created(&self, workitem_uid: &str, filter: FilterRouting) {
        let ts = timestamp_now();
        self.publish(UpsEvent {
            workitem_uid: workitem_uid.to_string(),
            previous_state: String::new(),
            new_state: "SCHEDULED".to_string(),
            timestamp: ts,
            event_id: 0,
            step_label: filter.step_label.clone(),
            scheduled_code: filter.scheduled_code.clone(),
            input_readiness: filter.input_readiness.clone().unwrap_or_default(),
            filter: Box::new(filter),
        });
    }

    fn notify_state_change(
        &self,
        workitem_uid: &str,
        previous: &str,
        new: &str,
        filter: FilterRouting,
    ) {
        if previous == new {
            return;
        }
        let ts = timestamp_now();
        self.publish(UpsEvent {
            workitem_uid: workitem_uid.to_string(),
            previous_state: previous.to_string(),
            new_state: new.to_string(),
            timestamp: ts,
            event_id: 0,
            step_label: filter.step_label.clone(),
            scheduled_code: filter.scheduled_code.clone(),
            input_readiness: filter.input_readiness.clone().unwrap_or_default(),
            filter: Box::new(filter),
        });
    }
}

/// Generate a UNIX-timestamp-seconds string (UTC).
fn timestamp_now() -> String {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

// -- Unit tests ----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::ups::event_bus::UpsEvent;

    fn ev(uid: &str, id: u64) -> UpsEvent {
        UpsEvent {
            workitem_uid: uid.to_string(),
            previous_state: "SCHEDULED".to_string(),
            new_state: "INPROGRESS".to_string(),
            timestamp: "0".to_string(),
            event_id: id,
            step_label: String::new(),
            scheduled_code: None,
            filter: Box::default(),
            input_readiness: "READY".to_string(),
        }
    }

    #[test]
    fn publish_n_receiver_gets_n() {
        let bus = MpscFanoutBus::new();
        let (sub, _) = bus.try_subscribe("1.2.3").expect("subscribe");
        for i in 0..5 {
            bus.publish(ev("1.2.3", i));
        }
        let mut got = Vec::new();
        while let Ok(e) = sub.recv_timeout(std::time::Duration::from_millis(50)) {
            got.push(e.event_id);
        }
        assert_eq!(got, vec![0, 1, 2, 3, 4]);
    }

    /// W30: the SSE `id:` is per-UID and monotonic.
    ///
    /// Before the stamp, every event carried `event_id: 0` because
    /// `notify_created`/`notify_state_change` hardcode the placeholder and
    /// `publish` sent it as-is. A client that renders `id: 0` on every frame
    /// cannot tell a dropped event from a delivered one, which is the only
    /// reason an id exists in an SSE stream.
    ///
    /// Two UIDs are published into INTERLEAVED on purpose: a GLOBAL counter
    /// would make each stream skip numbers whenever the other one was busy,
    /// which is the wrong granularity for a client watching one workitem.
    #[test]
    fn event_ids_are_monotonic_and_per_uid() {
        let bus = MpscFanoutBus::new();
        let (sub_a, _) = bus.try_subscribe("A").expect("subscribe A");
        let (sub_b, _) = bus.try_subscribe("B").expect("subscribe B");

        // Interleave: A, B, A, B, A.
        for round in 0..3 {
            bus.publish(ev("A", 0));
            if round < 2 {
                bus.publish(ev("B", 0));
            }
        }

        let drain = |sub: &Subscriber| {
            let mut ids = Vec::new();
            while let Ok(e) = sub.recv_timeout(std::time::Duration::from_millis(50)) {
                ids.push(e.event_id);
            }
            ids
        };
        assert_eq!(drain(&sub_a), vec![0, 1, 2], "A counts on its own");
        assert_eq!(drain(&sub_b), vec![0, 1], "B counts on its own");
    }

    /// W30: a DROPPED event still advances the counter.
    ///
    /// A full channel swallows the newest event, and an id that did not move
    /// would make the next delivered event contiguous with the last one - the
    /// stream would look complete while an event was missing. The gap is the
    /// signal.
    #[test]
    fn a_dropped_event_still_advances_the_counter() {
        let bus = MpscFanoutBus::new();
        let (sub, _) = bus.try_subscribe("1.2.3").expect("subscribe");

        // Fill the channel exactly, so the next publish hits `Full`.
        for _ in 0..FANOUT_CAPACITY {
            bus.publish(ev("1.2.3", 0));
        }
        // Dropped: the channel is full.
        bus.publish(ev("1.2.3", 0));
        // Delivered: consume one first so there is room again.
        let first = sub.recv_timeout(std::time::Duration::from_millis(50));
        assert!(first.is_ok(), "the channel had events to deliver");
        bus.publish(ev("1.2.3", 0));

        // Drain what arrived; the last id must be past the dropped one and
        // strictly greater than the count of DELIVERED events.
        let mut ids = Vec::new();
        while let Ok(e) = sub.recv_timeout(std::time::Duration::from_millis(50)) {
            ids.push(e.event_id);
        }
        let last = *ids.last().expect("at least one event was delivered");
        assert_eq!(
            last,
            (FANOUT_CAPACITY + 1) as u64,
            "the dropped event consumed an id, so the last delivered one skips it: {ids:?}"
        );
        // The gap is observable: 127 -> 129 rather than 127 -> 128.
        assert!(
            !ids.contains(&(FANOUT_CAPACITY as u64)),
            "the dropped id must never be delivered: {ids:?}"
        );
    }

    #[test]
    fn unsubscribe_yields_silence_and_gc() {
        let bus = MpscFanoutBus::new();
        {
            let (_sub, _) = bus.try_subscribe("1.2.3").expect("subscribe");
        }
        // Receiver dropped -> the drop guard removes the map entry right away,
        // without needing a publish. This is the property the old
        // GC-on-publish-only design did not provide.
        assert_eq!(bus.tracked_uids(), 0, "drop must remove the entry at once");
        // A publish to that UID still finds nothing and stays a no-op.
        bus.publish(ev("1.2.3", 1));
        assert_eq!(bus.tracked_uids(), 0);
    }

    #[test]
    fn full_channel_drops_newest() {
        let bus = MpscFanoutBus::new();
        let (sub, _) = bus.try_subscribe("1.2.3").expect("subscribe");
        // Fill the bounded channel (128) exactly.
        for i in 0..128 {
            bus.publish(ev("1.2.3", i));
        }
        // Next send would be Full -> dropped new (the 128th) but 127th stays.
        bus.publish(ev("1.2.3", 200));
        let mut got = Vec::new();
        while let Ok(e) = sub.recv_timeout(std::time::Duration::from_millis(50)) {
            got.push(e.event_id);
        }
        assert_eq!(got.len(), 128, "full channel received 128");
        assert_eq!(got[127], 127, "newest (200) dropped on full");
        assert!(!got.contains(&200));
    }

    // -- Task F: cap and orphan GC ----------------------------------------

    #[test]
    fn subscriber_cap_refuses_the_65th_subscription() {
        // 64 is the cap; going one past it must be refused rather than
        // quietly accepted, because each subscriber costs a channel buffer
        // and the UID is client-chosen.
        let bus = MpscFanoutBus::new();
        let mut subs = Vec::new();
        for i in 0..MAX_SUBSCRIBERS_PER_UID {
            let (sub, count) = bus.try_subscribe("1.2.3").expect("within cap");
            assert_eq!(count, i + 1);
            subs.push(sub);
        }
        assert_eq!(bus.subscriber_count("1.2.3"), MAX_SUBSCRIBERS_PER_UID);
        assert_eq!(
            bus.try_subscribe("1.2.3").err(),
            Some(SubscribeError::TooManySubscribers),
            "the 65th subscription must be refused"
        );
        // The refusal must not have grown the list.
        assert_eq!(bus.subscriber_count("1.2.3"), MAX_SUBSCRIBERS_PER_UID);

        // The cap is PER UID: a different UID is unaffected.
        let _other = bus.try_subscribe("9.9.9").expect("other uid is fine");

        // Freeing one slot admits a new subscriber again.
        subs.pop();
        assert!(bus.try_subscribe("1.2.3").is_ok());
    }

    #[test]
    fn refused_subscribe_does_not_create_a_map_entry() {
        // A UID that is at the cap is already present, so this pins the
        // subtler case: the entry must not be left behind by a refusal on a
        // UID that was never tracked. That path is unreachable while the cap
        // is non-zero (a uid at the cap is by definition present), so the
        // observable guarantee is simply that `tracked_uids` never grows
        // because of a refusal.
        let bus = MpscFanoutBus::new();
        let mut subs = Vec::new();
        for _ in 0..MAX_SUBSCRIBERS_PER_UID {
            subs.push(bus.try_subscribe("1.2.3").expect("within cap").0);
        }
        let before = bus.tracked_uids();
        assert!(bus.try_subscribe("1.2.3").is_err());
        assert_eq!(bus.tracked_uids(), before);
    }

    #[test]
    fn dropping_the_last_subscriber_removes_the_uid_entry() {
        let bus = MpscFanoutBus::new();
        // Distinct UIDs, one subscriber each: the orphaning shape the task
        // describes (a client subscribing once to many UIDs and going away).
        let subs: Vec<_> = (0..50)
            .map(|i| bus.try_subscribe(&format!("1.2.{i}")).expect("subscribe").0)
            .collect();
        assert_eq!(bus.tracked_uids(), 50);
        // Drop them all WITHOUT publishing to any of them. GC-on-publish
        // alone would leave all 50 entries behind forever.
        drop(subs);
        assert_eq!(
            bus.tracked_uids(),
            0,
            "every orphaned uid must be removed on drop"
        );
    }

    #[test]
    fn dropping_one_of_two_subscribers_keeps_the_survivor() {
        // Identity-based removal must not take out a live sibling.
        let bus = MpscFanoutBus::new();
        let (a, _) = bus.try_subscribe("1.2.3").expect("a");
        let (b, _) = bus.try_subscribe("1.2.3").expect("b");
        assert_eq!(bus.subscriber_count("1.2.3"), 2);
        drop(a);
        assert_eq!(bus.subscriber_count("1.2.3"), 1, "one remains");
        assert_eq!(bus.tracked_uids(), 1, "the uid is still tracked");
        // The survivor still receives events, and receives no junk from the
        // dropped sibling's cleanup.
        //
        // The id is STAMPED by publish now (W30), not passed through: a
        // subscriber of a live UID sees its own counter, so the first event it
        // receives is 0 whatever `event_id` the caller put in the event.
        bus.publish(ev("1.2.3", 7));
        let got = b.recv_timeout(std::time::Duration::from_millis(50));
        assert_eq!(got.map(|e| e.event_id), Ok(0));
        // Nothing else was queued by the drop path.
        assert!(
            b.recv_timeout(std::time::Duration::from_millis(10))
                .is_err()
        );
    }

    #[test]
    fn dual_sink_publishes_to_both() {
        let sink = DualEventSink::new();
        let (fan, _) = sink.fanout_try_subscribe("7.7").expect("fan subscribe");
        let bus = sink.subscribe("7.7");
        EventSink::publish(&sink, ev("7.7", 42));
        // The fan-out stamps its own per-UID id; the caller's placeholder
        // (42 here) is overwritten.
        assert_eq!(fan.recv().unwrap().event_id, 0);
        // Event-bus receiver: `send` inside publish sets event_id monotonic.
        let rx_check = bus.try_recv().map(|e| e.workitem_uid == "7.7");
        assert_eq!(rx_check, Ok(true));
    }
}
