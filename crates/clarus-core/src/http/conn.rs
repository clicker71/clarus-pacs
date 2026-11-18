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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Absolute deadline for the request head, armed by [`Conn::begin_head`].
    ///
    /// A socket receive timeout is re-armed on every `read`, so it alone
    /// bounds each SLICE and not the whole head: a peer trickling one byte
    /// inside every slice would keep a worker indefinitely. The loop compares
    /// against this instant instead.
    head_deadline: Option<std::time::Instant>,
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
            head_deadline: None,
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
    /// them. Bytes left in [`Conn::prefetched`] by the PREVIOUS request's body
    /// belong to THIS request's head (a pipelined second request) and are
    /// adopted into `buf` before the socket is read, exactly like
    /// [`Conn::begin_head`]. `timeout` is the header deadline.
    ///
    /// A2/A3: the buffer is caller-owned (a stack array in the connection
    /// loop), so the parsed head never aliases the `Conn`; socket timeouts
    /// are applied here ONCE per phase (recv = header/keep-alive deadline,
    /// send = the configured send timeout).
    pub fn read_head(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        set_timeouts(&self.stream, timeout, self.send_timeout)?;
        // Arm the ABSOLUTE deadline here too, not only in `begin_head`:
        // `read_head_from` enforces `head_deadline`, and this entry point is
        // reachable without `begin_head`, so an unarmed deadline meant a
        // slow-drip peer could hold the head open indefinitely. Always a fresh
        // instant - a leftover from a previous request would be in the past.
        self.head_deadline = Some(std::time::Instant::now() + timeout);
        // `prefetched` is NOT cleared here. On a persistent connection it
        // holds the first bytes of a pipelined request that arrived with the
        // previous one; adopting them into `buf` (exactly like `begin_head`)
        // is the only way that request is not lost.
        self.response_bytes = 0;
        self.unread_body = false;
        self.body_drained = 0;
        self.announced_body = None;
        let carried = self.adopt_prefetched_into(buf);
        match self.read_head_from(buf, carried)? {
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
        // The header deadline is ABSOLUTE, not per-read. A socket timeout is
        // re-armed on every `read`, so a peer that delivers one byte just
        // inside each slice keeps a worker for as long as it likes; the
        // deadline recorded here is the one the loop enforces.
        self.head_deadline = Some(std::time::Instant::now() + timeout);
        // `prefetched` is NOT cleared here. It holds bytes that arrived with
        // the PREVIOUS request's header - on a persistent connection those are
        // the first bytes of the pipelined request that follows, and dropping
        // them loses that request entirely. They are consumed by the body
        // reader; whatever survives belongs to the next head, so it is moved
        // into `buf` below rather than discarded.
        self.response_bytes = 0;
        self.unread_body = false;
        self.body_drained = 0;
        self.announced_body = None;
        let carried = self.adopt_prefetched_into(buf);
        self.read_head_from(buf, carried)
    }

    /// Move any unconsumed prefetched bytes into `buf` and return how many.
    ///
    /// The bytes belong to the NEXT request on a persistent connection: a
    /// client is free to send two requests back to back, so the second one's
    /// header can arrive in the same segment as the first one's body. Handing
    /// them to the head read is the only way that request is not lost.
    fn adopt_prefetched_into(&mut self, buf: &mut [u8]) -> usize {
        if self.prefetched.is_empty() {
            return 0;
        }
        // Move as much of the run as fits into `buf`; the remainder stays in
        // `prefetched` as the suffix. The head read then splices the head's
        // trailing bytes IN FRONT of that suffix (see `stash_prefetched`), so
        // the stream order survives even when the run is longer than the head
        // buffer.
        let n = self.prefetched.len().min(buf.len());
        buf[..n].copy_from_slice(&self.prefetched[..n]);
        self.prefetched.drain(..n);
        n
    }

    /// Stash head-trailing body bytes ahead of any prefetched suffix.
    ///
    /// On the common path `prefetched` is empty (the whole run was adopted
    /// into `buf`), so this is a plain append and costs nothing. When a
    /// leftover suffix survives [`Conn::adopt_prefetched_into`] (a prefetched
    /// run larger than the head buffer), those suffix bytes are EARLIER in the
    /// stream than the head's trailing bytes and must come AFTER them:
    /// splicing the new bytes in front keeps the stream order. Appending used
    /// to put the later suffix first, so the body reader saw the tail before
    /// the head's own body bytes.
    fn stash_prefetched(&mut self, bytes: &[u8]) {
        if self.prefetched.is_empty() {
            self.prefetched.extend_from_slice(bytes);
        } else {
            let tail = std::mem::take(&mut self.prefetched);
            self.prefetched.extend_from_slice(bytes);
            self.prefetched.extend_from_slice(&tail);
        }
    }

    /// Continue [`Conn::read_head`] from `used` bytes already collected in
    /// `buf`, with a fresh socket deadline.
    ///
    /// The connection loop re-issues the read after a short poll slice to
    /// notice a stop request while the connection idles; the bytes read
    /// before that poll stay in the caller's buffer and MUST NOT be asked for
    /// a second time, so progress is an explicit argument rather than local
    /// state. `response_bytes` is reset exactly once, by `read_head`, at the
    /// start of the request; `prefetched` is adopted into `buf` there.
    ///
    /// A poll slice that expires mid-head returns [`HeadRead::Idle`] with the
    /// number of bytes now in `buf`; the caller passes that count back to
    /// resume. Returning a bare error would lose the bytes already consumed
    /// from the socket, which is why this path does not reuse `io::Result`.
    pub fn read_head_from(&mut self, buf: &mut [u8], used: usize) -> io::Result<HeadRead> {
        let mut used = used;
        // A head that is ALREADY complete on entry (a pipelined second request
        // adopted from `prefetched`) must be answered without touching the
        // socket: reading first would block on a peer that has nothing more to
        // send. This scan also catches the first segment of a normal head.
        if let Some(rel) = find_subslice(&buf[..used], b"\r\n\r\n") {
            let head_end = rel + 4;
            self.stash_prefetched(&buf[head_end..used]);
            return Ok(HeadRead::Complete(head_end));
        }
        loop {
            if used >= buf.len() {
                return Err(io::Error::other("head too large"));
            }
            match self.stream.read(&mut buf[used..]) {
                Ok(0) => return Ok(HeadRead::Eof(used)),
                Ok(n) => {
                    used += n;
                    // Scan the JUST-READ bytes BEFORE the deadline check. The
                    // terminator may sit in the middle of a read chunk (a POST
                    // body usually follows the head in the same TCP segment),
                    // so the whole accumulated buffer is scanned, not just the
                    // tail. A head whose final bytes arrive in this segment is
                    // COMPLETE even though the absolute deadline has since
                    // passed - checking the deadline first rejected a fully
                    // buffered request as Idle.
                    if let Some(rel) = find_subslice(&buf[..used], b"\r\n\r\n") {
                        let head_end = rel + 4;
                        self.stash_prefetched(&buf[head_end..used]);
                        return Ok(HeadRead::Complete(head_end));
                    }
                    // Do not wait for the NEXT slice to notice an expired
                    // deadline: a peer that keeps sending one byte per slice
                    // would never be timed out by the socket option alone.
                    if self
                        .head_deadline
                        .is_some_and(|d| std::time::Instant::now() >= d)
                    {
                        return Ok(HeadRead::Idle(used));
                    }
                }
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

    /// A pipelined second request survives the first one's read.
    ///
    /// A client may send two requests back to back, so the second request's
    /// head can arrive in the same segment as the first one's. Those bytes are
    /// stashed in `prefetched` by the first read; `begin_head` used to clear
    /// the stash outright, so the second request was simply lost - the server
    /// answered the first and then waited for a request it had already thrown
    /// away.
    #[test]
    fn a_pipelined_second_request_survives_the_next_begin_head() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let mut client = std::net::TcpStream::connect(addr).expect("connect");
        let (server, _peer) = listener.accept().expect("accept");
        let mut conn = Conn::new(server, b"127.0.0.1".to_vec());

        const FIRST: &[u8] = b"GET /a HTTP/1.1\r\nHost: x\r\n\r\n";
        const SECOND: &[u8] = b"GET /b HTTP/1.1\r\nHost: x\r\n\r\n";
        let mut both = FIRST.to_vec();
        both.extend_from_slice(SECOND);
        client.write_all(&both).expect("write both requests");

        let mut head_buf = [0u8; HEADER_BUF_SIZE];
        let first_len = match conn
            .begin_head(&mut head_buf, Duration::from_secs(5))
            .expect("first head")
        {
            HeadRead::Complete(n) => n,
            other => panic!("first request must complete, got {other:?}"),
        };
        assert_eq!(
            &head_buf[..first_len],
            FIRST,
            "the first request is the one that was answered"
        );
        // The second request's bytes were stashed with the first read.
        assert_eq!(conn.prefetched, SECOND);

        // The next request must START from the stash, not from a cleared one.
        let second_len = match conn
            .begin_head(&mut head_buf, Duration::from_secs(5))
            .expect("second head")
        {
            HeadRead::Complete(n) => n,
            other => panic!("pipelined request must complete, got {other:?}"),
        };
        assert_eq!(
            &head_buf[..second_len],
            SECOND,
            "the pipelined request must not be lost"
        );
    }

    /// `read_head` must adopt the prefetched run too, not only `begin_head`:
    /// a caller using `read_head` directly on a keep-alive connection loses a
    /// pipelined second request if the stash is cleared instead of adopted.
    #[test]
    fn a_pipelined_second_request_survives_the_next_read_head() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let mut client = std::net::TcpStream::connect(addr).expect("connect");
        let (server, _peer) = listener.accept().expect("accept");
        let mut conn = Conn::new(server, b"127.0.0.1".to_vec());

        const FIRST: &[u8] = b"GET /a HTTP/1.1\r\nHost: x\r\n\r\n";
        const SECOND: &[u8] = b"GET /b HTTP/1.1\r\nHost: x\r\n\r\n";
        let mut both = FIRST.to_vec();
        both.extend_from_slice(SECOND);
        client.write_all(&both).expect("write both requests");

        let mut head_buf = [0u8; HEADER_BUF_SIZE];
        let first_len = conn
            .read_head(&mut head_buf, Duration::from_secs(5))
            .expect("first head");
        assert_eq!(&head_buf[..first_len], FIRST, "first request parsed");
        assert_eq!(conn.prefetched, SECOND, "second request stashed");

        // A short deadline keeps the falsification fast: if the stash were
        // cleared (the old behaviour), the second read blocks and times out.
        let second_len = conn
            .read_head(&mut head_buf, Duration::from_millis(300))
            .expect("second head");
        assert_eq!(
            &head_buf[..second_len],
            SECOND,
            "the pipelined request must not be lost"
        );
    }

    /// A prefetched run longer than the head buffer keeps its byte order.
    ///
    /// `adopt_prefetched_into` moves only `buf.len()` bytes and leaves the
    /// suffix in `prefetched`; the head's trailing bytes (EARLIER in the
    /// stream) must then be spliced IN FRONT of that suffix. Appending used to
    /// invert the order, so the body reader saw the later suffix before the
    /// head's own body bytes. Reverting the front-insertion makes the
    /// surviving `prefetched` equal `suffix ++ head_body` instead of the
    /// stream order asserted below, so the test fails - it is not vacuous.
    #[test]
    fn a_prefetched_run_longer_than_the_head_buffer_keeps_byte_order() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let _client = std::net::TcpStream::connect(addr).expect("connect");
        let (server, _peer) = listener.accept().expect("accept");
        let mut conn = Conn::new(server, b"127.0.0.1".to_vec());

        const HEAD: &[u8] = b"POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: 20000\r\n\r\n";
        // One stream: head + a body long enough that the run exceeds the head
        // buffer. The first HEADER_BUF_SIZE bytes are adopted into the caller's
        // buffer; the rest stays in `prefetched` as the suffix.
        let mut stream = HEAD.to_vec();
        stream.extend((0u8..=255u8).cycle().take(20_000));
        conn.prefetched = stream.clone();

        let mut head_buf = [0u8; HEADER_BUF_SIZE];
        let outcome = conn
            .begin_head(&mut head_buf, Duration::from_secs(5))
            .expect("head");
        let head_len = match outcome {
            HeadRead::Complete(n) => n,
            other => panic!("the head must complete from the prefetched run, got {other:?}"),
        };
        assert_eq!(
            &head_buf[..head_len],
            HEAD,
            "the head is parsed from the run"
        );

        // The surviving prefetched bytes must equal the body in STREAM order:
        // head-trailing bytes first, then the suffix.
        let expected_body = &stream[HEAD.len()..];
        assert_eq!(
            conn.prefetched.as_slice(),
            expected_body,
            "the prefetched body must keep its stream order (no inversion)"
        );
    }

    /// The header deadline is absolute, not per-read.
    ///
    /// A socket receive timeout is re-armed on every `read`, so a peer that
    /// sends one byte just inside each slice keeps a worker forever. The loop
    /// now compares against the instant `begin_head` recorded, so a drip feed
    /// reports `Idle` (which the serve loop decrements its budget from)
    /// instead of looping silently.
    #[test]
    fn a_slow_drip_peer_cannot_hold_the_head_read_past_its_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let mut client = std::net::TcpStream::connect(addr).expect("connect");
        let (server, _peer) = listener.accept().expect("accept");
        let mut conn = Conn::new(server, b"127.0.0.1".to_vec());

        // Trickle the head one byte at a time, well inside the socket timeout,
        // for longer than the header deadline.
        let drip = std::thread::spawn(move || {
            for byte in b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n" {
                if client.write_all(&[*byte]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });

        let mut head_buf = [0u8; HEADER_BUF_SIZE];
        // A short header deadline: the drip needs ~600 ms for the whole head.
        let outcome = conn
            .begin_head(&mut head_buf, Duration::from_millis(120))
            .expect("head read");
        match outcome {
            HeadRead::Idle(_) => {}
            other => {
                panic!("a slow-drip peer must be cut off by the header deadline, got {other:?}")
            }
        }
        drop(drip);
    }

    /// A head whose final bytes arrive AFTER the header deadline is still a
    /// complete request, not an Idle timeout.
    ///
    /// The scan used to run at the loop top and the deadline check after the
    /// read, so a head completed in the segment just read was rejected before
    /// the terminator was ever looked for. The scan now runs right after the
    /// read, before the deadline check.
    #[test]
    fn a_head_completed_after_the_deadline_is_complete_not_idle() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let mut client = std::net::TcpStream::connect(addr).expect("connect");
        let (server, _peer) = listener.accept().expect("accept");
        let mut conn = Conn::new(server, b"127.0.0.1".to_vec());

        // First segment: a partial head with no terminator. The read loop then
        // blocks on the second read until the socket timeout returns it as
        // Idle, which is the slow-drip behaviour this test must NOT disturb.
        client
            .write_all(b"GET /dicomweb/studies HTTP/1.1\r\nHost: x\r\n")
            .expect("first segment");

        let mut head_buf = [0u8; HEADER_BUF_SIZE];
        let timeout = Duration::from_millis(150);
        let first = conn.begin_head(&mut head_buf, timeout).expect("first read");
        let used = match first {
            HeadRead::Idle(n) => n,
            other => panic!("a partial head must report Idle, got {other:?}"),
        };
        assert!(used > 0, "the first segment must be buffered");

        // Let the absolute header deadline expire before the head completes.
        std::thread::sleep(timeout + Duration::from_millis(80));

        // The final segment arrives AFTER the deadline.
        client
            .write_all(b"Accept: application/dicom+json\r\n\r\n")
            .expect("second segment");

        let second = conn
            .read_head_from(&mut head_buf, used)
            .expect("second read");
        match second {
            HeadRead::Complete(n) => assert!(n > used, "the full head must be read"),
            other => panic!("a head completed after the deadline must be Complete, got {other:?}"),
        }
    }

    /// F9: `read_head` must arm the header deadline itself.
    ///
    /// `read_head_from` enforces `head_deadline`, but only `begin_head` used to
    /// set it. `read_head` is its own public entry point and does not call
    /// `begin_head`, so a caller on that path got no absolute deadline at all
    /// and a drip-feeding peer could hold a worker open until it chose to
    /// finish - the socket timeout alone is re-armed by every byte.
    #[test]
    fn read_head_arms_its_own_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let mut client = std::net::TcpStream::connect(addr).expect("connect");
        let (server, _peer) = listener.accept().expect("accept");
        let mut conn = Conn::new(server, b"127.0.0.1".to_vec());

        // One byte per slice, each well inside the socket timeout, for far
        // longer than the header deadline.
        let drip = std::thread::spawn(move || {
            for byte in b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n" {
                if client.write_all(&[*byte]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });

        let mut head_buf = [0u8; HEADER_BUF_SIZE];
        let started = std::time::Instant::now();
        let err = conn
            .read_head(&mut head_buf, Duration::from_millis(120))
            .expect_err("a drip-fed head must hit the header deadline");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the deadline must cut the read off, not wait for the peer: took {:?}",
            started.elapsed()
        );
        drop(drip);
    }
}
