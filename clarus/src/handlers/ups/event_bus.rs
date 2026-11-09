// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! UPS event bus for SSE subscriptions.
//!
//! Provides per-workitem fan-out channels: UPS handlers publish state change
//! events, SSE endpoints subscribe clients to them.
//!
//! # Design
//! - Synchronous `std::sync::mpsc::sync_channel` - multiple producers, multiple
//!   consumers per workitem; no tokio runtime required.
//! - Per-workitem channels (not a global bus) - subscribers receive only
//!   events for their workitem, no client-side filtering needed.
//! - Channel capacity 128 - buffers events during brief subscriber reconnection.
//! - Lazy channel creation: channel is created on first `subscribe()`.
//!
//! # Overflow semantics
//! `std::sync::mpsc::sync_channel` cannot drop the oldest event; on a full
//! channel the newest event is dropped and debug-logged (nginx-way - never
//! block the publisher). When a receiver is dropped the associated sender is
//! garbage-collected under the same lock that publishes.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use super::state_machine::is_final_state;

const CHANNEL_CAPACITY: usize = 128;

/// An event emitted when a workitem's workflow state changes.
///
/// Serialised as JSON in the SSE `data:` field (native `ups_event_json`).
///
/// # Filter keys are not payload
///
/// `step_label` and `scheduled_code` exist for ONE purpose: letting the
/// WebSocket path answer the same subscription filter the POST push answers.
/// They are deliberately NOT rendered by `ups_event_json` - the Event Report
/// on the wire carries only the Data Set attributes PS3.18 8.10.5.1.1 and
/// PS3.4 CC.2.4.3 require. An event carries what a transport may need to ROUTE
/// it, and only what the subscriber asked to receive travels.
#[derive(Debug, Clone)]
pub struct UpsEvent {
    /// DICOM workitem UID (e.g. `1.2.840.10008.5.1.4.34.6.1.20200101.1`)
    pub workitem_uid: String,
    /// Previous workflow state (empty string for creation events).
    pub previous_state: String,
    /// New workflow state (one of `VALID_STATES`).
    pub new_state: String,
    /// UNIX timestamp in seconds (UTC).
    pub timestamp: String,
    /// Monotonic event ID for `Last-Event-ID` support (SSE `id:` field).
    pub event_id: u64,
    /// (0074,1204) Procedure Step Label of the workitem, `""` when it has
    /// none. A subscription filtered on the label matches against this.
    pub step_label: String,
    /// (0040,4018) Scheduled Workitem Code as
    /// `(Code Value, Coding Scheme Designator)`, `None` when the workitem has
    /// no scheduled code.
    ///
    /// The two travel together because they are two attributes of ONE
    /// sequence item (PS3.4 C.2.2.2.6 Sequence Matching): an item that matched
    /// the value but not the scheme is not a match.
    pub scheduled_code: Option<(String, String)>,
    /// (0040,4041) Input Readiness State of the workitem at the moment of the
    /// event, `""` when the record carries none.
    ///
    /// Rendered into the Event Report because PS3.4 CC.2.4.3 requires the SCP
    /// to give the CURRENT value of Input Readiness State - it is not a
    /// constant, and a hardcoded "READY" would report a readiness the workitem
    /// does not have.
    pub input_readiness: String,
    /// The workitem's values for the remaining subscription filter keys.
    ///
    /// ROUTING data, not payload: PS3.18 11.10 decides which subscriptions
    /// receive an event, and the channel asks that question per requester, so
    /// the answer must travel with the event. None of it is rendered by
    /// `ups_event_json` - the Event Report carries only what 8.10.5.1.1 and
    /// PS3.4 CC.2.4.3 require, exactly as `step_label` and `scheduled_code`
    /// above.
    ///
    /// Boxed so the common case of an event with nothing to route does not
    /// grow the struct, which is cloned once per subscriber on the fan-out.
    pub filter: Box<FilterRouting>,
}

