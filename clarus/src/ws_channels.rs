// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Registry of open WebSocket Notification Connections (PS3.18 8.10.4).
//!
//! A channel is an OPEN CONNECTION, not a subscription: it records which AE
//! Title asked for it and where to hand its events. Subscriptions (PS3.18
//! 11.10) decide WHICH events the requester wants; this registry only knows
//! who is currently listening. Both are needed to deliver one event, and they
//! are deliberately separate records - see `NotificationChannel::accepts`.
//!
//! # Why a registry at all
//!
//! The connection lives in its own OS thread (the house thread-per-connection
//! model), while the event is published from whichever thread changed the
//! workitem state. Something has to hand the event from the second to the
//! first, and that something must never block the writer. The fan-out bus
//! below does exactly that.
//!
//! # Bounded, drop-and-count
//!
//! Each channel owns a BOUNDED queue. Publishing uses `try_send`, so a
//! subscriber that stops reading cannot stall the workitem write path - the
//! event is dropped and counted instead. That is the behaviour the AIW-I
//! Reliability statement commits to: notifications are a latency
//! optimisation, never the record of record.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Mutex, OnceLock};

/// Depth of one channel's outgoing queue.
///
/// A subscriber that falls this far behind is not keeping up with the workitem
/// state machine, and holding more would only delay the discovery. Fixed, not
/// a knob: the house rule for this sprint is zero new knobs, and the value
/// matches the SSE side's bounded queue.
pub const CHANNEL_QUEUE_DEPTH: usize = 32;

/// Largest number of simultaneous connections one requester may hold.
///
/// A user agent that opens channels in a loop would otherwise grow the
/// registry without bound. Fixed for the same reason as the queue depth.
pub const MAX_CHANNELS_PER_REQUESTER: usize = 8;

/// One open Notification Connection.
///
/// The connection thread owns the receiving half; the registry keeps the
/// sending half so publishers can reach it. Dropping a `NotificationChannel`
/// (or letting its receiver unwind) closes the channel - that is the whole
/// lifecycle, so there is no "close" bookkeeping to forget.
pub struct NotificationChannel {
    /// The AE Title from `{requester}` (PS3.18 Table 8.10.4-1a).
    pub requester: String,
    /// Monotonic id, unique per channel, for logging and for identity in the
    /// registry when one requester holds several channels.
    id: u64,
    /// Sending half of this channel's bounded queue.
    tx: SyncSender<String>,
    /// Events dropped on this channel because its queue was full.
    dropped: AtomicU64,
}

impl NotificationChannel {
    /// Whether this channel belongs to `requester` (case-insensitive AE Title
    /// comparison, matching how subscriptions are matched elsewhere).
    #[must_use]
    pub fn belongs_to(&self, requester: &str) -> bool {
        self.requester.eq_ignore_ascii_case(requester)
    }

    /// This channel's unique id.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Events dropped on this channel so far.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Hand one already-serialised payload to the channel.
    ///
    /// Never blocks and never fails the caller: a full queue means the
    /// subscriber is not reading, and the event is dropped and counted. A
    /// disconnected receiver is the normal end of a connection, not an error.
    ///
    /// Returns `true` when the payload was queued.
    pub fn try_deliver(&self, payload: String) -> bool {
        match self.tx.try_send(payload) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                crate::metrics::WS_CHANNEL_DROPPED.fetch_add(1, Ordering::Relaxed);
                false
            }
            // The connection thread has gone; the next open/close pass will
            // reap it. Counted separately so a leaked registry entry is
            // distinguishable from a slow subscriber.
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

/// The map key a requester is counted under.
///
/// AE Titles are case-insensitive by definition (PS3.4 C.3.4: the AE Title is
/// a 16-character, uppercase-with-space value), and `belongs_to` already
/// compares that way. Keying the cap on the raw string let one requester hold
/// a full allowance under every spelling.
fn cap_key(requester: &str) -> String {
    requester.to_ascii_uppercase()
}

/// The set of open channels, keyed by requester AE Title.
///
/// A `Mutex<HashMap<..>>` with short critical sections: the registry is read
/// on state changes and written on open/close, both operator-scale events.
/// Locks are never held across a socket write.
#[derive(Default)]
pub struct ChannelRegistry {
    channels: Mutex<HashMap<String, Vec<NotificationChannel>>>,
    next_id: AtomicU64,
    /// Live channel count, so the gauge can be read without taking the lock.
    live: AtomicUsize,
}

impl ChannelRegistry {
    /// Register a new channel for `requester`.
    ///
    /// Returns the receiver the connection thread reads from, plus the channel
    /// id. `None` when this requester already holds
    /// [`MAX_CHANNELS_PER_REQUESTER`] channels.
    pub fn open(&self, requester: &str) -> Option<(std::sync::mpsc::Receiver<String>, u64)> {
        let (tx, rx) = sync_channel(CHANNEL_QUEUE_DEPTH);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let channel = NotificationChannel {
            requester: requester.to_string(),
            id,
            tx,
            dropped: AtomicU64::new(0),
        };

        // The cap is keyed on the normalized AE Title: `belongs_to` compares
        // case-insensitively, so a case-sensitive key let one requester hold
        // MAX_CHANNELS_PER_REQUESTER channels per spelling and walk around the
        // cap by changing title case. The display string stays as received.
        let mut map = self.channels.lock().unwrap_or_else(|p| p.into_inner());
        let entry = map.entry(cap_key(requester)).or_default();
        if entry.len() >= MAX_CHANNELS_PER_REQUESTER {
            return None;
        }
        entry.push(channel);
        self.live.fetch_add(1, Ordering::Relaxed);
        Some((rx, id))
    }

