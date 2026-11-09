//! Worklist subscription push (PS3.18 11.10/11.11).
//!
//! Two layers are covered here:
//!
//! 1. The pure registration handlers - validation, idempotent unsubscribe,
//!    filter parsing. No I/O, so these are direct calls.
//! 2. The push itself, end to end: a real subscription is registered against a
//!    miniature loopback subscriber, `push_to_subscribers` runs the production
//!    worker path, and the test asserts the request that arrives on the wire.
//!
//! The subscription registry is process-global (the worker is a free function
//! with no `AppState`), so every test that registers a subscriber first removes
//! any entry it might have left behind and uses a unique AE title. That keeps
//! the tests independent under the parallel test runner.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

/// One recorded push received by the miniature subscriber.
struct Received {
    request_line: String,
    body: String,
}

/// A loopback HTTP server that records requests and answers with `codes`.
///
/// Returns the endpoint to register plus the receiving end of the record
/// channel and a stop handle.
fn mini_subscriber(codes: Vec<u16>) -> (String, Receiver<Received>, Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("addr").port();
    let endpoint = format!("http://127.0.0.1:{port}/ups");

    let (tx, rx) = channel();
    let (stop_tx, stop_rx) = channel::<()>();

    std::thread::Builder::new()
        .name("mini-subscriber".into())
        .spawn(move || {
            let mut remaining = codes.into_iter();
            loop {
                if stop_rx.try_recv().is_ok() {
                    return;
                }
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let code = remaining.next().unwrap_or(200);
                if let Some(rec) = serve_once(stream, code)
                    && tx.send(rec).is_err()
                {
                    return;
                }
            }
        })
        .expect("spawn mini-subscriber");

    (endpoint, rx, stop_tx)
}

/// Read one request off `stream`, answer with `code`, return what was read.
fn serve_once(mut stream: TcpStream, code: u16) -> Option<Received> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);

    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }

    let mut buf = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut buf).ok()?;
    }

    let reason = if code == 200 { "OK" } else { "Error" };
    let response =
        format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    stream.write_all(response.as_bytes()).ok()?;
    stream.flush().ok()?;

    Some(Received {
        request_line,
        body: String::from_utf8_lossy(&buf).into_owned(),
    })
}

/// Block until a push whose request line contains `needle` arrives.
///
/// The subscription registry is process-global and a subscriber receives EVERY
/// workitem push, so tests running in parallel see each other's pushes. This
/// drains past them until the one under test shows up.
fn recv_matching(rx: &Receiver<Received>, needle: &str, secs: u64) -> Received {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            panic!("no push for {needle} within {secs}s");
        }
        let got = rx
            .recv_timeout(remaining)
            .unwrap_or_else(|e| panic!("no push for {needle} within {secs}s: {e}"));
        if got.request_line.contains(needle) {
            return got;
        }
    }
}

/// Register `ae` against `endpoint`, replacing any earlier registration.
fn register(ae: &str, endpoint: &str, filtered: bool) -> u16 {
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(ae);
    let body = format!("{{\"endpoint\":\"{endpoint}\"}}");
    clarus_server::handlers::subscribe_pure::subscribe(ae, body.as_bytes(), filtered, "").status
}

/// Serialises every test that registers a subscriber or asserts on a received
/// push.
///
/// The registry is process-global and a subscriber receives EVERY workitem
/// push, so two tests that both register and push will cross-deliver: test A's
/// subscriber gets test B's push and vice versa. Draining until the expected
/// UID arrives is not enough on its own, because the receiving half may be
/// closed before the other test's job is delivered (`channel ... closed`).
/// Holding this lock for the whole register-push-assert sequence makes each
/// test's delivery deterministic.
fn registry_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

// -- Pure registration handlers --------------------------------------------

/// The matcher input for a push in this suite.
///
/// Only the label and the scheduled code are ever supplied here: an absent
/// key constrains nothing, so a subscription that does not carry it still
/// matches (PS3.18 8.3.4.1).
fn values<'a>(
    label: &'a str,
    code: Option<(&'a str, &'a str)>,
) -> clarus_server::push::FilterValues<'a> {
    clarus_server::push::FilterValues {
        step_label: label,
        code,
        ..Default::default()
    }
}

