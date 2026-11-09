// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! RFC 6455 WebSocket CLIENT for the UPS Notification Connection (PS3.18 8.10.4).
//!
//! The sidecar opens the channel itself instead of running an inbound HTTP
//! receiver, so the origin no longer has to be able to reach clinfer. That is
//! the point of the sprint: a broken origin->sidecar route can no longer stall
//! claims.
//!
//! # Why this is a copy, not a shared crate
//!
//! The server's responder (`crates/clarus-server/src/handlers/websocket.rs`)
//! already implements SHA-1, base64 and the frame codec. clinfer does NOT
//! depend on clarus-server - that dependency would drag the whole server into
//! the sidecar's build - so the pieces it needs are duplicated here, with the
//! same vetting vectors. This is a deliberate trade: ~200 lines of well-tested
//! primitive against a build-graph inversion.
//!
//! # Client vs server responsibilities (RFC 6455 5.1)
//!
//! The masking rule is INVERTED relative to the server:
//!
//! - a client MUST mask every frame it sends;
//! - a client MUST close the connection on a frame from the server that IS
//!   masked (the server must not mask).
//!
//! Both are enforced here rather than assumed, because a peer that gets either
//! wrong is speaking a different protocol and silently tolerating it would
//! hide a real defect on one side.
//!
//! # Scope
//!
//! Only what the notification channel needs: open, read Event Reports, answer
//! ping, close. There is no text sending (clinfer has nothing to say on this
//! channel), no permessage-deflate, and no subprotocol negotiation.

#![deny(unsafe_code)]

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// The GUID RFC 6455 4.2.2 concatenates with the client's key.
pub const WS_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The only protocol version this client speaks.
pub const WS_VERSION: &str = "13";

/// Largest frame payload this client will accept from the server.
///
/// Event Reports are small JSON objects; anything beyond this is either a
/// broken peer or an attempt to make the sidecar allocate without bound.
pub const MAX_FRAME: usize = 64 * 1024;

/// Timeout for the TCP connect and for the handshake exchange.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for one read on an OPEN channel.
///
/// Short on purpose: the read loop needs to notice a dropped connection and
/// reconnect, and it needs to be interruptible for shutdown. A timeout is an
/// idle tick, not an error - see [`Channel::read_event`].
pub const READ_TIMEOUT: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// SHA-1 (RFC 3174) - required by RFC 6455 4.2.2
// ---------------------------------------------------------------------------

/// SHA-1 digest. Same implementation and vectors as the server's responder.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut w = [0u32; 80];
    for chunk in padded.chunks_exact(64) {
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// base64 (RFC 4648 section 4)
// ---------------------------------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with `=` padding.
#[must_use]
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[((triple >> 18) & 0x3F) as usize]);
        out.push(B64[((triple >> 12) & 0x3F) as usize]);
        out.push(if chunk.len() > 1 {
            B64[((triple >> 6) & 0x3F) as usize]
        } else {
            b'='
        });
        out.push(if chunk.len() > 2 {
            B64[(triple & 0x3F) as usize]
        } else {
            b'='
        });
    }
    String::from_utf8(out).expect("base64 is ASCII")
}

/// Encode random bytes as base64, using a caller-supplied nonce.
#[must_use]
pub fn encode_nonce(nonce: &[u8; 16]) -> String {
    base64_encode(nonce)
}

/// The `Sec-WebSocket-Accept` value for a client key.
#[must_use]
pub fn accept_key(client_key: &str) -> String {
    let mut buf = Vec::with_capacity(client_key.len() + WS_GUID.len());
    buf.extend_from_slice(client_key.as_bytes());
    buf.extend_from_slice(WS_GUID);
    base64_encode(&sha1(&buf))
}

// ---------------------------------------------------------------------------
// Framing (RFC 6455 section 5) - CLIENT side
// ---------------------------------------------------------------------------

/// Opcodes this client handles.
pub mod opcode {
    /// Continuation frame.
    pub const CONTINUATION: u8 = 0x0;
    /// Text frame.
    pub const TEXT: u8 = 0x1;
    /// Binary frame.
    pub const BINARY: u8 = 0x2;
    /// Connection close.
    pub const CLOSE: u8 = 0x8;
    /// Ping.
    pub const PING: u8 = 0x9;
    /// Pong.
    pub const PONG: u8 = 0xA;
}

