// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! A fake DICOMweb origin, for the wire tests.
//!
//! A `TcpListener` on an ephemeral port that answers a scripted list of
//! responses and records the requests it received. It is deliberately
//! dumb: the tests assert on what clinfer SENT and drive what it RECEIVES,
//! and a fake that made its own decisions would be testing itself.
//!
//! Shared by the integration tests; compiled once.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

/// One request the fake received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    /// Lower-cased header names to values.
    pub headers: Vec<(String, String)>,
    /// The body as received. Kept as bytes because a retrieve request's
    /// body is a DICOM instance, not text.
    pub body_bytes: Vec<u8>,
}

impl Recorded {
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The request body, or `None` when its framing cannot be reduced to a
    /// plain body.
    ///
    /// A `multipart/related` Content-Type means the body is frames around
    /// parts rather than the object itself, so the part bytes are extracted
    /// here. That is what a server does on receipt, and a helper that
    /// returned the frames instead would hand a test the boundary markers as
    /// if they were DICOM, which parses as garbage rather than failing.
    ///
    /// Anything else is returned as it arrived: a `application/dicom` body is
    /// already the object, and a JSON body is already the document.
    #[must_use]
    pub fn unparted_body(&self) -> Option<Vec<u8>> {
        let multipart = self
            .header("content-type")
            .is_some_and(|value| value.starts_with("multipart/related"));
        if !multipart {
            return Some(self.body_bytes.clone());
        }
        // The boundary is a parameter of the header; the body's first line is
        // `--<boundary>`, and each part ends with `\r\n`.
        let boundary = self
            .header("content-type")
            .and_then(|value| value.split("boundary=").nth(1))
            .map(|value| value.trim_matches('"').trim())?
            .to_string();
        let open = format!("--{boundary}\r\n");
        let start = find_subslice(&self.body_bytes, open.as_bytes())? + open.len();
        // Skip the part's own headers, which end at the first blank line.
        let headers_end = find_subslice(&self.body_bytes[start..], b"\r\n\r\n")? + start;
        let value_start = headers_end + 4;
        let close = format!("\r\n--{boundary}");
        let value_end =
            find_subslice(&self.body_bytes[value_start..], close.as_bytes())? + value_start;
        Some(self.body_bytes[value_start..value_end].to_vec())
    }

    #[must_use]
    pub fn identity(&self) -> Option<&str> {
        self.header("x-authenticated-user")
    }

    /// The body as text, for the JSON calls.
    ///
    /// LOSSY, and only safe for the JSON calls: a DICOM object is binary,
    /// and every byte above 0x7F becomes U+FFFD here. Reading a stored
    /// object through this accessor makes a test assert on mojibake and
    /// still pass, which is why the binary path uses `body_bytes`.
    #[must_use]
    pub fn body(&self) -> String {
        String::from_utf8_lossy(&self.body_bytes).into_owned()
    }

    /// The body as received, for the calls that carry a DICOM object.
    #[must_use]
    pub fn dicom_body(&self) -> &[u8] {
        &self.body_bytes
    }
}

/// One scripted answer.
#[derive(Debug, Clone)]
pub struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
    pub content_type: String,
    /// When set, only this many body bytes are written before the socket is
    /// closed. The head still announces the FULL body length, which is what
    /// makes this indistinguishable on the wire from a connection dropped
    /// mid-write - the case a client must treat as a transport failure
    /// rather than as a short answer.
    pub close_after: Option<usize>,
    /// Extra header lines, verbatim, each already `Name: value`.
    pub extra_headers: Vec<String>,
}

