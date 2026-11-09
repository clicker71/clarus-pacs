// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Hand-rolled HTTP/1.1 client.
//!
//! DICOMweb requests here happen at the rate of workitems - a few per
//! study - so there is no pool, no keep-alive state machine and no
//! connection reuse. One request, one connection, `Connection: close`, and
//! the response read to EOF. That is the whole client, and it is the same
//! shape the server's own push uses.
//!
//! The layer returns the status and the body; deciding what a 409 means is
//! the caller's business.

use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// How long one request may take end to end.
///
/// Fixed rather than configurable: a workitem that cannot be claimed in
/// this budget is one the catch-up search will find again on the next
/// start, so there is nothing an operator would tune here.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// The largest response body accepted by default.
///
/// A QIDO response for one study is kilobytes. The bound exists so a
/// misdirected request (a base URL pointing at something that streams
/// endlessly) fails with a message rather than growing the process until
/// the box swaps.
const MAX_BODY: usize = 8 * 1024 * 1024;

/// The largest body accepted by an instance retrieve.
///
/// An instance is fetched WHOLE and carries its pixel data inline, so the
/// 8 MiB that bounds a JSON answer is the wrong number for it: a 2048x2048
/// 16-bit DX/CR/MG instance is 8 MiB of samples before any header, and
/// refusing it would keep working studies out of the queue. The ceiling is
/// per instance, not per slide, so 256 MiB bounds one object rather than
/// the process's working set; the worker holds one instance at a time.
/// PS3.18 10.4 (Retrieve Instance) defines no size ceiling, so the number
/// is ours and lives here to be changed in one place.
pub const MAX_INSTANCE_BODY: usize = 256 * 1024 * 1024;

/// How much body one request may read.
///
/// Passed by value into the read loop rather than reached for as a
/// constant: the caller knows what it asked for and therefore what a
/// plausible answer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyBudget(usize);

impl BodyBudget {
    /// The default: a JSON answer or a small dataset.
    pub const DEFAULT: Self = Self(MAX_BODY);

    /// One whole instance.
    pub const INSTANCE: Self = Self(MAX_INSTANCE_BODY);

    #[must_use]
    pub const fn bytes(self) -> usize {
        self.0
    }
}

/// Total attempts made for a retriable request, the first one included.
///
/// Not configurable, deliberately: a knob here would be tuned per site and
/// then hide the origin's real saturation. Three attempts ride out a burst
/// without turning a dead origin into a long stall.
pub const MAX_ATTEMPTS: u32 = 3;

/// The wait before retrying a request that failed at the transport level.
///
/// Fixed, and shorter than any `Retry-After` the origin sends, because the
/// two cases are different: a `503` says "busy, come back later" and the
/// origin's own hint is the right delay, while a dropped connection or an
/// incomplete body says nothing about how long to wait. A quarter second is
/// long enough to let a transient NAT or socket state clear and short enough
/// that a retry of a genuinely dead endpoint does not hold the worker thread
/// the queue needs back.
pub const BACKOFF: Duration = Duration::from_millis(250);

/// What an operator sees when a URL asks for TLS.
///
/// Shared by both URL parsers so the refusal reads the same wherever it is
/// met, and worded as an instruction rather than as a complaint: the site
/// that wants a cloud endpoint needs a local proxy, not a different build.
pub const HTTPS_MESSAGE: &str = "TLS is the perimeter's job: point base_url at a local nginx proxy \
     (see clinfer.conf.example.en)";

/// The longest the client will wait on a `Retry-After`, in seconds.
///
/// The origin's value is a hint, not a contract, and a wrong one (minutes)
/// would hold a worker thread that the queue needs back. Ten seconds is
/// longer than any plausible transcode; past it, giving up and letting the
/// next pass retry is the better use of the thread.
const MAX_RETRY_AFTER_SECS: u64 = 10;

/// The wait for one retry, from a `Retry-After` header.
///
/// Only the delay-seconds form is understood (RFC 9110 10.2.3). The
/// HTTP-date form is not parsed: the origin here is our own server and it
/// sends seconds, and a date parser would be a lot of code to serve a case
/// that does not occur. An absent or unparsable value waits one second -
/// longer than a tight loop and short enough not to matter.
fn retry_after_secs(header: Option<&str>) -> Duration {
    let seconds = header
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(1);
    Duration::from_secs(seconds.min(MAX_RETRY_AFTER_SECS))
}