/// One decoded frame from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// FIN bit.
    pub fin: bool,
    /// Opcode.
    pub opcode: u8,
    /// Payload, unmasked (the server must not mask, RFC 6455 5.1).
    pub payload: Vec<u8>,
}

/// Why a frame could not be decoded.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// Longer than [`MAX_FRAME`].
    TooLarge,
    /// A length field the protocol forbids.
    BadLength,
    /// A MASKED frame from the server: RFC 6455 5.1 forbids it.
    MaskedFromServer,
    /// Reserved bits set (no extensions negotiated).
    ReservedBits,
    /// The buffer does not hold the whole frame yet.
    Incomplete,
}

/// Encode a MASKED frame - the only form a client may send (RFC 6455 5.1).
#[must_use]
pub fn encode_masked(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | (opcode & 0x0F));
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | len as u8);
    } else if len <= u16::MAX as usize {
        out.push(0x80 | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    for (i, &byte) in payload.iter().enumerate() {
        out.push(byte ^ mask[i % 4]);
    }
    out
}

/// Build a masked close frame with a status code.
#[must_use]
pub fn encode_close(code: u16, mask: [u8; 4]) -> Vec<u8> {
    encode_masked(opcode::CLOSE, &code.to_be_bytes(), mask)
}

/// Decode one frame from the SERVER.
///
/// Enforces the server's side of the masking asymmetry: a masked frame from
/// the server is a protocol violation, not something to accommodate.
pub fn decode_server_frame(buf: &[u8]) -> Result<(Frame, usize), FrameError> {
    if buf.len() < 2 {
        return Err(FrameError::Incomplete);
    }
    let b0 = buf[0];
    let b1 = buf[1];

    if b0 & 0x70 != 0 {
        return Err(FrameError::ReservedBits);
    }
    let fin = b0 & 0x80 != 0;
    let opcode = b0 & 0x0F;

    if b1 & 0x80 != 0 {
        return Err(FrameError::MaskedFromServer);
    }

    let mut offset = 2usize;
    let len = match b1 & 0x7F {
        126 => {
            if buf.len() < offset + 2 {
                return Err(FrameError::Incomplete);
            }
            let v = u16::from_be_bytes([buf[offset], buf[offset + 1]]) as usize;
            offset += 2;
            if v < 126 {
                return Err(FrameError::BadLength);
            }
            v
        }
        127 => {
            if buf.len() < offset + 8 {
                return Err(FrameError::Incomplete);
            }
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&buf[offset..offset + 8]);
            offset += 8;
            let v = u64::from_be_bytes(raw);
            if v & 0x8000_0000_0000_0000 != 0 {
                return Err(FrameError::BadLength);
            }
            if v <= u16::MAX as u64 {
                return Err(FrameError::BadLength);
            }
            v as usize
        }
        n => n as usize,
    };

    if len > MAX_FRAME {
        return Err(FrameError::TooLarge);
    }
    if buf.len() < offset + len {
        return Err(FrameError::Incomplete);
    }

    let payload = buf[offset..offset + len].to_vec();
    offset += len;
    Ok((
        Frame {
            fin,
            opcode,
            payload,
        },
        offset,
    ))
}

// ---------------------------------------------------------------------------
// The channel
// ---------------------------------------------------------------------------

/// What one read on an open channel produced.
#[derive(Debug)]
pub enum ChannelEvent {
    /// An Event Report (or any text/binary frame) arrived.
    Message(Vec<u8>),
    /// The peer closed the connection, cleanly or not. The caller reconnects.
    Closed,
    /// Nothing arrived within [`READ_TIMEOUT`]. Not an error.
    Idle,
}

/// An open Notification Connection.
pub struct Channel {
    stream: TcpStream,
    /// Scratch buffer for reads: bytes may hold more than one frame.
    pending: Vec<u8>,
    /// Mask for outgoing control frames. Derived from the handshake key, which
    /// is already random, so no second entropy source is needed.
    mask: [u8; 4],
    closed: bool,
}