impl Answer {
    #[must_use]
    pub fn ok(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into().into_bytes(),
            content_type: "application/dicom+json".to_string(),
            close_after: None,
            extra_headers: Vec::new(),
        }
    }

    #[must_use]
    pub fn status(status: u16) -> Self {
        Self {
            status,
            body: Vec::new(),
            content_type: "application/dicom+json".to_string(),
            close_after: None,
            extra_headers: Vec::new(),
        }
    }

    #[must_use]
    pub fn with(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into().into_bytes(),
            content_type: "application/dicom+json".to_string(),
            close_after: None,
            extra_headers: Vec::new(),
        }
    }

    /// The same answer, with one more header line.
    #[must_use]
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.extra_headers.push(format!("{name}: {value}"));
        self
    }

    /// The same answer, cut off after `sent` body bytes.
    ///
    /// The head is unchanged, so its `Content-Length` still names the whole
    /// body: a client that trusts the close to mean "the body ended" without
    /// comparing the two sees a short answer, which is exactly the failure
    /// this knob exists to reproduce.
    #[must_use]
    pub fn cut_short(self, sent: usize) -> Self {
        Self {
            close_after: Some(sent),
            ..self
        }
    }

    /// The same answer, with the extra header lines appended after the
    /// standard ones. Used to drive `Retry-After` and anything else a test
    /// needs the client to read.
    fn extra_header_block(&self) -> String {
        self.extra_headers
            .iter()
            .map(|line| format!("{line}\r\n"))
            .collect()
    }

    /// An answer carrying an instance, as a retrieve returns one.
    ///
    /// The body is a real multipart body with the boundary the header
    /// announces, because the reader takes the first part and a test that
    /// sent a bare body would not exercise that.
    #[must_use]
    pub fn multipart_dicom(dicom: &[u8]) -> Self {
        Self::multipart_dicom_labelled(dicom, "1.2.840.10008.1.2.1")
    }

    /// The same answer, with the part labelled as some other transfer
    /// syntax. An origin that cannot decompress stores what it has and
    /// answers with it, so the part header is where it says so.
    #[must_use]
    pub fn multipart_dicom_labelled(dicom: &[u8], transfer_syntax: &str) -> Self {
        const BOUNDARY: &str = "clinfer-test-boundary";
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Type: application/dicom; transfer-syntax={transfer_syntax}\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(dicom);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        Self {
            status: 200,
            body,
            content_type: format!(
                "multipart/related; type=\"application/dicom\"; boundary={BOUNDARY}"
            ),
            close_after: None,
            extra_headers: Vec::new(),
        }
    }

    /// An answer carrying a bare instance body, as a simpler origin
    /// returns one.
    #[must_use]
    pub fn dicom(dicom: &[u8]) -> Self {
        Self {
            status: 200,
            body: dicom.to_vec(),
            content_type: "application/dicom".to_string(),
            close_after: None,
            extra_headers: Vec::new(),
        }
    }

    /// The same as [`Self::multipart_dicom`] but with NO transfer-syntax
    /// parameter on the part.
    ///
    /// PS3.18 8.7.3 makes the parameter optional, so an origin that omits
    /// it is conformant - and the reader then has to establish the encoding
    /// from the dataset's own (0002,0010) rather than assuming.
    #[must_use]
    pub fn multipart_dicom_unlabelled(dicom: &[u8]) -> Self {
        const BOUNDARY: &str = "clinfer-test-boundary";
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Type: application/dicom\r\n\r\n");
        body.extend_from_slice(dicom);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        Self {
            status: 200,
            body,
            content_type: format!(
                "multipart/related; type=\"application/dicom\"; boundary={BOUNDARY}"
            ),
            close_after: None,
            extra_headers: Vec::new(),
        }
    }
}

/// A running fake origin.
pub struct FakeOrigin {
    port: u16,
    requests: Arc<Mutex<Vec<Recorded>>>,
    /// Answers still to be given, in order. When it runs dry the fake
    /// answers 200 with an empty body, which is a bug in the test rather
    /// than something to guess at.
    answers: Arc<Mutex<Vec<Answer>>>,
    _thread: thread::JoinHandle<()>,
    stop: Arc<Mutex<bool>>,
}

/// One route rule: when the request path starts with `starts_with`, answer
/// with `answer`.
///
/// A route table rather than a queue, for the tests where MORE THAN ONE
/// CALLER talks to the origin at the same time. A queue is an ordering, and
/// two concurrent children make the order they reach it in a race: the
/// second child would be served the first one's study row and the test would
/// fail for a reason that has nothing to do with what it asserts. A route
/// answers by what is being asked, which is what an origin does.
#[derive(Debug, Clone)]
pub struct Route {
    pub starts_with: String,
    pub answer: Answer,
}

impl FakeOrigin {
    /// Start a fake that answers by ROUTE rather than by order.
    ///
    /// The FIRST rule whose prefix matches the request path wins, so the
    /// caller orders its rules from most to least specific. A path that
    /// matches nothing gets 200 with an empty body - the same "bug in the
    /// test" answer the queue gives when it runs dry, and for the same
    /// reason.
    pub fn start_routed(routes: Vec<Route>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");

        let requests = Arc::new(Mutex::new(Vec::new()));
        let routes = Arc::new(routes);
        let stop = Arc::new(Mutex::new(false));

        let thread_requests = Arc::clone(&requests);
        let thread_routes = Arc::clone(&routes);
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            loop {
                if *thread_stop.lock().expect("stop lock") {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        handle_one_routed(stream, &thread_requests, &thread_routes);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });

        Self {
            port,
            requests,
            // The queue is unused in routed mode: an empty one makes the
            // accessor below answer "no scripted answers", which is true.
            answers: Arc::new(Mutex::new(Vec::new())),
            _thread: handle,
            stop,
        }
    }

    /// Start a fake with the given script.
    pub fn start(answers: Vec<Answer>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");

        let requests = Arc::new(Mutex::new(Vec::new()));
        let answers = Arc::new(Mutex::new(answers));
        let stop = Arc::new(Mutex::new(false));

        let thread_requests = Arc::clone(&requests);
        let thread_answers = Arc::clone(&answers);
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            loop {
                if *thread_stop.lock().expect("stop lock") {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        handle_one(stream, &thread_requests, &thread_answers);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });

        Self {
            port,
            requests,
            answers,
            _thread: handle,
            stop,
        }
    }