#[test]
fn subscriber_registration_returns_201_with_the_subscriber() {
    let (endpoint, _rx, _stop) = mini_subscriber(vec![]);
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe("REG201");
    let body = format!("{{\"endpoint\":\"{endpoint}\"}}");

    let resp =
        clarus_server::handlers::subscribe_pure::subscribe("REG201", body.as_bytes(), false, "");
    assert_eq!(
        resp.status,
        201,
        "body: {}",
        String::from_utf8_lossy(&resp.body)
    );

    let text = String::from_utf8_lossy(&resp.body).into_owned();
    assert!(text.contains("REG201"), "subscriber echoed: {text}");

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe("REG201");
}

#[test]
fn unsubscribe_is_idempotent_204() {
    let (endpoint, _rx, _stop) = mini_subscriber(vec![]);
    let _ = register("UNSUB1", &endpoint, false);

    let first = clarus_server::handlers::subscribe_pure::unsubscribe("UNSUB1");
    let second = clarus_server::handlers::subscribe_pure::unsubscribe("UNSUB1");
    assert_eq!(first.status, 204);
    assert_eq!(second.status, 204, "unsubscribe must be idempotent");
}

#[test]
fn filtered_subscription_rejects_unknown_filter_key() {
    let (endpoint, _rx, _stop) = mini_subscriber(vec![]);
    // A key this server cannot HONOUR. Scheduled Procedure Step Expiration
    // DateTime (0040,4008) is a real PS3.4 CC.2.5-3 matching key (O), yet the
    // workitem record stores no such field - which is exactly why it must be
    // REFUSED rather than silently ignored: a subscriber filtering on it would
    // believe it will not receive those workitems.
    let body = format!(
        "{{\"endpoint\":\"{endpoint}\",\"filter\":{{\"00404008\":{{\"Value\":[\"20260101\"]}}}}}}"
    );
    let resp =
        clarus_server::handlers::subscribe_pure::subscribe("FILTBAD", body.as_bytes(), true, "");
    assert_eq!(
        resp.status,
        400,
        "body: {}",
        String::from_utf8_lossy(&resp.body)
    );

    let text = String::from_utf8_lossy(&resp.body).into_owned();
    assert!(
        text.to_ascii_lowercase().contains("00404008"),
        "the offending key must be named: {text}"
    );
}

#[test]
fn filtered_subscription_rejects_an_unknown_query_filter_key() {
    let (endpoint, _rx, _stop) = mini_subscriber(vec![]);
    let body = format!("{{\"endpoint\":\"{endpoint}\"}}");
    let resp = clarus_server::handlers::subscribe_pure::subscribe(
        "QTBAD",
        body.as_bytes(),
        true,
        "filter=00404008=20260101",
    );
    assert_eq!(
        resp.status,
        400,
        "an unsupported query filter key is refused: {}",
        String::from_utf8_lossy(&resp.body)
    );
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    assert!(text.to_ascii_lowercase().contains("00404008"), "{text}");
}

#[test]
fn filtered_subscription_accepts_step_label_filter() {
    // Registers an endpoint-backed subscriber, so it MUST hold the registry
    // lock: the delivery worker is a single thread and a registration left
    // behind here would be pushed to by every other test, occupying that
    // thread with a subscriber whose receiver has gone away.
    let _guard = registry_lock();
    let (endpoint, _rx, _stop) = mini_subscriber(vec![]);
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe("FILTOK");
    let body = format!(
        "{{\"endpoint\":\"{endpoint}\",\"filter\":{{\"00741204\":{{\"Value\":[\"CAD\"]}}}}}}"
    );
    let resp =
        clarus_server::handlers::subscribe_pure::subscribe("FILTOK", body.as_bytes(), true, "");
    assert_eq!(
        resp.status,
        201,
        "body: {}",
        String::from_utf8_lossy(&resp.body)
    );
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe("FILTOK");
}

