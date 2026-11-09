// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! WebSocket Notification Connection (PS3.18 8.10.4, RFC 6455).
//!
//! AIW-I RAD-109 opens the connection; RAD-87 carries Event Reports over it.
//! Both were PLANNED before this module existed; this is the conformance
//! channel the AIW-I statement names.
//!
//! # Why hand-rolled
//!
//! The house rule is zero new dependencies, and this server already owns its
//! HTTP/1.1 stack and its thread-per-connection model. A WebSocket connection
//! is an HTTP/1.1 connection that changes protocol after a `101`, so it slots
//! into the existing connection thread with no runtime and no library:
//!
//! - the ACCEPT step needs SHA-1 (RFC 3174) and base64 (RFC 4648). RFC 6455
//!   4.2.2 MANDATES SHA-1 here - it is a fixed protocol constant applied once
//!   per handshake with a magic GUID, not a general-purpose cryptographic
//!   primitive we are choosing. Both are small, self-contained, and tested
//!   against the published vectors.
//! - framing is length-prefixed and small; the server only ever sends unfragmented
//!   text frames (events are single JSON objects), and the client's frames are
//!   read and discarded.
//!
//! # Role: responder only
//!
//! This server never opens a WebSocket; it answers one. RFC 6455 4.1 (client
//! handshake) is therefore not implemented, and neither is client-side masking
//! of outgoing frames - 5.1 requires the SERVER to send unmasked frames.
//!
//! # Scope
//!
//! `Sec-WebSocket-Protocol` is deliberately not negotiated: our channel
//! carries JSON text frames and defines no subprotocol, so the response omits
//! the header rather than echoing an offer we cannot honour.

#![forbid(unsafe_code)]

// ---------------------------------------------------------------------------
// SHA-1 (RFC 3174) - required by RFC 6455 4.2.2 for Sec-WebSocket-Accept
// ---------------------------------------------------------------------------

/// Streaming-free SHA-1: the handshake digest is over a short, known header.
///
/// Returns the 20-byte digest. Callers pass `key || MAGIC_GUID`.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];

    // Message padding (RFC 3174 section 4): append 0x80, then zeros until the
    // length is 56 mod 64, then the bit length big-endian. Five blocks of
    // scratch space is enough for the single extra block the padding needs.
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
// base64 (RFC 4648 section 4) - standard alphabet, padding required
// ---------------------------------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode to standard base64 with `=` padding.
///
/// Writes directly into a caller-provided buffer, so the hot handshake path
/// needs no heap: the encoded length is known from the input length.
pub fn base64_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(base64_len(data.len()));
    base64_encode_into(data, &mut out);
    out
}

/// Encoded length for `n` input bytes, including padding.
#[must_use]
pub fn base64_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

/// Encode `data`, appending to `out`. Allocation-free.
pub fn base64_encode_into(data: &[u8], out: &mut Vec<u8>) {
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
}

/// Decode base64. Returns `None` on any invalid character, bad length or bad
/// padding - the handshake treats that as a malformed key.
#[must_use]
pub fn base64_decode(s: &[u8]) -> Option<Vec<u8>> {
    // Empty input is the empty string (RFC 4648 section 10), not an error.
    // A modulus of 1 is impossible: a 4-symbol group cannot encode that.
    if !s.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let groups = s.chunks_exact(4).count();
    for (group_index, chunk) in s.chunks_exact(4).enumerate() {
        // Padding is legal only in the LAST group: `Zg==Zg==` is two groups
        // with padding in the first, which is malformed.
        let is_last = group_index + 1 == groups;
        let mut vals = [0u32; 4];
        let mut pad = 0usize;
        for (i, &ch) in chunk.iter().enumerate() {
            if ch == b'=' {
                // Padding occupies only the last two positions of the group.
                if i < 2 || !is_last {
                    return None;
                }
                pad += 1;
                vals[i] = 0;
                continue;
            }
            if pad > 0 {
                return None; // real data after padding
            }
            vals[i] = u32::from(b64_value(ch)?);
        }
        let triple = (vals[0] << 18) | (vals[1] << 12) | (vals[2] << 6) | vals[3];
        out.push((triple >> 16) as u8);
        if pad < 2 {
            out.push((triple >> 8) as u8);
        }
        if pad < 1 {
            out.push(triple as u8);
        }
    }
    Some(out)
}

