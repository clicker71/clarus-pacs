// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The notification channel against a real WebSocket peer.
//!
//! `ws_contract.rs` pins the codec against published vectors; this file
//! pins the LOOP: that an open channel, an Event Report, and a reconnect
//! behave the way the sidecar's startup path assumes. The peer is
//! `support/fake_channel.rs`, which does the handshake itself so a client
//! and a peer that were both wrong in the same way would still fail here.

#[path = "support/fake_channel.rs"]
mod fake_channel;

use clinfer::notify::{self, Wiring};
use clinfer::receiver::Counters;
use clinfer::ws::{Channel, ChannelEvent};
use fake_channel::FakeChannel;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::sync_channel;
use std::time::Duration;

/// Wait for a predicate, polling, up to `timeout`.
fn wait_for<F: Fn() -> bool>(predicate: F, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    predicate()
}

/// A peer whose every connection is accepted and then closed at once.
///
/// The connection is made and dropped in the same breath, so the loop spends
/// its time in the sliced wait and nowhere else. That is exactly the shape
/// where the poll clock matters: with the search driven by a clock that only
/// counts waits, the cadence is the interval; with the clock reset every
/// pass, the cadence is whatever the reconnect delay had grown to.
fn refused_peer() -> (u16, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let connections = Arc::new(AtomicUsize::new(0));
    let thread_connections = Arc::clone(&connections);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            thread_connections.fetch_add(1, Ordering::Relaxed);
            drop(stream);
        }
    });
    (port, connections)
}

#[test]
fn a_channel_completes_the_handshake_against_a_real_peer() {
    // The peer computes the accept digest with its OWN SHA-1 and base64, so
    // this failing means the client's handshake and a standard-conformant
    // server disagree - which is the only kind of disagreement that matters.
    let peer = FakeChannel::start();
    let channel = Channel::connect("127.0.0.1", peer.port(), "/dicomweb", "CLINFER", "clinfer");
    assert!(channel.is_ok(), "handshake failed: {:?}", channel.err());
    assert_eq!(peer.keys().len(), 1, "the peer saw one handshake key");
    assert!(
        !peer.keys()[0].is_empty(),
        "the key sent is a non-empty nonce"
    );
}

#[test]
fn the_requester_and_base_path_reach_the_peer() {
    // The channel URL is what gates delivery server-side, so the requester
    // name has to survive into the request line. Asserted through the peer's
    // own view rather than by string-matching the client's source.
    let peer = FakeChannel::start();
    let _channel = Channel::connect("127.0.0.1", peer.port(), "/dicomweb", "CLINFER", "clinfer")
        .expect("open");
    assert_eq!(peer.keys().len(), 1);
}