/// How long a connect may take before the endpoint is treated as
/// unreachable.
///
/// Shorter than any request budget on purpose: a connect that has not
/// completed in this window is not going to, and a booth cannot afford to
/// discover an unreachable endpoint by timing out a whole completion -
/// still less by doing so once per study.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A transport failure, or a response clinfer refuses to interpret.
#[derive(Debug)]
pub enum HttpError {
    /// The connection or the exchange failed.
    Io(std::io::Error),
    /// The target address could not be resolved.
    Address(String),
    /// The response is not HTTP/1.x with a status line.
    Protocol(String),
    /// The response body ended before the origin said it would.
    ///
    /// A connection dropped mid-body looks exactly like this: the head
    /// arrived complete, its `Content-Length` promised more bytes than the
    /// socket delivered, and the read ended at EOF. That is a transport
    /// failure rather than an answer - the origin sent nothing to interpret
    /// and the same request may well succeed on a fresh connection - so it
    /// is kept apart from [`HttpError::Protocol`], whose parse refusals are
    /// the origin's own statement about the request and stay fatal.
    Truncated {
        /// The body length the head announced.
        announced: usize,
        /// The body length that actually arrived.
        received: usize,
    },
    /// The response body exceeded the request's [`BodyBudget`].
    TooLarge(usize),
    /// A redirect status: the client does not follow them, and silently
    /// reading the redirect body as if it were the answer would report a
    /// finding that does not exist.
    Redirect(u16),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "transport: {err}"),
            Self::Address(message) => write!(f, "address: {message}"),
            Self::Protocol(message) => write!(f, "protocol: {message}"),
            Self::Truncated {
                announced,
                received,
            } => write!(
                f,
                "transport: the origin announced {announced} body bytes and sent {received} \
                 before closing the connection"
            ),
            Self::TooLarge(limit) => {
                write!(f, "response larger than {} bytes", limit)
            }
            Self::Redirect(status) => {
                write!(f, "HTTP {status} redirect; the origin URL is wrong")
            }
        }
    }
}

impl std::error::Error for HttpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for HttpError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

pub type Result<T> = std::result::Result<T, HttpError>;

/// The method of a request. The vocabulary is the four DICOMweb uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

/// The content type of every DICOMweb JSON request and response.
pub const DICOM_JSON: &str = "application/dicom+json";

/// One request.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    /// Path including any query string, starting with `/`.
    pub path: String,
    /// Body, empty for GET and DELETE.
    pub body: Option<Vec<u8>>,
    pub content_type: Option<String>,
    /// Accept header, when the origin should be told what is expected.
    pub accept: Option<String>,
}

impl Request {
    #[must_use]
    pub fn get(path: impl Into<String>) -> Self {
        Self {
            method: Method::Get,
            path: path.into(),
            body: None,
            content_type: None,
            accept: None,
        }
    }

    #[must_use]
    pub fn json(method: Method, path: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            method,
            path: path.into(),
            body: Some(body),
            content_type: Some(DICOM_JSON.to_string()),
            accept: Some(DICOM_JSON.to_string()),
        }
    }

    #[must_use]
    pub fn accept(mut self, accept: impl Into<String>) -> Self {
        self.accept = Some(accept.into());
        self
    }
}

/// A response: the status, the headers and the raw body.
///
/// Headers are kept because one caller needs them: a retrieve response is
/// multipart, and the boundary that delimits its parts lives in the
/// Content-Type header rather than in the body.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// A header's value, case-insensitively, as every HTTP field name is.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The body as text, for the error message of a failed call.
    #[must_use]
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Where to send requests: the authority and base path parsed out of
/// `server.base_url`, plus the identity header value.
#[derive(Debug, Clone)]
pub struct Client {
    /// The authority as the operator wrote it, brackets included, so the
    /// `Host` header and the composed URLs match the configuration byte
    /// for byte.
    authority: String,
    host: String,
    port: u16,
    /// Base path with no trailing slash, e.g. `/dicomweb`.
    base: String,
    identity_user: String,
}