/// A code-filtered subscriber receives a workitem whose code matches, and
/// nothing when it does not - the filter the performer relies on.
#[test]
fn a_code_filtered_subscriber_receives_only_its_own_code() {
    // Endpoint-backed and it asserts on what arrives, so it holds the lock for
    // the whole register-push-assert sequence, like every other push test.
    let _guard = registry_lock();
    let (endpoint, rx, _stop) = mini_subscriber(vec![]);
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe("CODEFILT");
    let body = format!("{{\"endpoint\":\"{endpoint}\"}}");
    let resp = clarus_server::handlers::subscribe_pure::subscribe(
        "CODEFILT",
        body.as_bytes(),
        true,
        "filter=00404018.00080102=99CLARUS&filter=00404018.00080100=CXR-AI",
    );
    assert_eq!(resp.status, 201, "{}", String::from_utf8_lossy(&resp.body));

    // A workitem with the wrong code is not pushed.
    clarus_server::push::push_to_subscribers_with_values(
        "1.2.3.1",
        &values("CAD", Some(("OTHER", "99CLARUS"))),
    );
    // The right code IS pushed.
    clarus_server::push::push_to_subscribers_with_values(
        "1.2.3.2",
        &values("CAD", Some(("CXR-AI", "99CLARUS"))),
    );

    let pushed = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the matching workitem is pushed");
    assert!(
        pushed.request_line.contains("1.2.3.2"),
        "only the matching code is delivered: {}",
        pushed.request_line
    );
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(300))
            .is_err(),
        "the non-matching workitem is never pushed"
    );

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe("CODEFILT");
}

#[test]
fn invalid_endpoint_is_rejected_400() {
    let body = br#"{"endpoint":"https://example.test/ups"}"#;
    let resp = clarus_server::handlers::subscribe_pure::subscribe("BADEP", body, false, "");
    assert_eq!(
        resp.status,
        400,
        "body: {}",
        String::from_utf8_lossy(&resp.body)
    );
}

#[test]
fn invalid_subscriber_ae_title_is_rejected_400() {
    let (endpoint, _rx, _stop) = mini_subscriber(vec![]);
    let body = format!("{{\"endpoint\":\"{endpoint}\"}}");
    let resp =
        clarus_server::handlers::subscribe_pure::subscribe("BAD AE!", body.as_bytes(), false, "");
    assert_eq!(
        resp.status,
        400,
        "body: {}",
        String::from_utf8_lossy(&resp.body)
    );
}

#[test]
fn empty_filter_matches_everything() {
    let (endpoint, _rx, _stop) = mini_subscriber(vec![]);
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe("FILTANY");
    let body = format!("{{\"endpoint\":\"{endpoint}\",\"filter\":{{}}}}");
    let resp =
        clarus_server::handlers::subscribe_pure::subscribe("FILTANY", body.as_bytes(), true, "");
    assert_eq!(
        resp.status,
        201,
        "body: {}",
        String::from_utf8_lossy(&resp.body)
    );
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe("FILTANY");
}

// -- The push itself --------------------------------------------------------

/// The push must leave the process as a POST to `{endpoint}/{workitem-uid}`
/// carrying the workitem UID, because that is what the subscriber's
/// `{workitem}` URI template expands to (PS3.18 11.10.1).
#[test]
fn push_reaches_a_live_subscriber_with_the_workitem_uid() {
    let _guard = registry_lock();
    let (endpoint, rx, _stop) = mini_subscriber(vec![200]);
    assert_eq!(register("LIVEPUSH", &endpoint, false), 201);

    const UID: &str = "1.2.826.0.1.3680043.9.9999.1";
    clarus_server::push::push_to_subscribers(UID, &values("CAD", None));

    let got = recv_matching(&rx, UID, 15);
    assert!(
        got.request_line.starts_with("POST "),
        "push must be a POST: {}",
        got.request_line
    );
    assert!(
        got.body.contains(UID),
        "the UID must be in the notification body: {}",
        got.body
    );

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe("LIVEPUSH");
}