/// The record-backed values a subscription filter is matched against.
///
/// One struct so a new filter key does not change the signature of every
/// notification call site, and so the default (all-`None`) is a single value a
/// test can name instead of twelve absent arguments.
#[derive(Debug, Clone, Default)]
pub struct FilterRouting {
    /// (0074,1204) Procedure Step Label, empty when the workitem has none.
    pub step_label: String,
    /// (0040,4018) Code Value and Coding Scheme Designator, when the workitem
    /// carries a scheduled code.
    pub scheduled_code: Option<(String, String)>,
    /// (0020,000D) Study Instance UID of the input study.
    pub study_uid: Option<String>,
    /// (0010,0020) Patient ID.
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
    /// (0040,0001) Scheduled Station AE Title.
    pub station_aet: Option<String>,
}

impl FilterRouting {
    /// The routing values of a stored record.
    ///
    /// One converter so a new filter key is added in one place rather than at
    /// every notification call site, where missing one would silently narrow
    /// nothing and be invisible to a test that happened to use a matching
    /// workitem.
    #[must_use]
    pub fn from_record(record: &clarus_core::ups::WorkItemRecord) -> Self {
        Self {
            step_label: record.step_label.clone().unwrap_or_default(),
            scheduled_code: record
                .scheduled_workitem_code
                .as_ref()
                .map(|code| (code.value.clone(), code.scheme.clone())),
            study_uid: record.input_study_uid.clone(),
            patient_id: record.patient_id.clone(),
            workflow_state: Some(record.workflow_state.clone()),
            input_readiness: record.input_readiness.clone(),
            sps_priority: record.sps_priority.clone(),
            sps_start_datetime: record.sps_start_datetime.clone(),
            patient_name: record.patient_name.clone(),
            patient_id_issuer: record.patient_id_issuer.clone(),
            accession_number: record.accession_number.clone(),
            requested_procedure_id: record.requested_procedure_id.clone(),
            station_aet: record.station_aet.clone(),
        }
    }
}

/// Registry of per-workitem fan-out channels.
///
/// Thread-safe: all methods take `&self` (internal locking).
/// Cheaply cloneable - shares the same backing `Arc`.
#[derive(Clone)]
pub struct UpsEventBus {
    senders: Arc<Mutex<HashMap<String, Vec<SyncSender<UpsEvent>>>>>,
    counter: Arc<AtomicU64>,
}