impl Client {
    /// Parse `http://host:port/base` and the identity to present.
    ///
    /// HTTPS is refused: clinfer has no TLS of its own, and the documented
    /// deployment is nginx terminating TLS in front of a loopback origin.
    /// Accepting the scheme and then sending plaintext would be worse than
    /// refusing it.
    pub fn new(base_url: &str, identity_user: &str) -> Result<Self> {
        let (host, port, base, authority) = parse_url(base_url)?;
        Ok(Self {
            authority,
            host,
            port,
            base,
            identity_user: identity_user.to_string(),
        })
    }

    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}{}", self.authority, self.base)
    }

    /// The `(host, port, base path)` the client sends to.
    ///
    /// The WebSocket channel opens a raw TCP connection rather than going
    /// through [`Client::send`], so it needs the same three pieces. Exposing
    /// them from the one place that parsed `base_url` keeps the handshake's
    /// target and the HTTP target from drifting apart.
    #[must_use]
    pub fn target(&self) -> (String, u16, String) {
        (self.host.clone(), self.port, self.base.clone())
    }

    /// The identity presented on every request.
    #[must_use]
    pub fn identity_user(&self) -> &str {
        &self.identity_user
    }

    /// The absolute path of a resource under the base URL.
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Perform one request.
    pub fn send(&self, request: &Request) -> Result<Response> {
        self.send_within(request, BodyBudget::DEFAULT)
    }

    /// Perform one request whose response may be as large as `budget`.
    ///
    /// The budget is a parameter rather than a field on [`Client`] because
    /// it describes the REQUEST, not the origin: the same client reads a
    /// QIDO answer and a whole instance within the same second.
    pub fn send_within(&self, request: &Request, budget: BodyBudget) -> Result<Response> {
        let mut stream = self.connect()?;
        let head = self.head(request);
        stream.write_all(head.as_bytes())?;
        if let Some(body) = &request.body {
            stream.write_all(body)?;
        }
        stream.flush()?;
        let raw = read_response_from(&mut stream, budget)?;
        parse_response(&raw)
    }

    /// Perform one request, retrying a busy origin or a transport failure
    /// under a fixed budget.
    ///
    /// Two failure classes are retried, and they are the same class seen
    /// from two sides: the origin is momentarily unable to answer. A `503`
    /// with `Retry-After` is the origin saying so in words - the transcode
    /// pool is saturated - and the header's own delay is honoured. An
    /// [`HttpError::Io`] (timeout, reset, refused) or an
    /// [`HttpError::Truncated`] body is the transport saying so by dropping
    /// or cutting the exchange, and the fixed [`BACKOFF`] applies because
    /// nothing in the exchange carries a hint.
    ///
    /// Any other outcome stays fatal on the first attempt. A `404`, `406` or
    /// `409`, a parse refusal, and a transfer-syntax refusal are all the
    /// origin's ANSWERS about the request: re-sending produces the same
    /// answer, and a retry would only delay the report by a backoff.
    ///
    /// Only the calls that can race a busy origin or a transient network
    /// gap use this: fetching an instance, storing a result, and the UPS
    /// state changes. The budget is fixed at [`MAX_ATTEMPTS`] attempts
    /// including the first - the same no-knob policy family the server's
    /// `push_attempts` uses. Every attempt runs on a fresh connection
    /// ([`Self::connect`]), so a wedged socket is abandoned rather than
    /// reused, and a copied GET is safe to re-send because GET is
    /// idempotent.
    ///
    /// `attempts` reports how many requests were actually sent, so a caller
    /// can count the retries it paid for. It is written even when the call
    /// fails: the count is what the caller is asking about, not the outcome.
    pub fn send_retrying(&self, request: &Request, attempts: &mut u32) -> Result<Response> {
        self.send_retrying_within(request, attempts, BodyBudget::DEFAULT)
    }

    /// [`Self::send_retrying`] for a response as large as `budget`.
    pub fn send_retrying_within(
        &self,
        request: &Request,
        attempts: &mut u32,
        budget: BodyBudget,
    ) -> Result<Response> {
        let mut attempt = 1;
        loop {
            *attempts = attempt;
            let last = attempt >= MAX_ATTEMPTS;
            let wait = match self.send_within(request, budget) {
                // The origin's own hint is the right delay for its own
                // saturation, so a 503 uses `Retry-After`.
                Ok(response) if response.status == 503 && !last => {
                    retry_after_secs(response.header("retry-after"))
                }
                // Any other status - and a 503 on the last attempt - is an
                // answer, not a busy origin.
                Ok(response) => return Ok(response),
                // A dropped or cut exchange carries no hint, so the fixed
                // backoff applies.
                Err(err @ (HttpError::Io(_) | HttpError::Truncated { .. })) => {
                    if last {
                        return Err(err);
                    }
                    BACKOFF
                }
                // Anything else is the origin's word about the request and
                // stays fatal on the first attempt.
                Err(err) => return Err(err),
            };
            std::thread::sleep(wait);
            attempt += 1;
        }
    }

    /// GET a resource and parse its JSON body.
    pub fn get_json(&self, path: &str) -> Result<(u16, Vec<u8>)> {
        let response = self.send(&Request::get(self.url(path)))?;
        Ok((response.status, response.body))
    }

    fn connect(&self) -> Result<TcpStream> {
        let address = format!("{}:{}", self.host, self.port);
        let resolved = address
            .to_socket_addrs()
            .map_err(|err| HttpError::Address(format!("{address}: {err}")))?
            .next()
            .ok_or_else(|| HttpError::Address(format!("{address}: no address")))?;
        let stream = TcpStream::connect_timeout(&resolved, TIMEOUT)?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        Ok(stream)
    }

    fn head(&self, request: &Request) -> String {
        let mut head = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nX-Authenticated-User: {}\r\nConnection: close\r\n",
            request.method.as_str(),
            request.path,
            self.authority,
            self.identity_user
        );
        if let Some(content_type) = &request.content_type {
            head.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        if let Some(accept) = &request.accept {
            head.push_str(&format!("Accept: {accept}\r\n"));
        }
        head.push_str(&format!(
            "Content-Length: {}\r\n\r\n",
            request.body.as_ref().map_or(0, Vec::len)
        ));
        head
    }
}