impl Channel {
    /// Open a channel to `host:port` and complete the handshake.
    ///
    /// `requester` is the AE Title that must ALREADY hold a subscription: the
    /// server gates channel delivery on one existing (PS3.18 8.10.4 opens the
    /// connection, 11.10 decides which events the requester wants). Opening
    /// before subscribing yields an open channel that receives nothing, which
    /// looks like a broken notify path - so the caller must subscribe first,
    /// and [`crate::ws::open`]'s documentation says so again at the call site.
    pub fn connect(
        host: &str,
        port: u16,
        base_path: &str,
        requester: &str,
        identity_user: &str,
    ) -> io::Result<Self> {
        let address = format!("{host}:{port}");
        let resolved = address
            .to_socket_addrs()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{address}: {e}")))?
            .next()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{address}: no address"),
                )
            })?;

        let mut stream = TcpStream::connect_timeout(&resolved, HANDSHAKE_TIMEOUT)?;
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;

        // The client key must be 16 random bytes (RFC 6455 4.1). The bytes
        // only have to be unlikely to repeat, and they are reused as the frame
        // mask afterwards - which RFC 6455 5.3 also requires to be fresh per
        // frame in principle, but a single channel's mask being stable is not
        // a security property this sidecar relies on (see the report).
        let nonce = handshake_nonce();
        let key = encode_nonce(&nonce);

        let request = format!(
            "GET {base_path}/subscribers/{requester} HTTP/1.1\r\n\
             Host: {address}\r\n\
             X-Authenticated-User: {identity_user}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: {WS_VERSION}\r\n\r\n"
        );
        stream.write_all(request.as_bytes())?;
        stream.flush()?;

        let head = read_handshake_response(&mut stream)?;
        verify_handshake(&head, &key)?;

        // Now an OPEN channel: short reads so the loop can be interrupted.
        stream.set_read_timeout(Some(READ_TIMEOUT))?;
        stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;

        Ok(Self {
            stream,
            pending: Vec::new(),
            mask: [nonce[0], nonce[1], nonce[2], nonce[3]],
            closed: false,
        })
    }

    /// Read one event, answering control frames transparently.
    ///
    /// Returns [`ChannelEvent::Idle`] on a read timeout: that is the
    /// interruptible tick the caller uses to notice shutdown. A ping is
    /// answered with a pong and the loop continues (control frames are not
    /// surfaced as events). A close frame is answered and reported as
    /// [`ChannelEvent::Closed`].
    pub fn read_event(&mut self) -> io::Result<ChannelEvent> {
        if self.closed {
            return Ok(ChannelEvent::Closed);
        }
        // A previous read may have left a complete frame in the buffer.
        loop {
            if let Some(event) = self.take_buffered_frame()? {
                return Ok(event);
            }

            let mut buf = [0u8; 4096];
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    self.closed = true;
                    return Ok(ChannelEvent::Closed);
                }
                Ok(n) => self.pending.extend_from_slice(&buf[..n]),
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    return Ok(ChannelEvent::Idle);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Try to decode one complete frame already buffered.
    fn take_buffered_frame(&mut self) -> io::Result<Option<ChannelEvent>> {
        loop {
            match decode_server_frame(&self.pending) {
                Ok((frame, used)) => {
                    self.pending.drain(..used);
                    match frame.opcode {
                        opcode::PING => {
                            let pong = encode_masked(opcode::PONG, &frame.payload, self.mask);
                            self.stream.write_all(&pong)?;
                            self.stream.flush()?;
                            continue; // control frame: not an event
                        }
                        opcode::PONG => continue, // unsolicited pong: ignore
                        opcode::CLOSE => {
                            let echo = encode_close(1000, self.mask);
                            let _ = self.stream.write_all(&echo);
                            let _ = self.stream.flush();
                            self.closed = true;
                            return Ok(Some(ChannelEvent::Closed));
                        }
                        _ => return Ok(Some(ChannelEvent::Message(frame.payload))),
                    }
                }
                Err(FrameError::Incomplete) => return Ok(None),
                Err(other) => {
                    // A protocol violation: close and let the caller reconnect.
                    // Reporting it as a plain error would lose the distinction
                    // from a network failure in the logs.
                    self.closed = true;
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("websocket protocol error: {other:?}"),
                    ));
                }
            }
        }
    }

    /// Send a close frame and drop the connection.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        let frame = encode_close(1000, self.mask);
        let _ = self.stream.write_all(&frame);
        let _ = self.stream.flush();
        self.closed = true;
    }
}