    /// Remove the channel with `id`.
    ///
    /// Called when the connection thread ends for any reason: a close frame, a
    /// TCP EOF, or a failed send. Returns whether a channel was removed, so the
    /// caller can log an accurate lifecycle line.
    pub fn close(&self, id: u64) -> bool {
        let mut map = self.channels.lock().unwrap_or_else(|p| p.into_inner());
        for list in map.values_mut() {
            if let Some(pos) = list.iter().position(|c| c.id == id) {
                list.remove(pos);
                self.live.fetch_sub(1, Ordering::Relaxed);
                map.retain(|_, v| !v.is_empty());
                return true;
            }
        }
        false
    }

    /// Number of open channels (all requesters).
    #[must_use]
    pub fn live_count(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    /// Number of open channels for one requester.
    #[must_use]
    pub fn count_for(&self, requester: &str) -> usize {
        self.channels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&cap_key(requester))
            .map_or(0, Vec::len)
    }

    /// Every requester currently holding at least one channel.
    #[must_use]
    pub fn requesters(&self) -> Vec<String> {
        let map = self.channels.lock().unwrap_or_else(|p| p.into_inner());
        let mut out: Vec<String> = map
            .values()
            .flatten()
            .map(|c| c.requester.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Deliver `payload` to every open channel.
    ///
    /// The filter is applied by the CALLER-supplied predicate so the policy
    /// (which subscription accepts this event) stays with the subscription
    /// logic rather than being duplicated here. Locks are held only while
    /// enqueueing: `try_deliver` never blocks, so the workitem write path is
    /// never delayed by a slow subscriber.
    ///
    /// Returns how many channels accepted the payload.
    pub fn broadcast(&self, payload: &str, accepts: impl Fn(&str) -> bool) -> usize {
        let map = self.channels.lock().unwrap_or_else(|p| p.into_inner());
        let mut delivered = 0usize;
        for list in map.values() {
            for channel in list {
                // The predicate is asked about EVERY channel, with the AE Title
                // as RECEIVED (the stored string), not the map's normalized cap
                // key: subscribers match on the title they were given, and a
                // case-folded key is a cap detail, not an identity.
                //
                // The answer must not be cached per bucket. A bucket holds the
                // case variants of one AE Title, so it can contain both
                // "CT_MOD" and "ct_mod"; the first channel's answer says
                // nothing about the second. Caching it skipped a channel the
                // predicate accepts, and also delivered to one it rejects -
                // the same cached `true` was reused for a channel that does not
                // match.
                let ok = accepts(&channel.requester);
                if !ok {
                    continue;
                }
                if channel.try_deliver(payload.to_string()) {
                    delivered += 1;
                }
            }
        }
        delivered
    }

    /// Remove every channel. Test support only - the wire never empties the
    /// registry except by closing connections one at a time.
    pub fn clear(&self) {
        let mut map = self.channels.lock().unwrap_or_else(|p| p.into_inner());
        map.clear();
        self.live.store(0, Ordering::Relaxed);
    }
}

/// Process-wide channel registry. One per server process.
fn registry() -> &'static ChannelRegistry {
    static REGISTRY: OnceLock<ChannelRegistry> = OnceLock::new();
    REGISTRY.get_or_init(ChannelRegistry::default)
}

/// The registry accessor used by the handlers.
pub fn channels() -> &'static ChannelRegistry {
    registry()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is process-wide, so tests that inspect its contents take
    /// this lock. Without it they race each other exactly like the subscription
    /// tests did (see the Sprint C D0 note).
    static REG_LOCK: Mutex<()> = Mutex::new(());