/// Split `http://host:port/path` into its parts, keeping the authority as
/// written so an IPv6 literal round-trips with its brackets.
fn parse_url(url: &str) -> Result<(String, u16, String, String)> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        if url.starts_with("https://") {
            HttpError::Address(HTTPS_MESSAGE.to_string())
        } else {
            HttpError::Address(format!("expected an http:// URL, got '{url}'"))
        }
    })?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    // A bracketed IPv6 literal carries its own colons.
    let (host, port) = if let Some(after) = authority.strip_prefix('[') {
        let close = after
            .find(']')
            .ok_or_else(|| HttpError::Address(format!("unterminated IPv6 literal in '{url}'")))?;
        let host = after[..close].to_string();
        let port = match after[close + 1..].strip_prefix(':') {
            Some(text) => text
                .parse()
                .map_err(|_| HttpError::Address(format!("invalid port in '{url}'")))?,
            None => 80,
        };
        (host, port)
    } else {
        match authority.rfind(':') {
            Some(index) => {
                let port = authority[index + 1..]
                    .parse()
                    .map_err(|_| HttpError::Address(format!("invalid port in '{url}'")))?;
                (authority[..index].to_string(), port)
            }
            None => (authority.to_string(), 80),
        }
    };
    if host.is_empty() {
        return Err(HttpError::Address(format!("empty host in '{url}'")));
    }
    if port == 0 {
        return Err(HttpError::Address(format!("port 0 in '{url}'")));
    }
    Ok((
        host,
        port,
        path.trim_end_matches('/').to_string(),
        authority.to_string(),
    ))
}

/// Read until the peer closes. `Connection: close` makes EOF the end of
/// the body, so no framing is needed and a chunked response decodes for
/// free.
///
/// Generic over the stream so the read loop is written once: the caller
/// passes the socket, and duplicating the loop would be duplicating the
/// budget guard.
fn read_response_from<S: Read>(stream: &mut S, budget: BodyBudget) -> Result<Vec<u8>> {
    let mut raw = Vec::with_capacity(1024);
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if raw.len() + n > budget.bytes() {
                    return Err(HttpError::TooLarge(budget.bytes()));
                }
                raw.extend_from_slice(&chunk[..n]);
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(HttpError::Io(err)),
        }
    }
    if raw.is_empty() {
        // Nothing at all arrived. That is a dropped connection rather than a
        // refusal the origin worded, so it takes the transport-failure path
        // and is retried; a response head that is present but malformed is
        // still a `Protocol` refusal and stays fatal.
        return Err(HttpError::Truncated {
            announced: 0,
            received: 0,
        });
    }
    Ok(raw)
}

/// Split a raw response into status and body.
fn parse_response(raw: &[u8]) -> Result<Response> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| HttpError::Protocol("no header terminator".to_string()))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let status_line = head
        .lines()
        .next()
        .ok_or_else(|| HttpError::Protocol("no status line".to_string()))?;
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(HttpError::Protocol(format!(
            "unexpected version '{version}'"
        )));
    }
    let status: u16 = parts
        .next()
        .ok_or_else(|| HttpError::Protocol("no status code".to_string()))?
        .parse()
        .map_err(|_| HttpError::Protocol("status code is not a number".to_string()))?;
    if (300..400).contains(&status) {
        return Err(HttpError::Redirect(status));
    }

    let body = &raw[split + 4..];
    // `Transfer-Encoding: chunked` is decoded here rather than at the
    // caller: the read loop collects the raw stream, and a chunked body
    // that reached JSON parsing would fail with a message about braces.
    let chunked = head.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.starts_with("transfer-encoding:") && lower.contains("chunked")
    });
    let headers = parse_headers(&head);
    let body = if chunked {
        dechunk(body)?
    } else {
        // The body is read to EOF, which is what makes a lying SHORT
        // Content-Length harmless (a chunked or connection-close body is
        // delimited by the transfer itself). The other direction is not
        // harmless: a body shorter than the length the head announced means
        // the connection closed before the origin finished writing, and the
        // bytes in hand are a prefix of an instance rather than the
        // instance. Returning them would hand the reader a truncated object
        // that fails much later, or worse, parses as a short one.
        if let Some(announced) = content_length(&headers)
            && body.len() < announced
        {
            return Err(HttpError::Truncated {
                announced,
                received: body.len(),
            });
        }
        body.to_vec()
    };
    Ok(Response {
        status,
        headers,
        body,
    })
}

/// The `Content-Length` the head declared, when it is a usable number.
///
/// A repeated or unparsable value yields `None`: such a response has no
/// trustworthy length to compare against, so it is left to the transfer
/// framing rather than refused on a header that cannot mean what it says.
fn content_length(headers: &[(String, String)]) -> Option<usize> {
    let mut found = None;
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = value.trim().parse::<usize>().ok();
    }
    found
}