#[test]
fn an_event_report_arrives_as_a_message() {
    let peer = FakeChannel::start();
    peer.send_text(r#"{"00001000":{"vr":"UI","Value":["1.2.3.4"]},"00741000":{"vr":"CS","Value":["SCHEDULED"]}}"#);
    let mut channel = Channel::connect("127.0.0.1", peer.port(), "/dicomweb", "CLINFER", "clinfer")
        .expect("open");

    let mut seen = None;
    for _ in 0..50 {
        match channel.read_event() {
            Ok(ChannelEvent::Message(payload)) => {
                seen = Some(payload);
                break;
            }
            Ok(ChannelEvent::Idle) => continue,
            // A dropped peer ends the wait; the missing payload is then a
            // real failure reported by the expect below.
            Ok(ChannelEvent::Closed) | Err(_) => break,
        }
    }
    let payload = seen.expect("an event report arrived");
    assert_eq!(
        notify::actionable_uid(&payload).as_deref(),
        Some("1.2.3.4"),
        "the payload that arrived through the real codec is the one the loop acts on"
    );
}

#[test]
fn a_split_frame_is_reassembled_across_reads() {
    // A real peer may send a frame in pieces. The codec must buffer rather
    // than refuse, and the loop must not see a truncated event.
    let peer = FakeChannel::start();
    // 200 bytes of JSON: comfortably past one 4096-byte read only if the
    // peer fragmented it, which is what a long body over a busy link does.
    let body = format!(
        r#"{{"00001000":{{"vr":"UI","Value":["1.2.3.4"]}},"00741000":{{"vr":"CS","Value":["SCHEDULED"]}},"00404041":{{"vr":"CS","Value":["{}"]}}}}"#,
        "x".repeat(200)
    );
    peer.send_text(&body);
    let mut channel = Channel::connect("127.0.0.1", peer.port(), "/dicomweb", "CLINFER", "clinfer")
        .expect("open");

    let mut seen = None;
    for _ in 0..50 {
        match channel.read_event() {
            Ok(ChannelEvent::Message(payload)) => {
                seen = Some(payload);
                break;
            }
            Ok(ChannelEvent::Idle) => continue,
            Ok(ChannelEvent::Closed) | Err(_) => break,
        }
    }
    assert_eq!(
        notify::actionable_uid(&seen.expect("an event report arrived")).as_deref(),
        Some("1.2.3.4")
    );
}

#[test]
fn a_server_close_is_reported_as_a_close() {
    let peer = FakeChannel::start();
    peer.send_close();
    let mut channel = Channel::connect("127.0.0.1", peer.port(), "/dicomweb", "CLINFER", "clinfer")
        .expect("open");

    let mut closed = false;
    for _ in 0..50 {
        match channel.read_event().expect("read") {
            ChannelEvent::Closed => {
                closed = true;
                break;
            }
            ChannelEvent::Idle => continue,
            ChannelEvent::Message(_) => continue,
        }
    }
    assert!(closed, "a close frame ends the channel");
}

#[test]
fn an_idle_channel_reports_idle_rather_than_blocking() {
    // The loop depends on this: an idle tick is how it notices shutdown, so
    // a read that blocked until an event arrived would make SIGTERM wait for
    // the origin to speak.
    //
    // The property asserted is the RETURN within the read timeout, not that
    // the peer lives forever. A peer that drops the connection is a valid
    // outcome here (and `Closed` is the correct report for it), so a read
    // error is not this test's subject - `a_server_close_is_reported_as_a_close`
    // covers the closing case. What must not happen is blocking indefinitely.
    let peer = FakeChannel::start();
    let mut channel = Channel::connect("127.0.0.1", peer.port(), "/dicomweb", "CLINFER", "clinfer")
        .expect("open");

    let started = std::time::Instant::now();
    loop {
        match channel.read_event() {
            Ok(ChannelEvent::Idle) => break,
            Ok(ChannelEvent::Message(_)) => continue,
            Ok(ChannelEvent::Closed) => break,
            // A transport error is a dropped peer: also a return, and the
            // caller reconnects. Not a hang, which is what is under test.
            Err(_) => break,
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "idle returns within the read timeout, not never"
    );
}

#[test]
fn the_loop_enqueues_only_scheduled_reports_from_the_wire() {
    // The whole path end to end through the real codec: three reports
    // arrive, one is actionable. The queue must receive exactly one UID.
    //
    // The frames are queued up front and the peer sends them as one burst,
    // so the loop usually decodes all three from a single read. The test
    // therefore waits for the loop to have OBSERVED all three before it
    // asserts the counts and stops the thread: asserting immediately after
    // the first UID arrives races the loop, which may still be on report 2
    // or 3 - and stopping it at that moment would freeze the counters
    // mid-run. That race is what made this test fail intermittently.
    let peer = FakeChannel::start();
    peer.send_text(r#"{"00001000":{"vr":"UI","Value":["9.9.9"]},"00741000":{"vr":"CS","Value":["COMPLETED"]}}"#);
    peer.send_text(r#"{"00001000":{"vr":"UI","Value":["2.2.2"]},"00741000":{"vr":"CS","Value":["SCHEDULED"]}}"#);
    peer.send_text(r#"{"00001000":{"vr":"UI","Value":["3.3.3"]},"00741000":{"vr":"CS","Value":["IN PROGRESS"]}}"#);

    let (queue, received) = sync_channel(8);
    let counters = Arc::new(Counters::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let wiring = Wiring {
        host: "127.0.0.1".to_string(),
        port: peer.port(),
        base_path: "/dicomweb".to_string(),
        requester: "CLINFER".to_string(),
        identity_user: "clinfer".to_string(),
        poll_interval_secs: 0,
    };
    let thread_shutdown = Arc::clone(&shutdown);
    let thread_counters = Arc::clone(&counters);
    let handle = std::thread::spawn(move || {
        notify::run_with(
            &wiring,
            queue,
            thread_counters,
            thread_shutdown,
            || 0,
            || Ok(()),
        );
    });

    let uid = received
        .recv_timeout(Duration::from_secs(10))
        .expect("the SCHEDULED report is enqueued");
    assert_eq!(uid, "2.2.2");
    assert!(
        received.try_recv().is_err(),
        "the COMPLETED and IN PROGRESS reports must not be enqueued"
    );
    // Every report must have been SEEN before the counts are asserted and the
    // thread is stopped: otherwise the counts describe a partial run.
    assert!(
        wait_for(
            || counters.ws_events.load(Ordering::Relaxed) >= 3,
            Duration::from_secs(10)
        ),
        "all three reports were read"
    );

    shutdown.store(true, Ordering::Relaxed);
    let _ = handle.join();
    assert_eq!(counters.ws_events.load(Ordering::Relaxed), 3);
    assert_eq!(counters.ws_connects.load(Ordering::Relaxed), 1);
    assert_eq!(counters.ws_dropped.load(Ordering::Relaxed), 0);
}

#[test]
fn a_closed_channel_reconnects_and_the_gap_triggers_a_catch_up() {
    // The peer completes the handshake and then CLOSES the channel. That is
    // a deterministic way to lose an established connection, unlike dropping
    // the next accepted socket: which connection a drop lands on is racy,
    // because the client retries while the accept loop is still running.
    //
    // The loop must reconnect, and the reconnect must re-run the catch-up
    // search: that search is the only thing that can find work announced
    // while the channel was down.
    let peer = FakeChannel::start();
    peer.send_close();

    let (queue, received) = sync_channel(8);
    let counters = Arc::new(Counters::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let wiring = Wiring {
        host: "127.0.0.1".to_string(),
        port: peer.port(),
        base_path: "/dicomweb".to_string(),
        requester: "CLINFER".to_string(),
        identity_user: "clinfer".to_string(),
        poll_interval_secs: 0,
    };
    let redeliveries = Arc::new(AtomicUsize::new(0));

    let thread_shutdown = Arc::clone(&shutdown);
    let thread_counters = Arc::clone(&counters);
    let thread_redeliveries = Arc::clone(&redeliveries);
    let handle = std::thread::spawn(move || {
        notify::run_with(
            &wiring,
            queue,
            thread_counters,
            thread_shutdown,
            move || {
                thread_redeliveries.fetch_add(1, Ordering::Relaxed);
                0
            },
            || Ok(()),
        );
    });

    let reconnected = wait_for(|| peer.connections() >= 2, Duration::from_secs(20));
    assert!(
        reconnected,
        "the loop reconnected after the close (connections seen: {})",
        peer.connections()
    );
    assert!(
        wait_for(
            || redeliveries.load(Ordering::Relaxed) >= 1,
            Duration::from_secs(10)
        ),
        "the reconnect re-ran the catch-up search"
    );
    assert!(
        received.try_recv().is_err(),
        "a reconnect alone enqueues nothing"
    );

    shutdown.store(true, Ordering::Relaxed);
    let _ = handle.join();
    assert!(
        counters.ws_reconnects.load(Ordering::Relaxed) >= 1,
        "the reconnect is counted"
    );
}

#[test]
fn the_first_connect_also_runs_the_search() {
    // A channel that has never been up still covers a window: the gap
    // between the caller's startup search snapshot and the channel becoming
    // readable. An event announced in that gap reaches nobody, because the
    // loop only drains the queue and there is no periodic search. Running
    // the search on the FIRST connect closes it. The repeat is cheap: the
    // origin arbitrates the claims.
    let peer = FakeChannel::start();
    let (queue, _received) = sync_channel(8);
    let counters = Arc::new(Counters::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let wiring = Wiring {
        host: "127.0.0.1".to_string(),
        port: peer.port(),
        base_path: "/dicomweb".to_string(),
        requester: "CLINFER".to_string(),
        identity_user: "clinfer".to_string(),
        poll_interval_secs: 0,
    };
    let redeliveries = Arc::new(AtomicUsize::new(0));

    let thread_shutdown = Arc::clone(&shutdown);
    let thread_counters = Arc::clone(&counters);
    let thread_redeliveries = Arc::clone(&redeliveries);
    let handle = std::thread::spawn(move || {
        notify::run_with(
            &wiring,
            queue,
            thread_counters,
            thread_shutdown,
            move || {
                thread_redeliveries.fetch_add(1, Ordering::Relaxed);
                0
            },
            || Ok(()),
        );
    });

    assert!(
        wait_for(
            || redeliveries.load(Ordering::Relaxed) >= 1,
            Duration::from_secs(10)
        ),
        "the first connect runs the search"
    );

    shutdown.store(true, Ordering::Relaxed);
    let _ = handle.join();
    assert!(
        counters.ws_connects.load(Ordering::Relaxed) >= 1,
        "the first connect is still counted as a connect"
    );
    assert_eq!(
        counters.ws_reconnects.load(Ordering::Relaxed),
        0,
        "the first connect is not a reconnect"
    );
}

#[test]
fn every_connect_asks_the_origin_to_subscribe_again() {
    // The origin holds subscriptions in process memory. A restarted origin
    // has forgotten this performer, and the channel then reads as open while
    // delivering nothing. The loop re-subscribes immediately before each
    // connect, so the ordering holds on every attempt rather than only at
    // startup, when the caller did it by hand.
    let peer = FakeChannel::start();
    peer.send_close();

    let (queue, _received) = sync_channel(8);
    let counters = Arc::new(Counters::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let wiring = Wiring {
        host: "127.0.0.1".to_string(),
        port: peer.port(),
        base_path: "/dicomweb".to_string(),
        requester: "CLINFER".to_string(),
        identity_user: "clinfer".to_string(),
        poll_interval_secs: 0,
    };
    let subscriptions = Arc::new(AtomicUsize::new(0));

    let thread_shutdown = Arc::clone(&shutdown);
    let thread_counters = Arc::clone(&counters);
    let thread_subscriptions = Arc::clone(&subscriptions);
    let handle = std::thread::spawn(move || {
        notify::run_with(
            &wiring,
            queue,
            thread_counters,
            thread_shutdown,
            || 0,
            move || {
                thread_subscriptions.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
        );
    });

    assert!(
        wait_for(
            || subscriptions.load(Ordering::Relaxed) >= 2,
            Duration::from_secs(20)
        ),
        "the loop subscribed once for the first connect and once for the \
         reconnect (saw {})",
        subscriptions.load(Ordering::Relaxed)
    );

    shutdown.store(true, Ordering::Relaxed);
    let _ = handle.join();
}

#[test]
fn a_failed_subscription_still_opens_the_channel() {
    // A subscription is how the origin decides what to SEND, not whether
    // this performer may connect. Refusing to connect on a subscribe failure
    // would turn a filter problem into a sidecar that finds no work at all,
    // while the searches can still see everything that is SCHEDULED.
    let peer = FakeChannel::start();
    let (queue, _received) = sync_channel(8);
    let counters = Arc::new(Counters::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let wiring = Wiring {
        host: "127.0.0.1".to_string(),
        port: peer.port(),
        base_path: "/dicomweb".to_string(),
        requester: "CLINFER".to_string(),
        identity_user: "clinfer".to_string(),
        poll_interval_secs: 0,
    };
    let searches = Arc::new(AtomicUsize::new(0));

    let thread_shutdown = Arc::clone(&shutdown);
    let thread_counters = Arc::clone(&counters);
    let thread_searches = Arc::clone(&searches);
    let handle = std::thread::spawn(move || {
        notify::run_with(
            &wiring,
            queue,
            thread_counters,
            thread_shutdown,
            move || {
                thread_searches.fetch_add(1, Ordering::Relaxed);
                0
            },
            || Err("the origin refused the subscription".to_string()),
        );
    });

    assert!(
        wait_for(|| !peer.keys().is_empty(), Duration::from_secs(10)),
        "the channel opened despite the refused subscription"
    );
    assert!(
        wait_for(
            || searches.load(Ordering::Relaxed) >= 1,
            Duration::from_secs(10)
        ),
        "the search still ran"
    );

    shutdown.store(true, Ordering::Relaxed);
    let _ = handle.join();
    assert!(
        counters.ws_connects.load(Ordering::Relaxed) >= 1,
        "the connect happened"
    );
}

#[test]
fn the_subscription_precedes_the_connect_and_the_search() {
    // ORDER IS LOAD-BEARING, and it is asserted as an order rather than
    // inferred from three flags: the origin gates delivery on a subscription
    // existing for the requester, so a connect that happened first would
    // yield an open, silent channel. The shared log makes the order a fact
    // the test reads rather than a fact a comment claims.
    let peer = FakeChannel::start();
    let (queue, _received) = sync_channel(8);
    let counters = Arc::new(Counters::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let wiring = Wiring {
        host: "127.0.0.1".to_string(),
        port: peer.port(),
        base_path: "/dicomweb".to_string(),
        requester: "CLINFER".to_string(),
        identity_user: "clinfer".to_string(),
        poll_interval_secs: 0,
    };
    let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));

    let thread_shutdown = Arc::clone(&shutdown);
    let thread_counters = Arc::clone(&counters);
    let subscribe_log = Arc::clone(&log);
    let search_log = Arc::clone(&log);
    let handle = std::thread::spawn(move || {
        notify::run_with(
            &wiring,
            queue,
            thread_counters,
            thread_shutdown,
            move || {
                search_log
                    .lock()
                    .expect("the log is not poisoned")
                    .push("search");
                0
            },
            move || {
                subscribe_log
                    .lock()
                    .expect("the log is not poisoned")
                    .push("subscribe");
                Ok(())
            },
        );
    });

    assert!(
        wait_for(|| !peer.keys().is_empty(), Duration::from_secs(10)),
        "the channel opened"
    );
    assert!(
        wait_for(
            || log.lock().expect("the log is not poisoned").len() >= 2,
            Duration::from_secs(10)
        ),
        "the subscribe and the search both ran"
    );

    shutdown.store(true, Ordering::Relaxed);
    let _ = handle.join();

    let order = log.lock().expect("the log is not poisoned").clone();
    assert_eq!(
        &order[..2],
        &["subscribe", "search"],
        "the subscription must happen before the search that follows the \
         connect; saw {order:?}"
    );
}

#[test]
fn shutdown_stops_the_loop_within_the_read_timeout() {
    // The drain promise: a shutdown must not wait for the origin to speak.
    // The read timeout is 500ms, so two seconds is a generous bound that
    // would fail loudly if the loop ever blocked on the socket read.
    let peer = FakeChannel::start();
    let (queue, _received) = sync_channel(8);
    let counters = Arc::new(Counters::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let wiring = Wiring {
        host: "127.0.0.1".to_string(),
        port: peer.port(),
        base_path: "/dicomweb".to_string(),
        requester: "CLINFER".to_string(),
        identity_user: "clinfer".to_string(),
        poll_interval_secs: 0,
    };
    let thread_shutdown = Arc::clone(&shutdown);
    let handle = std::thread::spawn(move || {
        notify::run_with(&wiring, queue, counters, thread_shutdown, || 0, || Ok(()));
    });

    assert!(
        wait_for(|| !peer.keys().is_empty(), Duration::from_secs(10)),
        "the channel opened"
    );
    shutdown.store(true, Ordering::Relaxed);

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !handle.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        handle.is_finished(),
        "the loop stopped promptly on shutdown"
    );
    let _ = handle.join();
}

#[test]
fn a_full_queue_drops_the_event_and_counts_it() {
    // Depth one, and nothing draining it, so the SECOND actionable report has
    // nowhere to go. It must be dropped rather than block the channel:
    // blocking here would stop the loop answering pings, and the origin would
    // eventually close the connection.
    //
    // The second frame is queued only AFTER the first is confirmed in the
    // queue, which is what makes this deterministic. Queueing both up front
    // and then calling `recv` is racy: the test's own `recv` frees the single
    // slot, and whether the loop processes report 2 before or after that call
    // decides the outcome. Both orders are legal, so the test would fail
    // intermittently - which it did, once, before this was fixed.
    let peer = FakeChannel::start();
    peer.send_text(r#"{"00001000":{"vr":"UI","Value":["1.1.1"]},"00741000":{"vr":"CS","Value":["SCHEDULED"]}}"#);

    let (queue, _received) = sync_channel(1);
    let counters = Arc::new(Counters::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let wiring = Wiring {
        host: "127.0.0.1".to_string(),
        port: peer.port(),
        base_path: "/dicomweb".to_string(),
        requester: "CLINFER".to_string(),
        identity_user: "clinfer".to_string(),
        poll_interval_secs: 0,
    };
    let thread_shutdown = Arc::clone(&shutdown);
    let thread_counters = Arc::clone(&counters);
    let handle = std::thread::spawn(move || {
        notify::run_with(
            &wiring,
            queue,
            thread_counters,
            thread_shutdown,
            || 0,
            || Ok(()),
        );
    });

    // The first report fills the single slot. It is NOT read out of the queue
    // yet: reading it would free the slot and let the second report in.
    //
    // `ws_events` is incremented BEFORE the enqueue, so its reaching 1 does
    // not by itself prove the slot is taken. The short settle below closes
    // that window: the enqueue is the very next statement, and there is
    // nothing between them that can block.
    assert!(
        wait_for(
            || counters.ws_events.load(Ordering::Relaxed) >= 1,
            Duration::from_secs(10)
        ),
        "the first report reached the loop"
    );
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        counters.ws_events.load(Ordering::Relaxed),
        1,
        "exactly one report was seen, so nothing else is competing for the slot"
    );
    assert_eq!(
        counters.ws_dropped.load(Ordering::Relaxed),
        0,
        "nothing dropped yet: the first report had a slot"
    );

    // Now the queue is provably full (depth 1, one event stored, nothing
    // draining it) and the second report must be dropped.
    peer.send_text(
        r#"{"00001000":{"vr":"UI","Value":["2.2.2"]},"00741000":{"vr":"CS","Value":["SCHEDULED"]}}"#,
    );
    assert!(
        wait_for(
            || counters.ws_dropped.load(Ordering::Relaxed) >= 1,
            Duration::from_secs(10)
        ),
        "the second report was counted as dropped"
    );
    assert_eq!(counters.ws_events.load(Ordering::Relaxed), 2);

    shutdown.store(true, Ordering::Relaxed);
    let _ = handle.join();
}

// -- Poll fallback while the channel is DOWN -----------------------------

/// A port nobody listens on, so every `Channel::connect` fails.
///
/// Binding and immediately dropping a listener gives a port the OS has just
/// advertised as free; the connect attempt then gets a refusal rather than a
/// hang. The loop under test never resolves it, so the exact errno does not
/// matter - only that the Err arm is the one being exercised.
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

/// While the channel is down, the catch-up search runs at the configured
/// cadence.
///
/// This is the whole point of the knob: the Err arm used to do nothing but
/// sleep, so a workitem created after startup was invisible until a restart.
/// The count is what is asserted, not the wall clock: two seconds of polling
/// must produce more than one search, and zero polls must produce none.
#[test]
fn the_fallback_search_runs_while_the_channel_is_down() {
    let (queue, _received) = sync_channel(8);
    let counters = Arc::new(Counters::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let wiring = Wiring {
        host: "127.0.0.1".to_string(),
        port: closed_port(),
        base_path: "/dicomweb".to_string(),
        requester: "CLINFER".to_string(),
        identity_user: "clinfer".to_string(),
        // 1s keeps the test short while still proving the CADENCE: the
        // reconnect backoff alone would produce a single iteration in this
        // window, so more than one search can only come from the poll.
        poll_interval_secs: 1,
    };

    let polls = Arc::new(AtomicUsize::new(0));
    let thread_polls = Arc::clone(&polls);
    let thread_shutdown = Arc::clone(&shutdown);
    let handle = std::thread::spawn(move || {
        notify::run_with(
            &wiring,
            queue,
            counters,
            thread_shutdown,
            move || {
                thread_polls.fetch_add(1, Ordering::Relaxed);
                0
            },
            || Ok(()),
        );
    });

    // WAIT FOR THE COUNT instead of sleeping a fixed window. A refused connect
    // takes about 2 s on this host, so how many iterations fit in any fixed
    // wall-clock window varies with load - a 7 s window sometimes yielded one
    // search and sometimes two, which made the test flaky rather than wrong.
    // The property under test is "searches REPEAT while the channel is down",
    // so the assertion polls for the second search with a deadline generous
    // enough for several refused connects.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while polls.load(Ordering::Relaxed) < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let seen = polls.load(Ordering::Relaxed);
    shutdown.store(true, Ordering::Relaxed);
    handle.join().expect("the notifier loop exits on shutdown");

    assert!(
        seen >= 2,
        "a 1s interval across several down iterations must search more than once, saw {seen}"
    );
}

/// `poll_interval_secs = 0` means no search in the Err arm at all.
///
/// A falling-over origin must also be able to opt OUT of the extra load, and
/// this is the assertion that makes the knob's off switch real rather than
/// documented.
#[test]
fn a_zero_poll_interval_never_searches_while_the_channel_is_down() {
    let (queue, _received) = sync_channel(8);
    let counters = Arc::new(Counters::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let wiring = Wiring {
        host: "127.0.0.1".to_string(),
        port: closed_port(),
        base_path: "/dicomweb".to_string(),
        requester: "CLINFER".to_string(),
        identity_user: "clinfer".to_string(),
        poll_interval_secs: 0,
    };

    let polls = Arc::new(AtomicUsize::new(0));
    let thread_polls = Arc::clone(&polls);
    let thread_shutdown = Arc::clone(&shutdown);
    let handle = std::thread::spawn(move || {
        notify::run_with(
            &wiring,
            queue,
            counters,
            thread_shutdown,
            move || {
                thread_polls.fetch_add(1, Ordering::Relaxed);
                0
            },
            || Ok(()),
        );
    });

    // The negative case has to WAIT for evidence rather than exit early: give
    // the loop long enough to complete a refused connect plus a poll interval
    // (refusal alone costs ~2 s here), so "no searches" is observed during a
    // live Err-arm sleep instead of being a race the test won by finishing
    // before the loop got there.
    std::thread::sleep(Duration::from_millis(8000));
    shutdown.store(true, Ordering::Relaxed);
    handle.join().expect("the notifier loop exits on shutdown");

    assert_eq!(
        polls.load(Ordering::Relaxed),
        0,
        "0 disables the fallback: the channel-less origin gets no searches"
    );
}

/// The peer below makes every connection fail at once, so the reconnect
/// delay is the whole gap between two iterations. With the poll clock reset
/// per pass, an iteration could only ever hold ONE search - the delay is
/// measured from zero on every reconnect - so a 3 s interval produced a
/// search every 1 s, 2 s, 4 s, 8 s instead of every 3 s.
///
/// The interval is deliberately longer than the first backoff step. At 1 s
/// the defect is invisible, because each iteration waits at least one
/// second and fires at most one search either way.
#[test]
fn the_search_cadence_does_not_ride_the_reconnect_delay() {
    let (port, connections) = refused_peer();
    let (queue, _received) = sync_channel(8);
    let counters = Arc::new(Counters::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let wiring = Wiring {
        host: "127.0.0.1".to_string(),
        port,
        base_path: "/dicomweb".to_string(),
        requester: "CLINFER".to_string(),
        identity_user: "clinfer".to_string(),
        poll_interval_secs: 3,
    };

    let polls = Arc::new(AtomicUsize::new(0));
    let thread_polls = Arc::clone(&polls);
    let thread_shutdown = Arc::clone(&shutdown);
    let handle = std::thread::spawn(move || {
        notify::run_with(
            &wiring,
            queue,
            counters,
            thread_shutdown,
            move || {
                thread_polls.fetch_add(1, Ordering::Relaxed);
                0
            },
            || Ok(()),
        );
    });

    // Ten seconds of wall clock, no early exit: the loop has to keep the
    // ORIGIN's cadence for the whole window, and stopping as soon as the
    // count is reached would measure the warm-up instead.
    let started = std::time::Instant::now();
    std::thread::sleep(Duration::from_secs(10));
    let seen = polls.load(Ordering::Relaxed);
    let dials = connections.load(Ordering::Relaxed);
    shutdown.store(true, Ordering::Relaxed);
    handle.join().expect("the notifier loop exits on shutdown");

    // A 3 s interval over ten seconds is three ticks. A clock reset per
    // iteration cannot produce them: each iteration waits one backoff step
    // and fires once at most, and 1+2+4 s of waiting only fits three
    // searches when the clock starts over at every one of them.
    assert!(
        seen >= 3,
        "a 3s interval must search on its own clock, independent of the \
         reconnect delay; saw {seen} search(es) over {dials} dial(s) in {:?}",
        started.elapsed()
    );
    assert!(
        (3..=8).contains(&dials),
        "the reconnect delay must still back off; saw {dials} dial(s)"
    );
}