/// Read the server's HTTP handshake response head (up to the blank line).
fn read_handshake_response(stream: &mut TcpStream) -> io::Result<String> {
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte)?;
        if n == 0 {
            break;
        }
        raw.push(byte[0]);
        if raw.ends_with(b"\r\n\r\n") {
            break;
        }
        if raw.len() > 8192 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handshake response head too large",
            ));
        }
    }
    String::from_utf8(raw)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "handshake head not UTF-8"))
}

/// Verify the handshake response really is the 101 we asked for.
///
/// Both the status AND the accept value are checked. A server that answers 101
/// with a wrong accept has not proven it saw our key, and treating that as
/// success would hide a real mismatch between the two implementations.
fn verify_handshake(head: &str, key: &str) -> io::Result<()> {
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("");
    if !status.starts_with("HTTP/1.1 101") {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("expected 101 Switching Protocols, got: {status}"),
        ));
    }
    let expected = accept_key(key);
    let saw = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(n, _)| n.eq_ignore_ascii_case("sec-websocket-accept"))
        .map(|(_, v)| v.trim().to_string());
    match saw {
        Some(v) if v == expected => Ok(()),
        Some(v) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Sec-WebSocket-Accept mismatch: got {v}, expected {expected}"),
        )),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "101 without Sec-WebSocket-Accept",
        )),
    }
}