/// Parse the header field lines of a response head.
///
/// A field may be folded across continuation lines, which the standard
/// allows and which an origin that writes long boundaries will use; the
/// continuation is unfolded into the value rather than dropped.
fn parse_headers(head: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in head.lines().skip(1) {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, value)) = out.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            out.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    out
}

/// Decode a chunked body.
fn dechunk(body: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        let line_end = body[pos..]
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| HttpError::Protocol("truncated chunk header".to_string()))?
            + pos;
        let header = String::from_utf8_lossy(&body[pos..line_end]);
        // A chunk extension follows a semicolon and is not part of the size.
        let size_text = header.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| HttpError::Protocol(format!("bad chunk size '{size_text}'")))?;
        pos = line_end + 2;
        if size == 0 {
            return Ok(out);
        }
        if pos + size > body.len() {
            return Err(HttpError::Protocol("truncated chunk".to_string()));
        }
        out.extend_from_slice(&body[pos..pos + size]);
        pos += size;
        // The chunk's own trailing CRLF. The last chunk is followed by one
        // as well, and swallowing a half CRLF is how a JSON body ends up
        // with a stray '\r' that the parser then reports as trailing
        // content.
        if body[pos..].starts_with(b"\r\n") {
            pos += 2;
        } else if body[pos..].starts_with(b"\r") {
            pos += 1;
        }
    }
}

// -- Outbound requests to a non-DICOMweb server -------------------------

/// An absolute `http://` URL split into the parts the transport needs.
///
/// There is no scheme field: the only endpoint this serves is a local model
/// server reached in cleartext on loopback, and the configuration refuses
/// anything else before a request is ever built. Carrying a second scheme
/// here would mean carrying a second byte-stream implementation - the
/// certificate stack the product deliberately keeps out of its binaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    /// Origin-form request target, starting with `/`.
    pub path: String,
}