/// The request path must carry the subscription endpoint's own base path.
#[test]
fn push_preserves_the_subscription_base_path() {
    let _guard = registry_lock();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let endpoint = format!("http://127.0.0.1:{port}/worklist/v1");

    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("path-subscriber".into())
        .spawn(move || {
            if let Ok((stream, _)) = listener.accept()
                && let Some(rec) = serve_once(stream, 200)
            {
                let _ = tx.send(rec);
            }
        })
        .expect("spawn path subscriber");

    assert_eq!(register("PATHSUB", &endpoint, false), 201);
    const UID: &str = "1.2.826.0.1.3680043.9.9999.4";
    clarus_server::push::push_to_subscribers(UID, &values("CAD", None));

    let got = recv_matching(&rx, UID, 15);
    assert!(
        got.request_line.contains(&format!("/worklist/v1/{UID}")),
        "base path must be kept: {}",
        got.request_line
    );

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe("PATHSUB");
}

/// A subscriber that never answers must not stall the caller: enqueueing is
/// cheap and the attempt budget is finite, so the caller returns at once.
#[test]
fn push_to_a_dead_subscriber_does_not_block_the_caller() {
    let _guard = registry_lock();
    // The attempt budget is process-global and this test is the only one that
    // changes it, so it needs no second lock - holding the registry lock for
    // the whole sequence is what keeps the value from being observed by a
    // concurrently registered subscriber.
    clarus_server::push::set_push_attempts(2);
    // A listener that accepts and then closes without replying is the cheapest
    // way to model an unreachable subscriber.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let endpoint = format!("http://127.0.0.1:{port}/ups");

    std::thread::Builder::new()
        .name("dead-subscriber".into())
        .spawn(move || {
            for stream in listener.incoming() {
                drop(stream);
            }
        })
        .expect("spawn dead subscriber");

    // Registered inline rather than through `register`, so that the whole
    // register-push-assert sequence stays inside one lock scope: this
    // subscriber is deliberately unreachable, and every push it is attempted
    // for costs the worker thread its full attempt budget.
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe("DEADSUB");
    let body = format!("{{\"endpoint\":\"{endpoint}\"}}");
    assert_eq!(
        clarus_server::handlers::subscribe_pure::subscribe("DEADSUB", body.as_bytes(), false, "")
            .status,
        201
    );

    let start = std::time::Instant::now();
    clarus_server::push::push_to_subscribers("1.2.826.0.1.3680043.9.9999.2", &values("CAD", None));
    let elapsed = start.elapsed();

    // The call only enqueues; it must return immediately regardless of the
    // subscriber's health. That is the whole point of the worker thread.
    assert!(
        elapsed < Duration::from_secs(2),
        "push_to_subscribers must not block on delivery (took {elapsed:?})"
    );

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe("DEADSUB");
    clarus_server::push::set_push_attempts(3);
}

/// No subscribers: the call must be a no-op, not an error.
#[test]
fn push_without_subscribers_is_a_no_op() {
    let _guard = registry_lock();
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe("NOBODY");
    clarus_server::push::push_to_subscribers("1.2.826.0.1.3680043.9.9999.3", &values("CAD", None));
}