    /// The base URL to hand to a client.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/dicomweb", self.port)
    }

    /// Everything received so far.
    #[must_use]
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().expect("requests lock").clone()
    }

    /// The last request received, or `None`.
    #[must_use]
    pub fn last(&self) -> Option<Recorded> {
        self.requests.lock().expect("requests lock").last().cloned()
    }

    /// Append more answers, for a test that drives a second phase.
    pub fn push_answers(&self, more: Vec<Answer>) {
        self.answers.lock().expect("answers lock").extend(more);
    }
}

impl Drop for FakeOrigin {
    fn drop(&mut self) {
        *self.stop.lock().expect("stop lock") = true;
    }
}

fn handle_one(
    mut stream: TcpStream,
    requests: &Arc<Mutex<Vec<Recorded>>>,
    answers: &Arc<Mutex<Vec<Answer>>>,
) {
    // The listener is non-blocking so the accept loop can poll the stop
    // flag, and an accepted socket INHERITS that mode (verifiable on this
    // platform: the probe in the harness-flake report prints
    // `Err kind=WouldBlock raw=Some(10035)` for the first read). A read
    // timeout does nothing on a non-blocking socket, so without this line
    // the very first read can return WouldBlock under load and the
    // connection is torn down mid-request.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    requests.lock().expect("requests lock").push(request);

    let answer = {
        let mut queue = answers.lock().expect("answers lock");
        if queue.is_empty() {
            Answer::ok("{}")
        } else {
            queue.remove(0)
        }
    };

    respond(stream, answer);
}

/// The routed handler: the request PATH decides the answer.
///
/// Same read, same response, same drain as [`handle_one`]; the only
/// difference is where the answer comes from. The route table is a shared
/// immutable map that several connections read at once, which is what makes
/// it correct under concurrency where a queue would not be.
fn handle_one_routed(
    mut stream: TcpStream,
    requests: &Arc<Mutex<Vec<Recorded>>>,
    routes: &Arc<Vec<Route>>,
) {
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    // The METHOD matters as much as the path: a claim and a cancel both go
    // to `/workitems/{uid}/state`, and only the method (PUT either way) and
    // the BODY tell them apart. Routed tests therefore keep their state
    // answers last, and assert on the recorded requests rather than on a
    // per-call answer.
    let answer = routes
        .iter()
        .find(|route| request.path.starts_with(&route.starts_with))
        .map_or_else(|| Answer::ok("{}"), |route| route.answer.clone());
    requests.lock().expect("requests lock").push(request);

    respond(stream, answer);
}

/// Write one answer and drain the connection.
///
/// The drain is what keeps a client from seeing os error 10053: dropping a
/// `TcpStream` that still has unread bytes pending makes the stack send an
/// RST rather than a FIN. The read timeout set by the caller bounds it, so a
/// client that never closes cannot hold this handler open.
fn respond(mut stream: TcpStream, answer: Answer) {
    let reason = match answer.status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{}",
        answer.status,
        reason,
        answer.content_type,
        answer.body.len(),
        answer.extra_header_block()
    );
    let head = format!("{head}Connection: close\r\n\r\n");
    let _ = stream.write_all(head.as_bytes());
    match answer.close_after {
        // Announce the whole body, send only part of it, then close: a
        // connection dropped mid-write, byte for byte.
        Some(sent) => {
            let sent = sent.min(answer.body.len());
            let _ = stream.write_all(&answer.body[..sent]);
            let _ = stream.flush();
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return;
        }
        None => {
            let _ = stream.write_all(&answer.body);
            let _ = stream.flush();
        }
    }

    // Half-close the write side and then drain whatever the client still
    // has in flight.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut scratch = [0u8; 1024];
    loop {
        match stream.read(&mut scratch) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                // A non-blocking socket with nothing to give returns
                // WouldBlock at once; retrying without a yield spins.
                std::thread::yield_now();
                continue;
            }
            // A timeout or a reset ends the drain: the response is already
            // sent and nothing further can be usefully read.
            Err(_) => break,
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    let mut raw = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let head_end;
    loop {
        let read = match stream.read(&mut chunk) {
            Ok(read) => read,
            // A zero-length read is the client having closed its end, and
            // that is the only read result that may end this loop early.
            // WouldBlock and Interrupted are RETRIED rather than treated as
            // a closed connection: on a blocking socket they cannot occur,
            // and if they ever do (a future mode change, a signal) the
            // request is still on its way. The retry yields for the same
            // reason: an unyielding retry on a socket with nothing to give
            // is a spin, not a wait.
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
        if raw.len() > 1 << 20 {
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
            // Retried for the same reason as the head read above, and with
            // the same yield: the body may still be arriving.
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
        body_bytes: body,
    })
}

/// A counter the tests can wait on: it lets the first wire test block
/// until the fake has seen a request rather than sleeping and hoping.
pub fn wait_for<F: Fn() -> bool>(predicate: F, timeout: std::time::Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if predicate() {
            return true;
        }
        thread::sleep(std::time::Duration::from_millis(5));
    }
    predicate()
}

/// The offset of `needle` in `haystack`, or `None`.
///
/// A local copy rather than a dependency: the only substrate in the tree is
/// `windows().position()`, and this is that one line. It exists as a named
/// function because the multipart reader above asks the same question three
/// times with three different needles.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
