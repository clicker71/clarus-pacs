// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Connection loop + keep-alive (E-006).
//!
//! One OS thread per connection, blocking sockets. Socket timeouts
//! (`SO_RCVTIMEO`/`SO_SNDTIMEO`) are applied ONCE per phase in [`Conn::read_head`]
//! (slowloris protection); the write path never re-applies them (A3). The
//! request head buffer is owned by the caller (a stack array in the connection
//! loop), so parsing borrows never alias the `Conn` (A2).

// -- Hard constants (in code, never config - they are protocol limits) ---
/// Per-connection thread stack size.
pub const STACK_SIZE: usize = 512 * 1024;
/// Maximum number of request headers.
///
/// Chromium behind nginx sends about 17 fields (the browser set plus the
/// proxy's `X-Real-IP`/`X-Forwarded-*`/`Via`/`Connection`), so a 16-field cap
/// rejected the embedded viewer with a 400 while every header was well within
/// the head buffer. 64 is the room a browser-and-proxy head actually needs
/// with margin; the real work bound is unchanged, because
/// [`HEADER_BUF_SIZE`] and [`HTTP_LINE_LIMIT`] still cap the bytes and the
/// length of each line, so a flood cannot buy more work than before.
pub const MAX_HEADERS: usize = 64;
/// Request head buffer (request line + all headers).
pub const HEADER_BUF_SIZE: usize = 8 * 1024;
/// Maximum request-target (URI) length.
pub const URI_LEN: usize = 4 * 1024;
/// Maximum multipart boundary length (RFC 2046: <= 70).
pub const MAX_BOUNDARY_LEN: usize = 72;
/// Maximum chunked trailer buffer.
pub const TRAILER_LIMIT: usize = 8 * 1024;
/// Body read buffer for streaming (multipart / STOW).
pub const READ_BUF: usize = 64 * 1024;
/// Maximum chunk-size line length (hex digits).
pub const CHUNK_LINE: usize = 16;
/// Per-line cap inside the head (slowloris guard: 4 KB).
pub const HTTP_LINE_LIMIT: usize = 4 * 1024;

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// First index of `needle` inside `hay`, if present.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Socket-level timeout values applied per phase.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub header: Duration,
    pub body: Duration,
    pub keepalive: Duration,
    pub send: Duration,
}

impl Timeouts {
    pub fn from_secs(header: u64, body: u64, keepalive: u64, send: u64) -> Self {
        Self {
            header: Duration::from_secs(header),
            body: Duration::from_secs(body),
            keepalive: Duration::from_secs(keepalive),
            send: Duration::from_secs(send),
        }
    }
}

/// Set the socket receive/send timeouts.
///
/// `set_read_timeout`/`set_write_timeout` are `setsockopt(SO_RCVTIMEO)` and
/// `setsockopt(SO_SNDTIMEO)` on unix, so the std spelling is the same call we
/// made by hand - and it returns the error instead of discarding it, which is
/// the difference that matters here: a timeout that was never applied turns a
/// stalled peer into a permanent parked worker rather than an idle phase.
///
/// A zero duration is rejected by std (`Duration::ZERO` cannot be represented as
/// "wait forever" through this API, only as "block"); callers derive the value
/// from `[server] *_timeout_secs`, whose parser has a floor of 1.
pub fn set_timeouts(stream: &TcpStream, recv: Duration, send: Duration) -> io::Result<()> {
    stream.set_read_timeout(Some(recv))?;
    stream.set_write_timeout(Some(send))?;
    Ok(())
}

/// Outcome of one [`Conn::read_head_from`] attempt.
///
/// `Idle` carries the byte count already in the caller's buffer so a poll
/// slice that expired mid-head can be resumed without dropping those bytes.
pub enum HeadRead {
    /// The head terminator was found; the value is the head length.
    Complete(usize),
    /// The peer closed mid-head (the value is the byte count read so far).
    Eof(usize),
    /// The poll slice expired (the value is the byte count read so far).
    Idle(usize),
}

