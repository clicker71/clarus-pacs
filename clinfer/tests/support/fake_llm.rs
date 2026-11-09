// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! A fake OpenAI-compatible endpoint, for the wire tests.
//!
//! A `TcpListener` on an ephemeral port that answers
//! `POST /v1/chat/completions` with a canned completion and records the
//! requests it received. Like the fake origin, it is deliberately dumb: the
//! tests assert on what clinfer SENT, and a fake that decided anything on
//! its own would be testing itself.
//!
//! One listener, cleartext, on loopback - which is also the only endpoint
//! the product reaches: the configuration refuses anything else, so there is
//! no second transport for a fake to stand in for.
//!
//! Shared by the integration tests; compiled once.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// One request the fake received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    /// Lower-cased header names to values.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The `Authorization` header value, which every OpenAI-compatible
    /// endpoint authenticates with.
    #[must_use]
    pub fn authorization(&self) -> Option<&str> {
        self.header("authorization")
    }

    #[must_use]
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// How a scripted answer is shaped.
#[derive(Debug, Clone)]
pub enum Reply {
    /// A well-formed completion carrying this text.
    Text(String),
    /// A status with a JSON error body.
    Status(u16, String),
    /// Accept the connection and never answer: the timeout scenario.
    Hang,
}

/// A running fake endpoint.
pub struct FakeLlm {
    port: u16,
    requests: Arc<Mutex<Vec<Recorded>>>,
    replies: Arc<Mutex<Vec<Reply>>>,
    /// Set once the listener has answered at least one request, so a test
    /// can wait for the exchange instead of sleeping.
    seen: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    _thread: thread::JoinHandle<()>,
}

impl FakeLlm {
    /// Start a plaintext fake with the given script.
    ///
    /// An exhausted script answers the canned text below rather than
    /// failing: a test that ran out of replies has a bug, and a 200 with
    /// recognizable text makes that visible in the assertion rather than in
    /// a connection error.
    pub fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");

        let requests = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(replies));
        let seen = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_requests = Arc::clone(&requests);
        let thread_replies = Arc::clone(&replies);
        let thread_seen = Arc::clone(&seen);
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            loop {
                if thread_stop.load(Ordering::Relaxed) {
                    return;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // The listener is non-blocking so the accept loop can
                        // poll the stop flag, and an accepted socket INHERITS
                        // that mode. A read timeout does nothing on a
                        // non-blocking socket, so the mode is restored first;
                        // without it the first read can return WouldBlock
                        // under load and the exchange is abandoned.
                        if stream.set_nonblocking(false).is_err() {
                            continue;
                        }
                        let _ = stream
                            .set_read_timeout(Some(Duration::from_secs(5)));
                        serve(&mut stream, &thread_requests, &thread_replies, &thread_seen);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });

        Self {
            port,
            requests,
            replies,
            seen,
            stop,
            _thread: handle,
        }
    }

    /// The base URL to hand to a client, with no trailing slash.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    #[must_use]
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().expect("requests lock").clone()
    }

    /// The single request received, panicking if there was not exactly one.
    /// One call per study is the contract, so this is what the tests want
    /// almost everywhere.
    #[must_use]
    pub fn the_request(&self) -> Recorded {
        let requests = self.requests();
        assert_eq!(requests.len(), 1, "one request per study");
        requests.into_iter().next().expect("one request")
    }

    /// Wait until at least one request has been answered.
    #[must_use]
    pub fn wait_for_request(&self, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.seen.load(Ordering::Relaxed) > 0 {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        self.seen.load(Ordering::Relaxed) > 0
    }

    /// The number of requests received so far.
    #[must_use]
    pub fn count(&self) -> usize {
        self.requests.lock().expect("requests lock").len()
    }

    pub fn push_replies(&self, more: Vec<Reply>) {
        self.replies.lock().expect("replies lock").extend(more);
    }
}

