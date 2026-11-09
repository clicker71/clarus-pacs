// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! A UPS event must be visible in the log as ONE `UPS_EVENT` line.
//!
//! Before this, the journal showed only the transport activity around an event
//! ("UPS SSE subscriber added", "UPS notification connection opened") and never
//! the event itself, so an operator asking "was a workitem created for this
//! study, and when did it change state?" had no line to read.
//!
//! This file is a single test on purpose: `clog::set_sender` installs a
//! process-wide writer channel once, so a second test here would race it.
//!
//! RELEASE ONLY (`#![cfg(not(debug_assertions))]` at the foot of the file): in
//! a debug build `clog::emit` writes the record straight to stderr and never
//! pushes it to the writer channel, so there is nothing here to read back. The
//! line SHAPE is pinned in every mode by the unit test in `event_bus.rs`.
//!
//! Run it with: cargo test --release -p clarus --test ups_event_log

#![cfg(not(debug_assertions))]

use clarus_server::handlers::ups::event_bus::UpsEventBus;
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// Collect everything the clog channel has received so far.
fn drain(rx: &Receiver<Vec<u8>>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(record) = rx.try_recv() {
        out.push(String::from_utf8_lossy(&record).into_owned());
    }
    out
}

fn event_lines(records: &[String]) -> Vec<String> {
    records
        .iter()
        .filter(|l| l.contains("UPS_EVENT"))
        .cloned()
        .collect()
}

#[test]
fn a_created_workitem_logs_one_ups_event_with_no_subscribers() {
    // The writer channel is a plain sync_channel: the server's clog writer
    // thread is NOT started in this harness, so every record lands directly in
    // the channel and is read back here.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4096);
    clarus_server::clog::set_sender(tx);

    let bus = UpsEventBus::new();

    // NO subscriber on purpose. A workitem is created with nobody listening in
    // the normal case (the performer subscribes afterwards), so if the line
    // were emitted after the subscriber lookup this event - the one an operator
    // most wants to see - would be the one that vanished.
    bus.notify_created("1.2.826.0.1.3680043.10.1234.9501", "", None);

    std::thread::sleep(Duration::from_millis(100));
    let lines = event_lines(&drain(&rx));

    assert_eq!(
        lines.len(),
        1,
        "a creation event must log exactly one UPS_EVENT line, got {lines:#?}"
    );
    let line = &lines[0];
    assert!(
        line.contains("workitem_uid=1.2.826.0.1.3680043.10.1234.9501"),
        "the line must name the workitem: {line}"
    );
    assert!(
        line.contains("previous_state=created"),
        "an empty previous_state must read as `created`: {line}"
    );
    assert!(
        line.contains("new_state=SCHEDULED"),
        "a creation event's new state is SCHEDULED: {line}"
    );
    assert!(
        line.contains("event_id="),
        "the line must carry the event id (it matches the SSE `id:` field): {line}"
    );

    // A state change logs its own line, with the real transition.
    bus.notify_state_change(
        "1.2.826.0.1.3680043.10.1234.9501",
        "SCHEDULED",
        "IN PROGRESS",
        "",
        None,
    );
    std::thread::sleep(Duration::from_millis(100));
    let lines = event_lines(&drain(&rx));
    assert_eq!(lines.len(), 1, "one transition -> one line: {lines:#?}");
    assert!(
        lines[0].contains("previous_state=SCHEDULED") && lines[0].contains("new_state=IN PROGRESS"),
        "the line must carry the real transition: {}",
        lines[0]
    );

    // A no-op transition (previous == new) is muted at the source and must
    // produce NO line: the journal says what happened, and nothing happened.
    bus.notify_state_change(
        "1.2.826.0.1.3680043.10.1234.9501",
        "COMPLETED",
        "COMPLETED",
        "",
        None,
    );
    std::thread::sleep(Duration::from_millis(100));
    let lines = event_lines(&drain(&rx));
    assert!(
        lines.is_empty(),
        "a no-op transition must not log: {lines:#?}"
    );
}