/// A per-connection worker. Owns the socket plus the reusable response
/// buffers. The REQUEST head buffer is caller-owned (a stack array in the
/// connection loop) - see [`Conn::read_head`].
pub struct Conn {
    stream: TcpStream,
    /// Response header scratch (reused across keep-alive requests).
    pub header: Vec<u8>,
    /// Peer address bytes (stringified) - identity fallback for untrusted peers.
    pub peer_ip: Vec<u8>,
    /// Configured send timeout (SO_SNDTIMEO), applied per write.
    send_timeout: Duration,
    /// Body bytes that arrived in the same segment(s) as the request head.
    /// Filled by [`Conn::read_head`], consumed by the request-body reader.
    pub prefetched: Vec<u8>,
    /// Total response bytes written for the current request (response head +
    /// body). Reset per keep-alive request; read by the access logger.
    response_bytes: u64,
    /// HEAD semantics: heads still carry the GET Content-Length, but every
    /// body write no-ops. Set per request by the serve loop.
    suppress_body: bool,
    /// Request-body bytes still sitting on the socket for the CURRENT request.
    /// A handler that answers without reading its body leaves them there; the
    /// connection loop must not offer keep-alive in that case, or the next
    /// request would be parsed starting inside the previous request's body
    /// (PS3.18 8.1 request framing). Set once by the serve loop, after
    /// dispatch, from the head's announced body and the drained byte count.
    unread_body: bool,
    /// Body bytes the handlers have drained for the current request. Reset by
    /// `begin_head`, so it always describes the request in hand.
    body_drained: u64,
    /// `Content-Length` of the current request, when one was announced.
    announced_body: Option<u64>,
}

impl Conn {
    pub fn new(stream: TcpStream, peer_ip: Vec<u8>) -> Self {
        Self {
            stream,
            header: Vec::with_capacity(1024),
            peer_ip,
            send_timeout: Duration::from_secs(60),
            prefetched: Vec::new(),
            response_bytes: 0,
            suppress_body: false,
            unread_body: false,
            body_drained: 0,
            announced_body: None,
        }
    }

    /// Total bytes written for the current response (head + body), including
    /// the chunked framing. Reset on every [`Conn::read_head`] so a keep-alive
    /// connection reports per-request bytes.
    pub fn response_bytes(&self) -> u64 {
        self.response_bytes
    }

    /// Credit `n` response bytes to the counter. Used where a body is written
    /// to the raw stream directly (e.g. [`crate::http::response::sendfile_response`])
    /// and therefore bypasses the counting write helpers; without it the access
    /// log `bytes=` undercounts streamed bodies.
    pub fn add_response_bytes(&mut self, n: u64) {
        self.response_bytes += n;
    }

    /// HEAD semantics (see the `suppress_body` field). Set per request by
    /// the serve loop before dispatch; the loop sets it from the method of
    /// EVERY request, so it never leaks across keep-alive requests.
    pub fn set_suppress_body(&mut self, on: bool) {
        self.suppress_body = on;
    }

    /// Is the response body suppressed (HEAD request)?
    pub fn suppress_body(&self) -> bool {
        self.suppress_body
    }

    /// Mark the current request as having a body left on the socket.
    ///
    /// Handlers answer requests whose payload they never need (an early 4xx, a
    /// route that only looks at the head) without draining it. Keeping the
    /// connection alive after that would make the next `read_head` parse body
    /// bytes as a request line - the classic request-smuggling desync
    /// (PS3.18 8.1). The connection loop closes instead.
    pub fn mark_unread_body(&mut self) {
        self.unread_body = true;
    }

    /// Was the current request's body left unread? Cleared by the next
    /// `begin_head`.
    pub fn unread_body(&self) -> bool {
        self.unread_body
    }

    /// Credit `n` drained request-body bytes to the current request.
    pub fn add_body_drained(&mut self, n: u64) {
        self.body_drained += n;
    }