impl Target {
    /// Split `http://host:port/path`.
    ///
    /// No userinfo, fragment or query is parsed: an endpoint's base URL has
    /// none of them, and accepting one silently would make the request
    /// target something the operator did not write.
    ///
    /// `https` is refused by name rather than by a generic scheme error:
    /// the operator who wrote it was reaching for a cloud endpoint, and the
    /// message has to say what to do instead.
    pub fn parse(url: &str) -> Result<Self> {
        let rest = url.strip_prefix("http://").ok_or_else(|| {
            if url.starts_with("https://") {
                return HttpError::Address(HTTPS_MESSAGE.to_string());
            }
            HttpError::Address(format!(
                "'{url}' is not an http:// URL; a model endpoint is reached over \
                 cleartext on loopback"
            ))
        })?;
        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, "/"),
        };
        // A bracketed IPv6 literal carries its own colons.
        let (host, port) = if let Some(after) = authority.strip_prefix('[') {
            let close = after.find(']').ok_or_else(|| {
                HttpError::Address(format!("unterminated IPv6 literal in '{url}'"))
            })?;
            let host = after[..close].to_string();
            let port = match after[close + 1..].strip_prefix(':') {
                Some(text) => text
                    .parse()
                    .map_err(|_| HttpError::Address(format!("invalid port in '{url}'")))?,
                None => 80,
            };
            (host, port)
        } else {
            match authority.rsplit_once(':') {
                Some((host, text)) => (
                    host.to_string(),
                    text.parse()
                        .map_err(|_| HttpError::Address(format!("invalid port in '{url}'")))?,
                ),
                None => (authority.to_string(), 80),
            }
        };
        if host.is_empty() {
            return Err(HttpError::Address(format!("empty host in '{url}'")));
        }
        if port == 0 {
            return Err(HttpError::Address(format!("port 0 in '{url}'")));
        }
        Ok(Self {
            host,
            port,
            path: path.to_string(),
        })
    }

    /// The `Host` header value: the authority, with the port omitted when
    /// it is the default.
    ///
    /// A host that contains a colon is an IPv6 literal and must be
    /// re-bracketed: the brackets belong to the authority and not to the
    /// address, so once they are stripped the colons become ambiguous and
    /// a naive `host:port` reads the port as an address section.
    #[must_use]
    pub fn authority(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == 80 {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

/// A request to an absolute URL, for the remote model endpoint.
///
/// [`Client`] is the DICOMweb client: it carries the origin's base URL and
/// the identity header, and every path it builds is relative to that
/// origin. A model endpoint is a different server with a different
/// protocol (a bearer token, a JSON body, no identity header), so it gets
/// its own small type rather than a flag on `Client` that would make every
/// DICOMweb call carry an endpoint's concerns.
///
/// The transport underneath is the same `TcpStream` read loop and the same
/// response parser as the DICOMweb side. There is no TLS branch: an
/// endpoint this type reaches is a local server behind the site's own
/// perimeter.
#[derive(Debug, Clone)]
pub struct Outbound {
    target: Target,
    authorization: String,
    body: Vec<u8>,
    content_type: String,
    accept: String,
    timeout: Duration,
}

impl Outbound {
    /// A JSON POST to an absolute URL with a bearer token.
    ///
    /// The timeout is per request and comes from the model's own
    /// `timeout_secs`: a completion is not a DICOMweb call, and a fixed
    /// budget would either cut off a slow endpoint or hang a booth.
    #[must_use]
    pub fn post_json(
        target: Target,
        authorization: &str,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Self {
        Self {
            target,
            authorization: authorization.to_string(),
            body,
            content_type: "application/json".to_string(),
            accept: "application/json".to_string(),
            timeout,
        }
    }

    /// Send the request and read the response.
    ///
    /// The connect and read budgets are the same value: a completion's
    /// total budget is what the configuration declares, and splitting it
    /// into a connect part and a body part would make the declared number
    /// mean two different things.
    pub fn send(&self) -> Result<Response> {
        let mut stream = self.connect()?;
        self.exchange(&mut stream)
    }

    fn connect(&self) -> Result<TcpStream> {
        let address = format!("{}:{}", self.target.host, self.target.port);
        let resolved = address
            .to_socket_addrs()
            .map_err(|err| HttpError::Address(format!("{address}: {err}")))?
            .next()
            .ok_or_else(|| HttpError::Address(format!("{address}: no address")))?;
        // The connect gets its own short budget while the exchange gets the
        // configured one: an unreachable endpoint must fail fast, and a
        // slow completion must not be cut off at the same number.
        let stream = TcpStream::connect_timeout(&resolved, CONNECT_TIMEOUT.min(self.timeout))?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        Ok(stream)
    }

    /// Write the request and read the response.
    fn exchange<S: Read + Write>(&self, stream: &mut S) -> Result<Response> {
        let head = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nAuthorization: {}\r\n\
             Content-Type: {}\r\nAccept: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.target.path,
            self.target.authority(),
            self.authorization,
            self.content_type,
            self.accept,
            self.body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(&self.body)?;
        stream.flush()?;
        // A completion is a JSON document whose length the model's own
        // output limit bounds; the default budget is already generous for
        // it and stays the one number.
        let raw = read_response_from(stream, BodyBudget::DEFAULT)?;
        parse_response(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(raw: &str) -> Result<Response> {
        parse_response(raw.as_bytes())
    }

    #[test]
    fn a_url_splits_into_host_port_and_base_path() {
        let client = Client::new("http://127.0.0.1:8024/dicomweb", "clinfer").expect("parses");
        assert_eq!(client.base_url(), "http://127.0.0.1:8024/dicomweb");
        // `url` is the request target, i.e. what goes on the request line
        // after the method: origin-form, not an absolute URI.
        assert_eq!(client.url("/workitems"), "/dicomweb/workitems");
    }

    #[test]
    fn a_url_without_a_port_or_path_gets_the_defaults() {
        let client = Client::new("http://pacs", "clinfer").expect("parses");
        // The authority is echoed as written; the port is 80 either way,
        // and the Host header carries the operator's spelling.
        assert_eq!(client.base_url(), "http://pacs");
        assert_eq!(client.url("/workitems"), "/workitems");
        let head = client.head(&Request::get("/workitems"));
        assert!(head.contains("Host: pacs\r\n"), "{head}");
    }

    #[test]
    fn a_trailing_slash_does_not_double_up() {
        let client = Client::new("http://x:80/dicomweb/", "clinfer").expect("parses");
        assert_eq!(client.base_url(), "http://x:80/dicomweb");
    }

    #[test]
    fn a_bracketed_ipv6_literal_keeps_its_colons() {
        let client = Client::new("http://[::1]:8024/dicomweb", "clinfer").expect("parses");
        assert_eq!(client.base_url(), "http://[::1]:8024/dicomweb");
    }

    #[test]
    fn https_is_refused_rather_than_downgraded() {
        let err = Client::new("https://pacs/dicomweb", "clinfer").expect_err("refuses");
        let text = err.to_string();
        assert!(text.contains("TLS"), "the rule is stated: {text}");
        assert!(text.contains("nginx"), "the remedy is named: {text}");
    }

    #[test]
    fn a_url_that_is_not_http_is_refused() {
        assert!(Client::new("ftp://pacs", "x").is_err());
        assert!(Client::new("pacs:8024", "x").is_err());
        assert!(Client::new("http://pacs:0", "x").is_err());
        assert!(Client::new("http://[::1/dicomweb", "x").is_err());
        assert!(Client::new("http://", "x").is_err());
    }

    #[test]
    fn the_request_carries_the_identity_and_the_body_length() {
        let client = Client::new("http://127.0.0.1:8024/dicomweb", "clinfer").expect("parses");
        let request = Request::json(Method::Put, "/workitems/1.2.3", b"{}".to_vec());
        let head = client.head(&request);
        assert!(
            head.starts_with("PUT /workitems/1.2.3 HTTP/1.1\r\n"),
            "{head}"
        );
        assert!(head.contains("Host: 127.0.0.1:8024\r\n"), "{head}");
        assert!(head.contains("X-Authenticated-User: clinfer\r\n"), "{head}");
        assert!(
            head.contains("Content-Type: application/dicom+json\r\n"),
            "{head}"
        );
        assert!(head.contains("Content-Length: 2\r\n"), "{head}");
        assert!(head.ends_with("\r\n\r\n"), "{head}");

        // A GET carries no content type and a zero length.
        let get = client.head(&Request::get("/workitems"));
        assert!(get.contains("Content-Length: 0\r\n"), "{get}");
        assert!(!get.contains("Content-Type"), "{get}");
    }

    #[test]
    fn a_response_splits_into_status_and_body() {
        let parsed = response("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}").expect("parses");
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.body_text(), "{}");
        assert!(parsed.is_success());
    }

    #[test]
    fn a_conflict_is_returned_rather_than_treated_as_an_error() {
        let parsed =
            response("HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\n\r\n").expect("parses");
        assert_eq!(parsed.status, 409);
        assert!(!parsed.is_success());
    }

    #[test]
    fn a_chunked_body_is_decoded() {
        let raw = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
3\r\n{\"a\r\n2\r\n\":\r\n2\r\n1}\r\n0\r\n\r\n";
        let parsed = response(raw).expect("parses");
        assert_eq!(parsed.body_text(), "{\"a\":1}");
    }

    #[test]
    fn a_chunked_body_with_an_extension_decodes_too() {
        let raw = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2;ext=1\r\nok\r\n0\r\n\r\n";
        let parsed = response(raw).expect("parses");
        assert_eq!(parsed.body_text(), "ok");
    }

    #[test]
    fn a_truncated_chunk_is_refused() {
        let raw = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nshort";
        assert!(response(raw).is_err());
    }

    #[test]
    fn a_redirect_is_refused_rather_than_followed() {
        let err = response("HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\n\r\n").expect_err("302");
        assert!(err.to_string().contains("redirect"), "{err}");
    }

    #[test]
    fn a_malformed_response_is_refused() {
        assert!(response("not http at all\r\n\r\n").is_err());
        assert!(response("HTTP/1.1\r\n\r\n").is_err());
        assert!(response("HTTP/1.1 abc OK\r\n\r\n").is_err());
        assert!(response("HTTP/1.1 200 OK\r\n").is_err());
    }

    #[test]
    fn a_body_shorter_than_the_announced_length_is_a_transport_failure() {
        // The origin announced 99 bytes and closed after 13: the body in
        // hand is a PREFIX of the object, not the object. Reading to EOF
        // still gives the right bytes for a body that is complete and
        // simply mislabelled, but a body that stops short of its own
        // Content-Length means the connection ended early, and returning
        // the prefix would hand a reader a truncated instance.
        let raw = "HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\n{\"real\":true}";
        let err = response(raw).expect_err("a short body is a transport failure");
        assert!(
            matches!(
                err,
                HttpError::Truncated {
                    announced: 99,
                    received: 13
                }
            ),
            "{err}"
        );
        assert!(err.to_string().contains("99"), "{err}");
    }

    #[test]
    fn a_body_that_matches_the_announced_length_is_taken() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\n{\"real\":true}";
        let parsed = response(raw).expect("an exactly sized body parses");
        assert_eq!(parsed.body_text(), "{\"real\":true}");
    }

    #[test]
    fn a_body_longer_than_the_announced_length_is_still_taken() {
        // Read to EOF: an origin that under-declares its length still gets
        // its whole body read, because nothing is lost by carrying on to
        // the close. Over-declaring is the case that loses data, and that
        // one is refused by the test above.
        let raw = "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n{\"real\":true}";
        let parsed = response(raw).expect("an over-long body parses");
        assert_eq!(parsed.body_text(), "{\"real\":true}");
    }

    #[test]
    fn a_chunked_body_short_circuits_the_content_length_check() {
        // A chunked body is delimited by its chunks, and its Content-Length
        // is not even defined; the check must not fire on one. The chunk
        // below is complete and the header would say otherwise.
        let raw = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 99\r\n\r\n\
                   2\r\nok\r\n0\r\n\r\n";
        let parsed = response(raw).expect("a chunked body is delimited by its chunks");
        assert_eq!(parsed.body_text(), "ok");
    }

    #[test]
    fn an_empty_response_is_a_truncated_exchange() {
        // Nothing at all arrived: a dropped connection, not a malformed
        // head. It must take the transport path so it is retried. The check
        // lives in the read loop, which is where a connection that sent
        // nothing is distinguishable from one that sent a bad head.
        let mut empty: &[u8] = b"";
        let err = read_response_from(&mut empty, BodyBudget::DEFAULT)
            .and_then(|raw| parse_response(&raw))
            .expect_err("an empty response is a transport failure");
        assert!(
            matches!(
                err,
                HttpError::Truncated {
                    announced: 0,
                    received: 0
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn a_retry_after_in_seconds_is_honoured() {
        assert_eq!(retry_after_secs(Some("2")), Duration::from_secs(2));
        assert_eq!(retry_after_secs(Some("0")), Duration::from_secs(0));
        assert_eq!(retry_after_secs(Some(" 3 ")), Duration::from_secs(3));
    }

    #[test]
    fn a_retry_after_is_capped_so_a_worker_thread_comes_back() {
        // A value larger than the cap is clamped, not obeyed.
        assert_eq!(retry_after_secs(Some("600")), Duration::from_secs(10));
        assert_eq!(retry_after_secs(Some("11")), Duration::from_secs(10));
        assert_eq!(retry_after_secs(Some("10")), Duration::from_secs(10));
    }

    // -- Outbound targets -------------------------------------------------

    #[test]
    fn an_outbound_target_keeps_its_host_port_and_path() {
        let target = Target::parse("http://127.0.0.1:8081/v1/chat/completions").expect("parses");
        assert_eq!(target.host, "127.0.0.1");
        assert_eq!(target.port, 8081);
        assert_eq!(target.path, "/v1/chat/completions");
    }

    #[test]
    fn an_outbound_target_defaults_to_port_eighty() {
        assert_eq!(Target::parse("http://x/").expect("parses").port, 80);
        // An explicit port wins over the default.
        assert_eq!(Target::parse("http://x:8081/").expect("parses").port, 8081);
    }

    #[test]
    fn the_host_header_omits_only_the_default_port() {
        assert_eq!(Target::parse("http://x/").expect("p").authority(), "x");
        assert_eq!(
            Target::parse("http://x:8443/").expect("p").authority(),
            "x:8443"
        );
        assert_eq!(
            Target::parse("http://127.0.0.1:8081/")
                .expect("p")
                .authority(),
            "127.0.0.1:8081"
        );
    }

    #[test]
    fn an_outbound_target_without_a_path_gets_the_root() {
        let target = Target::parse("http://127.0.0.1:8080").expect("parses");
        assert_eq!(target.path, "/");
    }

    #[test]
    fn a_bracketed_ipv6_outbound_target_round_trips() {
        let target =
            Target::parse("http://[::1]:8080/v1/chat/completions").expect("parses the literal");
        assert_eq!(target.host, "::1");
        assert_eq!(target.port, 8080);
        assert_eq!(target.authority(), "[::1]:8080");
    }

    #[test]
    fn an_outbound_https_target_is_refused_with_the_remedy_named() {
        // The operator who wrote `https://` wanted a cloud endpoint. The
        // message must not be a generic scheme complaint: it has to send
        // them to the proxy that makes the deployment work.
        let err =
            Target::parse("https://api.example.test/v1/chat/completions").expect_err("refuses");
        let text = err.to_string();
        assert!(text.contains("TLS"), "the rule is stated: {text}");
        assert!(text.contains("nginx"), "the remedy is named: {text}");
    }

    #[test]
    fn an_outbound_target_that_is_not_http_is_refused() {
        for url in [
            "ftp://x/y",
            "file:///etc/passwd",
            "api.example.test/v1/chat",
            "x:443/y",
            "ws://x/y",
        ] {
            let err = Target::parse(url).expect_err("refuses");
            assert!(!err.to_string().is_empty(), "{url}");
        }
    }

    #[test]
    fn an_outbound_target_with_no_host_or_a_zero_port_is_refused() {
        assert!(Target::parse("http:///v1/chat").is_err());
        assert!(Target::parse("http://x:0/v1/chat").is_err());
        assert!(Target::parse("http://x:notaport/v1/chat").is_err());
        assert!(Target::parse("http://[::1/v1").is_err());
    }

    #[test]
    fn the_connect_budget_is_shorter_than_a_completion_budget() {
        // The whole point of the separate constant: an unreachable
        // endpoint fails in seconds, and a slow completion still gets the
        // minutes its configuration declared.
        assert!(CONNECT_TIMEOUT < Duration::from_secs(30));
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(10));
    }

    #[test]
    fn an_absent_or_unparsable_retry_after_waits_one_second() {
        // The HTTP-date form is deliberately unsupported; it falls back to
        // the default rather than being misread as a number.
        assert_eq!(retry_after_secs(None), Duration::from_secs(1));
        assert_eq!(retry_after_secs(Some("")), Duration::from_secs(1));
        assert_eq!(retry_after_secs(Some("soon")), Duration::from_secs(1));
        assert_eq!(
            retry_after_secs(Some("Wed, 21 Oct 2026 07:28:00 GMT")),
            Duration::from_secs(1)
        );
        assert_eq!(retry_after_secs(Some("-3")), Duration::from_secs(1));
    }
}