impl UpsEventBus {
    /// Create a new empty event bus.
    pub fn new() -> Self {
        Self {
            senders: Arc::new(Mutex::new(HashMap::new())),
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Send a state change event to all subscribers of the workitem.
    ///
    /// If no subscribers exist for this workitem the event is dropped.
    /// Non-blocking `try_send`:
    /// - `Ok` -> delivered.
    /// - `Full` -> the newest event is dropped and debug-logged (std::mpsc
    ///   has no drop-oldest; blocking the publisher is worse than dropping).
    /// - `Disconnected` -> the subscriber vanished; its sender is removed
    ///   here, under the same lock (no separate GC pass).
    pub fn send(&self, mut event: UpsEvent) {
        // Assign a monotonic event ID.
        event.event_id = self.counter.fetch_add(1, Ordering::SeqCst);
        // Format timestamp if not already set.
        if event.timestamp.is_empty() {
            event.timestamp = timestamp_now();
        }

        // One journal line per event, emitted HERE and not in a transport: this
        // is the single place an event is raised, so a line here describes every
        // event exactly once, whatever carries it (SSE, WebSocket, or nothing).
        //
        // It is written BEFORE the subscriber lookup on purpose. A workitem
        // created with nobody yet subscribed is the common case - the performer
        // subscribes after the workitem exists - so logging only the delivered
        // events would hide precisely the ones an operator asks about
        // ("was the workitem ever created?"). The event happened; the absence of
        // a listener is not part of the event.
        //
        // `previous_state` is empty for a creation event (`notify_created`),
        // so it reads `created` rather than an empty field.
        let previous = if event.previous_state.is_empty() {
            "created"
        } else {
            event.previous_state.as_str()
        };
        crate::clog_event!(
            crate::clog::Level::Info,
            "UPS_EVENT",
            "",
            "workitem_uid={} previous_state={} new_state={} event_id={}",
            event.workitem_uid,
            previous,
            event.new_state,
            event.event_id
        );

        let mut senders = self.senders.lock().unwrap_or_else(|p| p.into_inner());
        let Some(list) = senders.get_mut(&event.workitem_uid) else {
            // Nobody subscribed to this workitem; nothing to fan out.
            return;
        };
        list.retain(|tx| match tx.try_send(event.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                crate::clog_debug!(
                    workitem_uid = %event.workitem_uid,
                    "UPS event channel full - dropped newest event"
                );
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        });
        if list.is_empty() {
            senders.remove(&event.workitem_uid);
        }

        // A workitem in a FINAL state will not transition again, so no further
        // event can ever reach these channels - but the senders keep them alive
        // until the process exits unless they are dropped here. A long-running
        // server accumulates one channel per completed workitem, and the map
        // walks them on every event.
        if is_final_state(&event.new_state) {
            senders.remove(&event.workitem_uid);
        }
    }

    /// Subscribe to events for a workitem.
    ///
    /// Creates the fan-out channel if it doesn't exist. The receiver (and
    /// only it) may block on `recv_timeout`; the publisher never blocks.
    /// Unsubscribe happens implicitly: when the receiver is dropped the
    /// sender's send fails with `Disconnected` and the bus garbage-collects
    /// it on the next event.
    pub fn subscribe(&self, workitem_uid: &str) -> mpsc::Receiver<UpsEvent> {
        let (tx, rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let mut senders = self.senders.lock().unwrap_or_else(|p| p.into_inner());
        senders
            .entry(workitem_uid.to_string())
            .or_default()
            .push(tx);
        rx
    }

    /// Send a notification that a workitem was created.
    ///
    /// Convenience wrapper: sets `new_state` to `"SCHEDULED"`
    /// with an empty `previous_state`.
    ///
    /// `step_label` and `code` are the workitem's filter keys, carried so the
    /// WebSocket path can answer a filtered subscription. The caller has both
    /// in hand (it just wrote the record), so no second store read is needed.
    pub fn notify_created(&self, workitem_uid: &str, filter: FilterRouting) {
        self.send(UpsEvent {
            workitem_uid: workitem_uid.to_string(),
            previous_state: String::new(),
            new_state: "SCHEDULED".to_string(),
            timestamp: timestamp_now(),
            event_id: 0, // filled by send()
            step_label: filter.step_label.clone(),
            scheduled_code: filter.scheduled_code.clone(),
            input_readiness: filter.input_readiness.clone().unwrap_or_default(),
            filter: Box::new(filter),
        });
    }

    /// Send a notification that a workitem state changed.
    ///
    /// No-op if `previous_state == new_state` (avoids duplicate events
    /// on non-state-changing updates).
    pub fn notify_state_change(
        &self,
        workitem_uid: &str,
        previous_state: &str,
        new_state: &str,
        filter: FilterRouting,
    ) {
        // Only send if state actually changed.
        if previous_state != new_state {
            self.send(UpsEvent {
                workitem_uid: workitem_uid.to_string(),
                previous_state: previous_state.to_string(),
                new_state: new_state.to_string(),
                timestamp: timestamp_now(),
                event_id: 0, // filled by send()
                step_label: filter.step_label.clone(),
                scheduled_code: filter.scheduled_code.clone(),
                input_readiness: filter.input_readiness.clone().unwrap_or_default(),
                filter: Box::new(filter),
            });
        }
    }
}

impl Default for UpsEventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Generate a UNIX-timestamp-seconds string (UTC).
///
/// Uses `std::time::SystemTime` - no external crate needed.
/// In a future release this may be replaced with ISO 8601 formatting
/// via the `time` crate for full DICOM compliance.
fn timestamp_now() -> String {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}