    /// Body bytes drained for the current request.
    pub fn body_drained(&self) -> u64 {
        self.body_drained
    }

    /// Record the `Content-Length` the current request announced, so the
    /// response writer can tell a drained body from a pending one before it
    /// commits the response head.
    pub fn set_announced_body(&mut self, len: Option<u64>) {
        self.announced_body = len.filter(|n| *n > 0);
    }

    /// Is a body the head announced still not fully drained?
    ///
    /// Chunked requests announce no length; the body reader marks them
    /// directly through [`Conn::mark_unread_body`] when it stops early.
    pub fn body_pending(&self) -> bool {
        self.unread_body || matches!(self.announced_body, Some(n) if self.body_drained < n)
    }

    pub fn set_send_timeout(&mut self, d: Duration) {
        self.send_timeout = d;
    }

    pub fn into_stream(self) -> TcpStream {
        self.stream
    }

    pub fn stream(&self) -> &TcpStream {
        &self.stream
    }

    pub fn stream_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }

    /// Read a full request head into the caller's `buf`, returning the byte
    /// count of the head (request line + headers + trailing blank line). Any
    /// bytes beyond the `\r\n\r\n` terminator belong to the request body:
    /// they are moved to [`Conn::prefetched`] so the body reader can consume
    /// them. `timeout` is the header deadline.
    ///
    /// A2/A3: the buffer is caller-owned (a stack array in the connection
    /// loop), so the parsed head never aliases the `Conn`; socket timeouts
    /// are applied here ONCE per phase (recv = header/keep-alive deadline,
    /// send = the configured send timeout).
    pub fn read_head(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        set_timeouts(&self.stream, timeout, self.send_timeout)?;
        self.prefetched.clear();
        self.response_bytes = 0;
        self.unread_body = false;
        self.body_drained = 0;
        self.announced_body = None;
        match self.read_head_from(buf, 0)? {
            HeadRead::Complete(n) | HeadRead::Eof(n) => Ok(n),
            // The single-deadline contract of this entry point: a timeout is
            // the header deadline expiring, so it stays an error for callers
            // that ask for the whole head in one call.
            HeadRead::Idle(_) => Err(io::Error::from(io::ErrorKind::TimedOut)),
        }
    }

    /// Begin a request: apply the header deadline, drop the previous request's
    /// prefetched body and byte counter, and return the first [`HeadRead`].
    ///
    /// This is [`Conn::read_head`] with the poll-slice outcome preserved; the
    /// serve loop uses it so a slice that expires mid-head can resume from the
    /// bytes already buffered instead of asking the socket for them again.
    pub fn begin_head(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<HeadRead> {
        set_timeouts(&self.stream, timeout, self.send_timeout)?;
        self.prefetched.clear();
        self.response_bytes = 0;
        self.unread_body = false;
        self.body_drained = 0;
        self.announced_body = None;
        self.read_head_from(buf, 0)
    }

    /// Continue [`Conn::read_head`] from `used` bytes already collected in
    /// `buf`, with a fresh socket deadline.
    ///
    /// The connection loop re-issues the read after a short poll slice to
    /// notice a stop request while the connection idles; the bytes read
    /// before that poll stay in the caller's buffer and MUST NOT be asked for
    /// a second time, so progress is an explicit argument rather than local
    /// state. `prefetched` and `response_bytes` are reset exactly once, by
    /// `read_head`, at the start of the request.
    ///
    /// A poll slice that expires mid-head returns [`HeadRead::Idle`] with the
    /// number of bytes now in `buf`; the caller passes that count back to
    /// resume. Returning a bare error would lose the bytes already consumed
    /// from the socket, which is why this path does not reuse `io::Result`.
    pub fn read_head_from(&mut self, buf: &mut [u8], used: usize) -> io::Result<HeadRead> {
        let mut used = used;
        loop {
            // The terminator may sit in the middle of a read chunk (a POST
            // body usually follows the head in the same TCP segment) - scan
            // the whole accumulated buffer, not just the tail.
            if let Some(rel) = find_subslice(&buf[..used], b"\r\n\r\n") {
                let head_end = rel + 4;
                self.prefetched.extend_from_slice(&buf[head_end..used]);
                return Ok(HeadRead::Complete(head_end));
            }
            if used >= buf.len() {
                return Err(io::Error::other("head too large"));
            }
            match self.stream.read(&mut buf[used..]) {
                Ok(0) => return Ok(HeadRead::Eof(used)),
                Ok(n) => used += n,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(HeadRead::Idle(used));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Take the body bytes prefetched by [`Conn::read_head`] (empty if none).
    pub fn take_prefetched(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.prefetched)
    }

    /// Write the built response head (`self.header`) to the socket.
    ///
    /// A3: no `setsockopt` here - the send timeout was applied once by
    /// [`Conn::read_head`] for this phase.
    pub fn write_head(&mut self) -> io::Result<()> {
        self.response_bytes += self.header.len() as u64;
        self.stream.write_all(&self.header)
    }

    /// Write raw response bytes.
    ///
    /// A3: no `setsockopt` here - the send timeout was applied once by
    /// [`Conn::read_head`] for this phase.
    pub fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        if self.suppress_body {
            return Ok(());
        }
        self.response_bytes += data.len() as u64;
        self.stream.write_all(data)
    }

    /// The configured send timeout (SO_SNDTIMEO) applied per write.
    ///
    /// fix (hardcoded-config audit): the explicit-`Duration` variants
    /// receive their timeout from each call site, which let local constants
    /// shadow the configured [`Self::send_timeout`]. These configured variants
    /// are the write path the server actually uses, so `send_timeout_secs` in
    /// the config takes effect.
    pub fn send_timeout(&self) -> Duration {
        self.send_timeout
    }

    /// Write the built response head using the configured send timeout.
    ///
    /// A3: the configured send timeout is applied once per phase by
    /// [`Conn::read_head`]; the write path performs no `setsockopt`.
    pub fn write_head_configured(&mut self) -> io::Result<()> {
        self.response_bytes += self.header.len() as u64;
        self.stream.write_all(&self.header)
    }

    /// Write raw response bytes using the configured send timeout.
    ///
    /// A3: no `setsockopt` per write - see [`Conn::write_head_configured`].
    pub fn write_all_configured(&mut self, data: &[u8]) -> io::Result<()> {
        if self.suppress_body {
            return Ok(());
        }
        self.response_bytes += data.len() as u64;
        self.stream.write_all(data)
    }

    // -- High-level response API (used by handlers) ------------------

    /// Send a JSON/text response with a Content-Length body.
    pub fn send_body(
        &mut self,
        code: u16,
        reason: &str,
        content_type: &[u8],
        body: &[u8],
        keep_alive: bool,
    ) -> io::Result<()> {
        crate::http::response::build_head(
            &mut self.header,
            code,
            reason,
            Some(content_type),
            Some(body.len() as u64),
            keep_alive,
            &[],
        );
        self.write_head()?;
        self.write_all(body)
    }

    /// Send a simple error with an empty/plain-text body.
    pub fn send_error(&mut self, code: u16, reason: &str, keep_alive: bool) -> io::Result<()> {
        self.send_body(code, reason, b"text/plain; charset=utf-8", b"", keep_alive)
    }

    /// Send a file via `sendfile` following a small head (WADO instance).
    pub fn send_file(
        &mut self,
        code: u16,
        reason: &str,
        content_type: &[u8],
        file: &std::fs::File,
        len: u64,
        keep_alive: bool,
    ) -> io::Result<()> {
        crate::http::response::build_head(
            &mut self.header,
            code,
            reason,
            Some(content_type),
            Some(len),
            keep_alive,
            &[],
        );
        let head = std::mem::take(&mut self.header);
        if self.suppress_body {
            // HEAD: the head (with the GET Content-Length) goes out, the
            // file bytes do not.
            self.response_bytes += head.len() as u64;
            self.stream.write_all(&head)?;
            self.header = head;
            return Ok(());
        }
        self.response_bytes += (head.len() as u64) + len;
        crate::http::response::sendfile_response(&self.stream, &head, file, len)?;
        self.header = head;
        Ok(())
    }

    /// Begin a chunked response: `{code}\r\nTransfer-Encoding: chunked\r\n\r\n`.
    ///
    /// B5: the status line renders into a stack buffer (no `format!`), so the
    /// whole chunked framing stays 0-alloc (ap101 gate).
    pub fn chunked_start(
        &mut self,
        code: u16,
        reason: &str,
        content_type: &[u8],
        keep_alive: bool,
    ) -> io::Result<()> {
        self.chunked_start_with(code, reason, content_type, keep_alive, &[])
    }

    /// Same, with extra response headers appended to the head.
    ///
    /// Used by the SSE stream to emit `X-Accel-Buffering: no` so an nginx
    /// proxy in front of the server does not buffer the event stream. The
    /// headers belong to the HEAD: writing them after the terminator would
    /// put them in the body.
    pub fn chunked_start_with(
        &mut self,
        code: u16,
        reason: &str,
        content_type: &[u8],
        keep_alive: bool,
        extra: &[(&[u8], &[u8])],
    ) -> io::Result<()> {
        self.header.clear();
        self.header.extend_from_slice(b"HTTP/1.1 ");
        let mut nb = [0u8; 24];
        self.header
            .extend_from_slice(crate::http::response::itoa(code as u64, &mut nb));
        self.header.extend_from_slice(b" ");
        self.header.extend_from_slice(reason.as_bytes());
        self.header.extend_from_slice(b"\r\nContent-Type: ");
        self.header.extend_from_slice(content_type);
        self.header.extend_from_slice(b"\r\n");
        for (name, value) in extra {
            self.header.extend_from_slice(name);
            self.header.extend_from_slice(b": ");
            self.header.extend_from_slice(value);
            self.header.extend_from_slice(b"\r\n");
        }
        // Easter eggs on every response, the chunked path included.
        self.header
            .extend_from_slice(b"x-pragma: once\r\nx-clarus: definitum semel\r\n");
        self.header
            .extend_from_slice(crate::http::response::CORS_ALLOW_ORIGIN);
        self.header
            .extend_from_slice(b"Transfer-Encoding: chunked\r\n");
        if !keep_alive {
            self.header.extend_from_slice(b"Connection: close\r\n");
        }
        self.header.extend_from_slice(b"\r\n");
        self.write_head()
    }

    /// Write one chunk of a chunked response (allocation-free size line).
    ///
    /// B5 write_vectored (all three segments in one syscall) was tried in
    /// The measured effect was
    /// below the noise floor at corpus part sizes; the only real consumer
    /// would be UPS SSE. Revisit WITH an SSE measurement if that path
    /// becomes hot.
    pub fn chunked_data(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let mut head = [0u8; 24];
        // `{:x}\r\n` hex chunk-size into a stack buffer.
        let mut v = data.len();
        let mut digits = [0u8; 16];
        let mut n = 0usize;
        if v == 0 {
            digits[0] = b'0';
            n = 1;
        } else {
            while v > 0 {
                let d = (v & 0xF) as u8;
                digits[n] = if d < 10 { b'0' + d } else { b'a' + (d - 10) };
                n += 1;
                v >>= 4;
            }
            digits[..n].reverse();
        }
        head[..n].copy_from_slice(&digits[..n]);
        head[n] = b'\r';
        head[n + 1] = b'\n';
        let head_len = n + 2;
        self.write_all(&head[..head_len])?;
        self.write_all(data)?;
        self.write_all(b"\r\n")
    }

    /// Terminate a chunked response.
    pub fn chunked_end(&mut self) -> io::Result<()> {
        self.write_all(b"0\r\n\r\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// `send_timeout()` reports the configured value; `set_send_timeout`
    /// updates it (the value that `write_head_configured`/`write_all_configured`
    /// apply per write).
    #[test]
    fn send_timeout_reports_configured_value() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let _client = std::net::TcpStream::connect(addr).expect("connect");
        let (server, _peer) = listener.accept().expect("accept");
        let mut conn = Conn::new(server, b"127.0.0.1".to_vec());

        assert_eq!(conn.send_timeout(), Duration::from_secs(60), "default 60s");
        conn.set_send_timeout(Duration::from_secs(42));
        assert_eq!(
            conn.send_timeout(),
            Duration::from_secs(42),
            "updated value"
        );
    }

    /// `set_timeouts` reports what the kernel refused instead of swallowing it.
    ///
    /// `SO_RCVTIMEO` cannot express "wait forever" as a duration, so std rejects
    /// `Duration::ZERO` outright. The previous hand-rolled `setsockopt` ignored
    /// both return values and answered `Ok(())`, which made a misconfigured
    /// connection indistinguishable from a configured one: the worker would park
    /// on a silent peer until the process died.
    #[test]
    fn set_timeouts_reports_a_zero_duration_instead_of_discarding_it() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let _client = std::net::TcpStream::connect(addr).expect("connect");
        let (server, _peer) = listener.accept().expect("accept");

        assert!(
            set_timeouts(&server, Duration::ZERO, Duration::from_secs(30)).is_err(),
            "a zero receive timeout must not be reported as applied"
        );
        assert!(
            set_timeouts(&server, Duration::from_secs(30), Duration::from_secs(30)).is_ok(),
            "a positive pair must be applied"
        );
    }

    /// HEAD semantics: the head goes out, the body does not, and
    /// `response_bytes` counts only what hit the socket.
    #[test]
    fn suppress_body_writes_head_only() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).expect("connect");
        let (server, _peer) = listener.accept().expect("accept");
        let mut conn = Conn::new(server, b"127.0.0.1".to_vec());

        conn.set_suppress_body(true);
        conn.send_body(200, "OK", b"text/plain", b"hello", true)
            .expect("send");
        let head_len = conn.header.len() as u64;
        assert_eq!(conn.response_bytes(), head_len, "head only on the wire");

        // A body write on a suppressed connection is a silent no-op.
        let before = conn.response_bytes();
        conn.write_all_configured(b"more body")
            .expect("no-op write");
        assert_eq!(conn.response_bytes(), before);

        drop(client);
    }

    /// The chunked head keeps its header lines CRLF-terminated: the CORS
    /// line must NOT merge into the Content-Type value (a missing `\r\n`
    /// there corrupts the multipart boundary for every client - regression
    /// pin for the 02.09 CORS fix).
    #[test]
    fn chunked_start_head_lines_are_crlf_separated_with_cors() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let _client = std::net::TcpStream::connect(addr).expect("connect");
        let (server, _peer) = listener.accept().expect("accept");
        let mut conn = Conn::new(server, b"127.0.0.1".to_vec());

        conn.chunked_start(200, "OK", b"multipart/related; boundary=\"b\"", true)
            .expect("chunked start");
        let head = String::from_utf8_lossy(&conn.header);
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "got {head}");
        assert!(
            head.contains("Content-Type: multipart/related; boundary=\"b\"\r\n"),
            "Content-Type must be its own line: {head}"
        );
        assert!(
            head.contains("Access-Control-Allow-Origin: *\r\n"),
            "CORS must be its own line: {head}"
        );
        assert!(
            head.contains("Transfer-Encoding: chunked\r\n"),
            "got {head}"
        );
        assert!(head.ends_with("\r\n\r\n"), "got {head}");
    }

    /// The head reader must terminate at the first `\r\n\r\n` ANYWHERE in the
    /// buffer and stash the trailing bytes (a POST body arriving with the
    /// head in the same segment) into `Conn::prefetched`.
    #[test]
    fn read_head_stashes_body_bytes_past_terminator() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let mut client = std::net::TcpStream::connect(addr).expect("connect");
        let (server, _peer) = listener.accept().expect("accept");
        client.set_nodelay(true).expect("nodelay");

        let body: Vec<u8> = {
            let mut b = b"--b\r\n\r\n".to_vec();
            b.extend_from_slice(&vec![0xAAu8; 5000]);
            b
        };
        let mut req =
            b"POST /dicomweb/studies HTTP/1.1\r\nHost: x\r\nContent-Length: 5006\r\n\r\n".to_vec();
        req.extend_from_slice(&body);

        // Write the whole request in one syscall so head+body land in the
        // server's first read chunk.
        client.write_all(&req).expect("write");

        let mut conn = Conn::new(server, b"127.0.0.1".to_vec());
        let mut head_buf = [0u8; HEADER_BUF_SIZE];
        let n = conn
            .read_head(&mut head_buf, Duration::from_secs(5))
            .expect("read head");
        assert_eq!(
            n,
            b"POST /dicomweb/studies HTTP/1.1\r\nHost: x\r\nContent-Length: 5006\r\n\r\n".len(),
            "read_head must return the head length, not the bytes read"
        );
        assert_eq!(
            &head_buf[..n],
            b"POST /dicomweb/studies HTTP/1.1\r\nHost: x\r\nContent-Length: 5006\r\n\r\n"
        );
        assert_eq!(
            conn.prefetched, body,
            "all body bytes past the terminator must be prefetched"
        );

        // Consuming the stash empties it exactly once.
        let taken = conn.take_prefetched();
        assert_eq!(taken, body);
        assert!(conn.prefetched.is_empty());
        assert!(conn.take_prefetched().is_empty());
    }

    /// `response_bytes` counts exactly the bytes written (head + body) and is
    /// reset on the next `read_head`, so a keep-alive connection reports
    /// per-request response size for the access log.
    #[test]
    fn response_bytes_counts_head_and_body_and_resets() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let mut client = std::net::TcpStream::connect(addr).expect("connect");
        let (server, _peer) = listener.accept().expect("accept");

        client
            .write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
            .expect("write");
        let mut conn = Conn::new(server, b"127.0.0.1".to_vec());
        let mut head_buf = [0u8; HEADER_BUF_SIZE];
        conn.read_head(&mut head_buf, Duration::from_secs(5))
            .expect("read head");
        assert_eq!(conn.response_bytes(), 0, "no response written yet");

        conn.send_body(200, "OK", b"text/plain", b"hello world", true)
            .expect("send body");
        // header + body; header includes the HTTP status line + headers.
        let head_len = conn.header.len() as u64;
        assert_eq!(conn.response_bytes(), head_len + 11, "head + body bytes");
    }

    /// A fresh `read_head` must reset the counter (keep-alive per-request
    /// accounting).
    #[test]
    fn response_bytes_resets_on_read_head() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let mut client = std::net::TcpStream::connect(addr).expect("connect");
        let (server, _peer) = listener.accept().expect("accept");
        let mut conn = Conn::new(server, b"127.0.0.1".to_vec());

        client
            .write_all(b"GET /a HTTP/1.1\r\nHost: x\r\n\r\n")
            .expect("w");
        let mut head_buf = [0u8; HEADER_BUF_SIZE];
        conn.read_head(&mut head_buf, Duration::from_secs(5))
            .expect("h1");
        conn.send_body(200, "OK", b"text/plain", b"12345", true)
            .expect("b1");
        let after_first = conn.response_bytes();
        assert!(after_first > 5);

        client
            .write_all(b"GET /b HTTP/1.1\r\nHost: x\r\n\r\n")
            .expect("w2");
        conn.read_head(&mut head_buf, Duration::from_secs(5))
            .expect("h2");
        assert_eq!(
            conn.response_bytes(),
            0,
            "counter must reset at the start of the next request"
        );
    }
}