/// Value of one base64 character, or `None` if it is not in the alphabet.
fn b64_value(ch: u8) -> Option<u8> {
    match ch {
        b'A'..=b'Z' => Some(ch - b'A'),
        b'a'..=b'z' => Some(ch - b'a' + 26),
        b'0'..=b'9' => Some(ch - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Handshake (RFC 6455 4.2.2)
// ---------------------------------------------------------------------------

/// The GUID RFC 6455 4.2.2 concatenates with the client's key.
pub const WS_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The protocol version this server implements.
pub const WS_VERSION: &str = "13";

/// Compute `Sec-WebSocket-Accept` from the client's `Sec-WebSocket-Key`.
///
/// RFC 6455 4.2.2: base64(SHA-1(key + GUID)).
#[must_use]
pub fn accept_key(client_key: &str) -> String {
    let mut buf = Vec::with_capacity(client_key.len() + WS_GUID.len());
    buf.extend_from_slice(client_key.as_bytes());
    buf.extend_from_slice(WS_GUID);
    let digest = sha1(&buf);
    String::from_utf8(base64_encode(&digest)).expect("base64 of a digest is ASCII")
}

/// Whether a header value contains `token` as a comma-separated token,
/// case-insensitively (RFC 7230 7.4.1 / RFC 6455 4.2.1).
///
/// Token comparison rather than substring matching: a value like
/// `Connection: keep-alive, Upgrade` must match, while `Upgrade: websocketx`
/// must not.
#[must_use]
pub fn header_has_token(value: &[u8], token: &str) -> bool {
    let token = token.as_bytes();
    value.split(|&b| b == b',').any(|part| {
        let part = trim_ascii(part);
        part.len() == token.len() && part.eq_ignore_ascii_case(token)
    })
}

/// Trim leading/trailing spaces and tabs (HTTP optional whitespace).
fn trim_ascii(mut b: &[u8]) -> &[u8] {
    while let Some((first, rest)) = b.split_first() {
        if *first == b' ' || *first == b'\t' {
            b = rest;
        } else {
            break;
        }
    }
    while let Some((last, rest)) = b.split_last() {
        if *last == b' ' || *last == b'\t' {
            b = rest;
        } else {
            break;
        }
    }
    b
}

/// Outcome of inspecting a request that asked for an upgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handshake {
    /// 101: the accept value to echo. The connection stays open.
    Accept(String),
    /// 426 Upgrade Required, with the supported version to advertise.
    VersionUnsupported,
    /// 400: the key is missing or is not 16 bytes of base64.
    BadKey,
}

/// Decide the response for a WebSocket upgrade request.
///
/// `only_upgrade_header` is the `Sec-WebSocket-Key` value if the request
/// carried one; `version` likewise. The caller has already established that
/// the request targets the notification-connection resource.
#[must_use]
pub fn evaluate_upgrade(key: Option<&str>, version: Option<&str>) -> Handshake {
    // Version first: RFC 6455 4.2.2 requires 426 for an unsupported version,
    // and a client that gets 426 can retry correctly without re-reading its key.
    match version {
        Some(v) if trim_str(v) == WS_VERSION => {}
        _ => return Handshake::VersionUnsupported,
    }
    // The key must decode to exactly 16 bytes (RFC 6455 4.2.1).
    let Some(key) = key else {
        return Handshake::BadKey;
    };
    let key = trim_str(key);
    match base64_decode(key.as_bytes()) {
        Some(bytes) if bytes.len() == 16 => Handshake::Accept(accept_key(key)),
        _ => Handshake::BadKey,
    }
}

/// Trim ASCII whitespace from a header value.
fn trim_str(s: &str) -> &str {
    s.trim_matches(|c: char| c == ' ' || c == '\t')
}

// ---------------------------------------------------------------------------
// Framing (RFC 6455 section 5)
// ---------------------------------------------------------------------------

/// Opcodes this server cares about (RFC 6455 5.2).
pub mod opcode {
    /// Continuation frame.
    pub const CONTINUATION: u8 = 0x0;
    /// Text frame (UTF-8 payload) - the only data frame we send.
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

/// Largest frame payload this server will accept from a client.
///
/// The channel is for acknowledgements and control frames, not for data the
/// server consumes, so the cap is deliberately small. A client that exceeds it
/// has its channel closed rather than buffered.
pub const MAX_CLIENT_FRAME: usize = 64 * 1024;

/// One decoded frame from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// FIN bit: whether this is the final fragment.
    pub fin: bool,
    /// Opcode (see [`opcode`]).
    pub opcode: u8,
    /// Payload, already unmasked.
    pub payload: Vec<u8>,
}

/// Build a server-to-client frame: FIN set, no fragmentation, UNMASKED.
///
/// RFC 6455 5.1: a server MUST NOT mask the frames it sends.
#[must_use]
pub fn encode_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push(0x80 | (opcode & 0x0F)); // FIN + opcode

    let len = payload.len();
    if len < 126 {
        out.push(len as u8);
    } else if len <= u16::MAX as usize {
        out.push(126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}

/// Build a close frame carrying a status code (RFC 6455 5.5.1).
#[must_use]
pub fn encode_close(code: u16) -> Vec<u8> {
    encode_frame(opcode::CLOSE, &code.to_be_bytes())
}

/// Why a frame could not be decoded.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The frame is longer than [`MAX_CLIENT_FRAME`].
    TooLarge,
    /// A length field the protocol forbids (non-minimal encoding).
    BadLength,
    /// A client frame arrived unmasked (RFC 6455 5.1 requires masking).
    Unmasked,
    /// Reserved bits were set (no extensions negotiated).
    ReservedBits,
    /// The buffer does not yet hold the whole frame; read more.
    Incomplete,
}

/// Decode one client frame from `buf`.
///
/// On success returns the frame and how many bytes it occupied, so the caller
/// can drain them and keep the remainder.
pub fn decode_client_frame(buf: &[u8]) -> Result<(Frame, usize), FrameError> {
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

    let masked = b1 & 0x80 != 0;
    if !masked {
        // RFC 6455 5.1: a client MUST mask. An unmasked frame is a protocol
        // error, not something to tolerate.
        return Err(FrameError::Unmasked);
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
                return Err(FrameError::BadLength); // non-minimal encoding
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
            // The high bit must be 0 (RFC 6455 5.2) and the value must not fit
            // in the shorter form.
            if v & 0x8000_0000_0000_0000 != 0 {
                return Err(FrameError::BadLength);
            }
            if v <= u16::MAX as u64 {
                return Err(FrameError::BadLength); // non-minimal encoding
            }
            v as usize
        }
        n => n as usize,
    };

    if len > MAX_CLIENT_FRAME {
        return Err(FrameError::TooLarge);
    }

    if buf.len() < offset + 4 {
        return Err(FrameError::Incomplete);
    }
    let mask = [
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ];
    offset += 4;

    if buf.len() < offset + len {
        return Err(FrameError::Incomplete);
    }
    let mut payload = Vec::with_capacity(len);
    for (i, &byte) in buf[offset..offset + len].iter().enumerate() {
        payload.push(byte ^ mask[i % 4]);
    }
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

/// Build a masked client frame. TEST SUPPORT ONLY.
///
/// The server never sends masked frames (RFC 6455 5.1); wire tests need this
/// to speak the client half of the protocol.
#[must_use]
pub fn encode_client_frame_masked(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;

    // -- SHA-1 vectors (RFC 3174 / RFC 6455 1.3) --------------------------

    #[test]
    fn sha1_rfc3174_vectors() {
        // RFC 3174 section 7.3 and the classic short vectors.
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
        // 448-bit case forcing a second padding block.
        assert_eq!(
            hex(&sha1(b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu")),
            "a49b2446a02c645bf419f995b67091253a04a259"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    // -- base64 vectors (RFC 4648 section 10) -----------------------------

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
                String::from_utf8(base64_encode(input.as_bytes())).unwrap(),
                expected,
                "encode {input:?}"
            );
            assert_eq!(
                base64_decode(expected.as_bytes()).as_deref(),
                Some(input.as_bytes()),
                "decode {expected:?}"
            );
        }
    }

    #[test]
    fn base64_decode_rejects_malformed_input() {
        for bad in [
            "Zg=", "Zg===", "Z", "Zm9v====", "Zg==Zg==", "Zm9!", "=Zg=", "A=AA",
        ] {
            assert!(
                base64_decode(bad.as_bytes()).is_none(),
                "must reject {bad:?}"
            );
        }
    }

    #[test]
    fn base64_len_matches_the_encoder() {
        for n in 0..64usize {
            let data = vec![0xA5u8; n];
            assert_eq!(base64_encode(&data).len(), base64_len(n), "n={n}");
        }
    }

    // -- The RFC 6455 1.3 handshake vector --------------------------------

    #[test]
    fn rfc6455_example_accept() {
        // RFC 6455 1.3: key -> accept, byte for byte.
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    // -- Token matching (RFC 6455 4.2.1) ----------------------------------

    #[test]
    fn header_token_matching_is_exact_and_case_insensitive() {
        assert!(header_has_token(b"Upgrade", "upgrade"));
        assert!(header_has_token(b"keep-alive, Upgrade", "upgrade"));
        assert!(header_has_token(b"Upgrade, keep-alive", "upgrade"));
        assert!(header_has_token(b"  upgrade  ", "upgrade"));

        assert!(!header_has_token(b"websocketx", "websocket"));
        assert!(!header_has_token(b"no-upgrade", "upgrade"));
        assert!(!header_has_token(b"", "upgrade"));
    }

    // -- Handshake decision ----------------------------------------------

    #[test]
    fn valid_key_and_version_are_accepted() {
        match evaluate_upgrade(Some("dGhlIHNhbXBsZSBub25jZQ=="), Some("13")) {
            Handshake::Accept(a) => assert_eq!(a, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
            other => panic!("expected Accept, got {other:?}"),
        }
    }

    #[test]
    fn a_version_other_than_13_is_426() {
        for v in [None, Some("8"), Some("12"), Some(""), Some("13.0")] {
            assert_eq!(
                evaluate_upgrade(Some("dGhlIHNhbXBsZSBub25jZQ=="), v),
                Handshake::VersionUnsupported,
                "version {v:?}"
            );
        }
    }

    #[test]
    fn a_key_that_is_not_16_bytes_of_base64_is_400() {
        for key in [
            None,
            Some(""),
            Some("not-base64!!"),
            Some("Zm9v"),                     // 3 bytes
            Some("Zm9vYmFy"),                 // 6 bytes
            Some("dGhlIHNhbXBsZSBub25jZQ"),   // missing padding
            Some("AAAAAAAAAAAAAAAAAAAAAAAA"), // 18 bytes
        ] {
            assert_eq!(
                evaluate_upgrade(key, Some("13")),
                Handshake::BadKey,
                "key {key:?}"
            );
        }
    }

    // -- Framing ----------------------------------------------------------

    #[test]
    fn server_frames_are_unmasked_and_fin() {
        let f = encode_frame(opcode::TEXT, b"hi");
        assert_eq!(f, vec![0x81, 0x02, b'h', b'i']);

        // 126-byte payload -> 16-bit extended length.
        let f = encode_frame(opcode::TEXT, &[b'x'; 126]);
        assert_eq!(f[0], 0x81);
        assert_eq!(f[1], 126);
        assert_eq!(u16::from_be_bytes([f[2], f[3]]), 126);
        assert_eq!(f.len(), 4 + 126);
    }

    #[test]
    fn client_frame_round_trip() {
        let mask = [0x37, 0xFA, 0x21, 0x3D];
        let raw = encode_client_frame_masked(opcode::TEXT, b"Hello", mask);
        let (frame, used) = decode_client_frame(&raw).expect("decodes");
        assert_eq!(used, raw.len());
        assert!(frame.fin);
        assert_eq!(frame.opcode, opcode::TEXT);
        assert_eq!(frame.payload, b"Hello");
    }

    #[test]
    fn client_frame_length_forms() {
        let mask = [1, 2, 3, 4];
        for len in [0usize, 1, 125, 126, 127, 1000, 65_535] {
            let payload = vec![0x5Au8; len];
            let raw = encode_client_frame_masked(opcode::BINARY, &payload, mask);
            let (frame, used) = decode_client_frame(&raw).expect("decodes");
            assert_eq!(used, raw.len(), "len {len}");
            assert_eq!(frame.payload, payload, "len {len}");
        }
    }

    #[test]
    fn the_64_bit_length_form_is_handled() {
        // 65536 bytes needs the 64-bit form; the cap allows it.
        let mask = [9, 8, 7, 6];
        let payload = vec![0x11u8; 65_536];
        let raw = encode_client_frame_masked(opcode::BINARY, &payload, mask);
        assert_eq!(raw[1], 0x80 | 127);
        let (frame, used) = decode_client_frame(&raw).expect("decodes");
        assert_eq!(used, raw.len());
        assert_eq!(frame.payload.len(), 65_536);
    }

    #[test]
    fn an_unmasked_client_frame_is_refused() {
        // FIN + text, length 2, NO mask bit.
        let raw = [0x81u8, 0x02, b'h', b'i'];
        assert_eq!(decode_client_frame(&raw), Err(FrameError::Unmasked));
    }

    #[test]
    fn non_minimal_lengths_are_refused() {
        // 125 sent in the 16-bit form.
        let mut raw = vec![0x81u8, 0x80 | 126, 0x00, 0x7D];
        raw.extend_from_slice(&[1, 2, 3, 4]);
        raw.extend_from_slice(&[0u8; 125]);
        assert_eq!(decode_client_frame(&raw), Err(FrameError::BadLength));

        // 65535 sent in the 64-bit form.
        let mut raw = vec![0x81u8, 0x80 | 127];
        raw.extend_from_slice(&(65_535u64).to_be_bytes());
        raw.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(decode_client_frame(&raw), Err(FrameError::BadLength));
    }

    #[test]
    fn the_high_bit_of_a_64_bit_length_is_refused() {
        let mut raw = vec![0x81u8, 0x80 | 127];
        raw.extend_from_slice(&0x8000_0000_0000_0000u64.to_be_bytes());
        assert_eq!(decode_client_frame(&raw), Err(FrameError::BadLength));
    }

    #[test]
    fn reserved_bits_are_refused() {
        let raw = [0xC1u8, 0x80, 1, 2, 3, 4];
        assert_eq!(decode_client_frame(&raw), Err(FrameError::ReservedBits));
    }

    #[test]
    fn an_oversized_frame_is_refused() {
        let raw = [
            0x81u8,
            0x80 | 127,
            // MAX_CLIENT_FRAME + 1, in the 64-bit form.
        ];
        let mut raw = raw.to_vec();
        raw.extend_from_slice(&((MAX_CLIENT_FRAME as u64) + 1).to_be_bytes());
        assert_eq!(decode_client_frame(&raw), Err(FrameError::TooLarge));
    }

    #[test]
    fn a_partial_frame_reports_incomplete() {
        let mask = [1, 2, 3, 4];
        let raw = encode_client_frame_masked(opcode::TEXT, b"Hello", mask);
        for cut in 0..raw.len() {
            assert_eq!(
                decode_client_frame(&raw[..cut]),
                Err(FrameError::Incomplete),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn decode_reports_how_many_bytes_to_drain() {
        let mask = [1, 2, 3, 4];
        let mut buf = encode_client_frame_masked(opcode::TEXT, b"one", mask);
        let first_len = buf.len();
        buf.extend_from_slice(&encode_client_frame_masked(opcode::PING, b"", mask));
        let (frame, used) = decode_client_frame(&buf).expect("decodes");
        assert_eq!(frame.payload, b"one");
        assert_eq!(used, first_len, "only the first frame is consumed");
    }

    #[test]
    fn close_frame_carries_its_status_code() {
        let f = encode_close(1000);
        assert_eq!(f[0], 0x88);
        assert_eq!(f[1], 2);
        assert_eq!(u16::from_be_bytes([f[2], f[3]]), 1000);
    }
}