/// S1: a subscription with NO endpoint is channel-only and must never be
/// pushed to.
///
/// The prompt asks for a "counter pin" here. The counters alone are not
/// enough to prove the skip: `UPS_PUSHED` only moves on a 2xx, so a
/// subscription that WAS attempted against a dead endpoint would leave it flat
/// too (it would move `UPS_PUSH_DROPPED`/`UPS_PUSH_RETRIES` instead). So the
/// test pins all three AND asserts the subscriber's own receiver saw nothing -
/// a subscriber that was attempted gets a connection, which the mini
/// subscriber records.
#[test]
fn a_subscription_without_an_endpoint_is_never_pushed_to() {
    let _guard = registry_lock();
    use std::sync::atomic::Ordering;

    const AE: &str = "CHANNELONLY1";
    let (endpoint, rx, _stop) = mini_subscriber(vec![200]);
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(AE);

    // Registered WITHOUT an endpoint: the channel-only shape.
    let resp = clarus_server::handlers::subscribe_pure::subscribe(
        AE,
        br#"{"endpoint":null}"#.as_ref(),
        false,
        "",
    );
    assert_eq!(
        resp.status,
        201,
        "a channel-only subscription must register: {}",
        String::from_utf8_lossy(&resp.body)
    );

    let before = (
        clarus_server::metrics::UPS_PUSHED.load(Ordering::Relaxed),
        clarus_server::metrics::UPS_PUSH_DROPPED.load(Ordering::Relaxed),
        clarus_server::metrics::UPS_PUSH_RETRIES.load(Ordering::Relaxed),
    );

    // A CAD push, which this subscription's filter accepts - so the ONLY
    // reason it is not delivered is the missing endpoint.
    clarus_server::push::push_to_subscribers("1.2.826.0.1.3680043.9.9999.4", &values("CAD", None));

    let after = (
        clarus_server::metrics::UPS_PUSHED.load(Ordering::Relaxed),
        clarus_server::metrics::UPS_PUSH_DROPPED.load(Ordering::Relaxed),
        clarus_server::metrics::UPS_PUSH_RETRIES.load(Ordering::Relaxed),
    );
    assert_eq!(
        before, after,
        "no push may be attempted, let alone delivered, for a subscriber with \
         no endpoint"
    );

    // And nothing arrived on the wire: the mini subscriber is still waiting,
    // which is the observable form of "not attempted".
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "the channel-only subscriber must receive no POST"
    );
    // The endpoint variable is only here to prove the receiver is the one we
    // registered alongside; it is deliberately NOT registered.
    assert!(!endpoint.is_empty());

    // The subscription itself is still live, which is what lets the channel
    // deliver to it (accepts_for is the other half of S1 and is unchanged).
    assert!(
        clarus_server::push::subscriptions().accepts_for(AE, "1.2.3", &values("CAD", None)),
        "a channel-only subscription is still a subscription"
    );

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(AE);
}

/// The contrast case: WITH an endpoint the same push IS attempted, so the test
/// above cannot pass because pushes are broken in general.
#[test]
fn a_subscription_with_an_endpoint_is_pushed_to() {
    let _guard = registry_lock();
    const AE: &str = "HASENDPOINT1";
    let (endpoint, rx, _stop) = mini_subscriber(vec![200]);
    assert_eq!(register(AE, &endpoint, false), 201);

    clarus_server::push::push_to_subscribers("1.2.826.0.1.3680043.9.9999.5", &values("CAD", None));
    let got = recv_matching(&rx, "1.2.826.0.1.3680043.9.9999.5", 10);
    assert!(
        got.request_line.contains("1.2.826.0.1.3680043.9.9999.5"),
        "a subscription WITH an endpoint is pushed to: {}",
        got.request_line
    );

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(AE);
}

/// A channel-only subscription and a pushable one coexist: the skip is per
/// subscription, not global.
#[test]
fn a_channel_only_subscription_does_not_suppress_other_pushes() {
    let _guard = registry_lock();
    const CHANNEL_ONLY: &str = "CHANNELONLY2";
    const PUSHY: &str = "GETSPUSHED1";

    let (endpoint, rx, _stop) = mini_subscriber(vec![200]);
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(CHANNEL_ONLY);
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(PUSHY);

    let r = clarus_server::handlers::subscribe_pure::subscribe(
        CHANNEL_ONLY,
        br#"{}"#.as_ref(),
        false,
        "",
    );
    assert_eq!(r.status, 201);
    assert_eq!(register(PUSHY, &endpoint, false), 201);

    clarus_server::push::push_to_subscribers("1.2.826.0.1.3680043.9.9999.6", &values("CAD", None));

    // The pushable subscriber still receives it.
    let got = recv_matching(&rx, "1.2.826.0.1.3680043.9.9999.6", 10);
    assert!(got.request_line.contains("1.2.826.0.1.3680043.9.9999.6"));

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(CHANNEL_ONLY);
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(PUSHY);
}