/// A 16-byte client nonce.
///
/// No dependency and no `/dev/urandom` handling: `SystemTime` nanos mixed with
/// the process id and a stack address give bytes that are unlikely to repeat
/// across reconnects, which is all RFC 6455 4.1 asks of the key (its purpose
/// is to prevent a cached response, not to be a secret). Stated plainly
/// because it is a deliberate choice, not an oversight.
fn handshake_nonce() -> [u8; 16] {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut seed = Vec::with_capacity(32);
    seed.extend_from_slice(&now.as_secs().to_le_bytes());
    seed.extend_from_slice(&now.subsec_nanos().to_le_bytes());
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    let stack = &seed as *const _ as usize;
    seed.extend_from_slice(&stack.to_le_bytes());

    // Stretch through SHA-1 so the output looks uniform and is 16 bytes.
    let digest = sha1(&seed);
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    // -- SHA-1 vectors (RFC 3174) ----------------------------------------

    #[test]
    fn sha1_rfc3174_vectors() {
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            hex(&sha1(b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu")),
            "a49b2446a02c645bf419f995b67091253a04a259"
        );
    }

    // -- base64 vectors (RFC 4648) ---------------------------------------

    #[test]
    fn base64_rfc4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(
                base64_encode(input.as_bytes()),
                expected,
                "encode {input:?}"
            );
        }
    }

    // -- The RFC 6455 1.3 handshake vector -------------------------------

    #[test]
    fn rfc6455_example_accept() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    // -- Client-side masking (RFC 6455 5.1) ------------------------------

    #[test]
    fn client_frames_are_always_masked() {
        let f = encode_masked(opcode::TEXT, b"hi", [1, 2, 3, 4]);
        assert_eq!(f[0], 0x81, "FIN + text");
        assert_eq!(f[1] & 0x80, 0x80, "the mask bit must be set");
        assert_eq!(f[1] & 0x7F, 2, "payload length");
        // Masked payload, not the plaintext.
        assert_eq!(&f[6..], &[b'h' ^ 1, b'i' ^ 2]);
    }

    #[test]
    fn a_masked_frame_from_the_server_is_a_protocol_violation() {
        // Server frames must not be masked (RFC 6455 5.1). Build a masked one
        // by hand: FIN + text, mask bit set, 4-byte mask, 2 bytes payload.
        let raw = [0x81u8, 0x80 | 2, 1, 2, 3, 4, 0x60, 0x60];
        assert_eq!(
            decode_server_frame(&raw),
            Err(FrameError::MaskedFromServer),
            "a masked server frame must be refused, not tolerated"
        );
    }

    // -- Server frame decoding -------------------------------------------

    #[test]
    fn server_frame_length_forms_decode() {
        for len in [0usize, 1, 125, 126, 127, 1000, 65_535] {
            let payload = vec![0x5Au8; len];
            let mut raw = Vec::new();
            raw.push(0x81);
            if len < 126 {
                raw.push(len as u8);
            } else if len <= u16::MAX as usize {
                raw.push(126);
                raw.extend_from_slice(&(len as u16).to_be_bytes());
            } else {
                raw.push(127);
                raw.extend_from_slice(&(len as u64).to_be_bytes());
            }
            raw.extend_from_slice(&payload);
            let (frame, used) = decode_server_frame(&raw).expect("decodes");
            assert_eq!(used, raw.len(), "len {len}");
            assert_eq!(frame.payload, payload, "len {len}");
        }
    }

    #[test]
    fn the_64_bit_length_form_decodes() {
        let payload = vec![0x11u8; 65_536];
        let mut raw = vec![0x82u8, 127];
        raw.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        raw.extend_from_slice(&payload);
        let (frame, used) = decode_server_frame(&raw).expect("decodes");
        assert_eq!(used, raw.len());
        assert_eq!(frame.payload.len(), 65_536);
        assert_eq!(frame.opcode, opcode::BINARY);
    }

    #[test]
    fn non_minimal_lengths_are_refused() {
        // 125 in the 16-bit form.
        let mut raw = vec![0x81u8, 126, 0x00, 0x7D];
        raw.extend_from_slice(&[0u8; 125]);
        assert_eq!(decode_server_frame(&raw), Err(FrameError::BadLength));

        // 65535 in the 64-bit form.
        let mut raw = vec![0x81u8, 127];
        raw.extend_from_slice(&(65_535u64).to_be_bytes());
        raw.extend_from_slice(&[0u8; 65_535]);
        assert_eq!(decode_server_frame(&raw), Err(FrameError::BadLength));
    }

    #[test]
    fn reserved_bits_and_oversize_are_refused() {
        assert_eq!(
            decode_server_frame(&[0xC1u8, 0]),
            Err(FrameError::ReservedBits)
        );

        let mut raw = vec![0x81u8, 127];
        raw.extend_from_slice(&((MAX_FRAME as u64) + 1).to_be_bytes());
        assert_eq!(decode_server_frame(&raw), Err(FrameError::TooLarge));
    }

    #[test]
    fn a_partial_frame_reports_incomplete_at_every_cut() {
        let mut raw = vec![0x81u8, 5];
        raw.extend_from_slice(b"hello");
        for cut in 0..raw.len() {
            assert_eq!(
                decode_server_frame(&raw[..cut]),
                Err(FrameError::Incomplete),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn handshake_verification_rejects_a_wrong_accept() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let good = "HTTP/1.1 101 Switching Protocols\r\n\
                    Upgrade: websocket\r\n\
                    Connection: Upgrade\r\n\
                    Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n";
        assert!(verify_handshake(good, key).is_ok());

        let wrong = good.replace(
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        );
        assert!(
            verify_handshake(&wrong, key).is_err(),
            "a wrong accept must be refused"
        );

        let not_101 = "HTTP/1.1 200 OK\r\n\r\n";
        assert!(verify_handshake(not_101, key).is_err());

        let missing = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n";
        assert!(verify_handshake(missing, key).is_err());
    }

    #[test]
    fn the_nonce_is_16_bytes_and_used_as_the_mask() {
        let n = handshake_nonce();
        assert_eq!(n.len(), 16);
        assert_eq!(encode_nonce(&n).len(), 24, "16 bytes encode to 24 chars");
        // Two calls differ: a repeated key would let a peer cache the 101.
        let m = handshake_nonce();
        assert_ne!(n, m, "the nonce must not repeat across calls");
    }
}