impl Drop for FakeLlm {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
#[must_use]
pub fn completion_json(text: &str) -> String {
    // One `format!` with the text JSON-escaped: the reply must be parseable
    // by the client, or a test would be measuring the fake's bug.
    format!(
        "{{\"id\":\"chatcmpl-test\",\"object\":\"chat.completion\",\"created\":0,\
          \"model\":\"demo-chat\",\"choices\":[{{\"index\":0,\"message\":{{\"role\":\"assistant\",\
          \"content\":\"{}\"}},\"finish_reason\":\"stop\"}}]}}",
        escape_json(text)
    )
}

fn escape_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch => out.push(ch),
        }
    }
    out
}

fn answer<S: Write>(stream: &mut S, reply: Reply) {
    match reply {
        Reply::Text(text) => {
            let body = completion_json(&text);
            write_response(stream, 200, "OK", &body);
        }
        Reply::Status(status, body) => {
            let reason = match status {
                429 => "Too Many Requests",
                500 => "Internal Server Error",
                503 => "Service Unavailable",
                401 => "Unauthorized",
                _ => "Unknown",
            };
            write_response(stream, status, reason, &body);
        }
        // The connection stays open with nothing written, so the client's
        // read timeout is what ends the exchange. That is the timeout
        // scenario, and it is the only way to produce it without a clock
        // the test controls.
        Reply::Hang => {
            thread::sleep(Duration::from_secs(60));
        }
    }
}

/// The canned completion for a text reply.
/// Serve one request.
fn serve<S: Read + Write>(
    stream: &mut S,
    requests: &Arc<Mutex<Vec<Recorded>>>,
    replies: &Arc<Mutex<Vec<Reply>>>,
    seen: &Arc<AtomicU64>,
) {
    let Some(request) = read_request(stream) else {
        return;
    };
    requests.lock().expect("requests lock").push(request);
    seen.fetch_add(1, Ordering::Relaxed);

    let reply = {
        let mut queue = replies.lock().expect("replies lock");
        if queue.is_empty() {
            Reply::Text("no scripted reply".to_string())
        } else {
            queue.remove(0)
        }
    };
    answer(stream, reply);
}

/// Standard base64 (RFC 4648 section 4) decoded.
///
/// The fixture is committed as base64 because a byte-array literal of 302
/// bytes per blob is unreadable; the decoder is twelve lines and the crate
/// already carries the encoder for the image path.
fn decode_base64(text: &str) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        if byte == b'=' || byte == b'\n' {
            continue;
        }
        let Some(digit) = ALPHABET.iter().position(|candidate| *candidate == byte) else {
            continue;
        };
        buffer = (buffer << 6) | digit as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    out
}

fn write_response<S: Write>(stream: &mut S, status: u16, reason: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

fn read_request<S: Read>(stream: &mut S) -> Option<Recorded> {
    let mut raw = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let head_end;
    loop {
        let read = match stream.read(&mut chunk) {
            Ok(read) => read,
            // Only a zero-length read may end this loop early: WouldBlock
            // and Interrupted are retried, because the request is still on
            // its way - but they are retried after a yield. A non-blocking
            // socket that has nothing to give returns WouldBlock at once,
            // and an unyielding retry turns this reader into a spin that
            // burns a core for as long as the client is deciding.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                std::thread::yield_now();
                continue;
            }
            Err(_) => return None,
        };
        if read == 0 {
            return None;
        }
        raw.extend_from_slice(&chunk[..read]);
        if let Some(index) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            head_end = index;
            break;
        }
        if raw.len() > 1 << 24 {
            return None;
        }
    }

    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        if name == "content-length" {
            content_length = value.parse().unwrap_or(0);
        }
        headers.push((name, value));
    }

    let mut body = raw[head_end + 4..].to_vec();
    while body.len() < content_length {
        let read = match stream.read(&mut chunk) {
            Ok(read) => read,
            // Same reason as the head loop: a WouldBlock retried without a
            // yield spins for as long as the body is still being written.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                std::thread::yield_now();
                continue;
            }
            Err(_) => return None,
        };
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);

    Some(Recorded {
        method,
        path,
        headers,
        body,
    })
}