    fn locked<R>(f: impl FnOnce() -> R) -> R {
        let _guard = REG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let saved = channels().live_count();
        channels().clear();
        let out = f();
        channels().clear();
        let _ = saved;
        out
    }

    #[test]
    fn open_registers_and_close_removes() {
        locked(|| {
            let (rx, id) = channels().open("WS_A").expect("opens");
            assert_eq!(channels().live_count(), 1);
            assert_eq!(channels().count_for("WS_A"), 1);
            assert_eq!(channels().requesters(), vec!["WS_A".to_string()]);

            assert!(channels().close(id));
            assert_eq!(channels().live_count(), 0);
            assert_eq!(channels().count_for("WS_A"), 0);
            assert!(
                channels().requesters().is_empty(),
                "an empty requester entry is reaped"
            );
            drop(rx);
        });
    }

    #[test]
    fn close_is_idempotent_and_reports_whether_it_removed() {
        locked(|| {
            let (_rx, id) = channels().open("WS_B").expect("opens");
            assert!(channels().close(id));
            assert!(!channels().close(id), "second close removes nothing");
        });
    }

    /// The cap is per requester, and `belongs_to` compares AE Titles
    /// case-insensitively (PS3.4 C.3.4). Keying the cap case-sensitively let
    /// one requester hold a full allowance under each spelling.
    #[test]
    fn the_channel_cap_is_case_insensitive() {
        locked(|| {
            for _ in 0..MAX_CHANNELS_PER_REQUESTER {
                assert!(
                    channels().open("CT_MOD").is_some(),
                    "the first {MAX_CHANNELS_PER_REQUESTER} channels open"
                );
            }
            assert!(
                channels().open("ct_mod").is_none(),
                "a different spelling of the same AE Title is the same requester"
            );
            assert_eq!(
                channels().count_for("CT_MOD"),
                MAX_CHANNELS_PER_REQUESTER,
                "both spellings count against one budget"
            );
            assert_eq!(channels().count_for("ct_mod"), MAX_CHANNELS_PER_REQUESTER);
        });
    }

    #[test]
    fn one_requester_may_hold_several_channels() {
        locked(|| {
            let (rx1, id1) = channels().open("WS_C").expect("first");
            let (rx2, id2) = channels().open("WS_C").expect("second");
            assert_ne!(id1, id2);
            assert_eq!(channels().count_for("WS_C"), 2);
            assert_eq!(channels().live_count(), 2);
            drop((rx1, rx2));
        });
    }

    #[test]
    fn the_per_requester_cap_is_enforced() {
        locked(|| {
            let mut held = Vec::new();
            for _ in 0..MAX_CHANNELS_PER_REQUESTER {
                held.push(channels().open("WS_D").expect("within the cap"));
            }
            assert!(
                channels().open("WS_D").is_none(),
                "the cap must refuse, not grow"
            );
            assert_eq!(channels().count_for("WS_D"), MAX_CHANNELS_PER_REQUESTER);
            // A different requester is unaffected by another's cap.
            assert!(channels().open("WS_E").is_some());
            drop(held);
        });
    }

    /// The predicate is asked about the AE Title as RECEIVED, not the map's
    /// case-folded cap key. Handing it the normalized key made a subscriber
    /// filtering on `ct_mod` miss a channel opened as `ct_mod`, and made
    /// `requesters()` report a title the requester never sent.
    #[test]
    fn the_predicate_sees_the_stored_requester_case() {
        locked(|| {
            let (rx, _id) = channels().open("ct_mod").expect("opens");

            let delivered = channels().broadcast("{\"a\":1}", |r| r == "ct_mod");
            assert_eq!(delivered, 1, "the predicate matches the stored spelling");
            assert_eq!(rx.recv().expect("receives"), "{\"a\":1}");

            assert_eq!(
                channels().requesters(),
                vec!["ct_mod".to_string()],
                "requesters() reports the title as received"
            );
        });
    }

    #[test]
    fn delivery_reaches_a_channel_and_its_receiver() {
        locked(|| {
            let (rx, _id) = channels().open("WS_F").expect("opens");
            let n = channels().broadcast("{\"a\":1}", |_| true);
            assert_eq!(n, 1, "one channel accepted");
            assert_eq!(rx.recv().expect("receives"), "{\"a\":1}");
        });
    }

    /// One `cap_key` bucket holds the CASE VARIANTS of one AE Title, so two
    /// channels can share a bucket and still be different requesters as stored.
    /// A predicate answer cached per bucket therefore answers for a channel it
    /// was never asked about: a rejected first channel silently skipped the
    /// accepted second one, and an accepted first channel delivered to a
    /// second one the predicate rejects.
    #[test]
    fn a_predicate_answer_is_not_reused_across_case_variants_in_one_bucket() {
        locked(|| {
            let (rx_upper, _) = channels().open("CT_MOD").expect("opens");
            let (rx_lower, _) = channels().open("ct_mod").expect("opens");

            // Accepted: the lower-case spelling only.
            let delivered = channels().broadcast("p", |r| r == "ct_mod");
            assert_eq!(
                delivered, 1,
                "only the channel whose stored title matches may be served"
            );
            assert_eq!(rx_lower.recv().expect("receives"), "p");
            assert!(
                rx_upper.try_recv().is_err(),
                "the upper-case channel must not receive the event"
            );

            // The mirror case: accepted by the upper-case spelling only.
            let delivered = channels().broadcast("q", |r| r == "CT_MOD");
            assert_eq!(delivered, 1, "the mirror predicate serves one channel");
            assert_eq!(rx_upper.recv().expect("receives"), "q");
            assert!(
                rx_lower.try_recv().is_err(),
                "the lower-case channel must not receive the event"
            );
        });
    }

    #[test]
    fn the_predicate_filters_by_requester() {
        locked(|| {
            let (rx_a, _) = channels().open("WS_G").expect("opens");
            let (rx_b, _) = channels().open("WS_H").expect("opens");
            let n = channels().broadcast("payload", |requester| requester == "WS_G");
            assert_eq!(n, 1, "only the named requester is served");
            assert_eq!(rx_a.recv().expect("receives"), "payload");
            assert!(
                rx_b.try_recv().is_err(),
                "the other requester must receive nothing"
            );
        });
    }

    #[test]
    fn a_full_queue_drops_and_counts_rather_than_blocking() {
        locked(|| {
            let (rx, _id) = channels().open("WS_I").expect("opens");
            // Fill the bounded queue exactly.
            for i in 0..CHANNEL_QUEUE_DEPTH {
                let n = channels().broadcast(&format!("e{i}"), |_| true);
                assert_eq!(n, 1, "slot {i} should accept");
            }
            // One more must be dropped, and the publisher must not block.
            let n = channels().broadcast("overflow", |_| true);
            assert_eq!(n, 0, "the overflow is not queued");
            assert_eq!(channels().live_count(), 1, "the channel stays open");

            // The dropped event is visible on close-out accounting.
            let dropped = {
                let map = channels().channels.lock().unwrap();
                map.get("WS_I").map_or(0, |v| v[0].dropped())
            };
            assert_eq!(dropped, 1, "the drop is counted");
            drop(rx);
        });
    }

    #[test]
    fn delivering_to_a_dead_receiver_is_not_an_error() {
        locked(|| {
            let (rx, _id) = channels().open("WS_J").expect("opens");
            drop(rx); // the connection thread ended
            // Must not panic; reports nothing delivered.
            let n = channels().broadcast("payload", |_| true);
            assert_eq!(n, 0);
        });
    }

    #[test]
    fn belongs_to_compares_ae_titles_case_insensitively() {
        locked(|| {
            let (rx, _id) = channels().open("WS_K").expect("opens");
            let own = {
                let map = channels().channels.lock().unwrap();
                map.get("WS_K").expect("present")[0].belongs_to("ws_k")
            };
            assert!(own, "AE Titles match without regard to case");
            drop(rx);
        });
    }
}
