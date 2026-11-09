// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! CBOR journal + plaintext log (B-008 / B-008b).
//!
//! Two outputs share one source, nginx-way:
//!
//! ```text
//! handlers --> SPSC ring of 64-byte tokens
//!                  |
//!                  +-> .sys/index.journal  (CBOR, machine: all events)
//!                  +-> logs/clarus.log     (plaintext, tail -f: subset)
//! ```
//!
//! The writer hotpath (a STOW/QIDO handler) pushes a fixed 64-byte token into
//! a ring buffer. The ring keeps every event off the heap: pushing copies a
//! `[u8; 64]` into a preallocated slot, bumps a counter - no `Mutex`, no `Vec`,
//! no `format!`. A reader drains tokens and turns them into the durable
//! outputs.
//!
//! # Token layout (64 bytes)
//!
//! ```text
//! 0..2   event_code (u16)
//! 2..3   level (u8, EventLevel as u8)
//! 3..7   user_token (u32, from auth::UserRegistry)
//! 7      ip_len (4 or 16)
//! 8..24  ip (16 bytes, zero-padded)
//! 24..26 payload_len (u16, 0..=38)
//! 26..64 payload (38 bytes, e.g. study UID)
//! ```
//!
//! The 38-byte payload carries one short field (a study UID or an error
//! reason). Longer data never enters the ring - it is the journal's contract
//! that referential detail lives elsewhere (the manifest/indexes).

/// Byte length of every journal token - chosen so one cache line holds two
/// tokens and a batch flush aligns with fsync cadence.
///
/// Justification (P3, in place of a bench): two tokens per line is a deliberate
/// density choice for flush alignment, and the token is separately
/// `#[repr(align(64))]`-pinned (see [`JournalToken`]) so a producer store can
/// never straddle two lines. Both are layout facts, checkable by the
/// `ap101_cacheline` gate; a timing bench would not defend either.
pub const TOKEN_LEN: usize = 64;

/// Maximum payload bytes inside a token.
pub const MAX_PAYLOAD: usize = 38;

/// Ring capacity in tokens - 8192 x 64 = 512 KB of bounded memory.
pub const RING_CAPACITY: usize = 8192;

/// Operating severity of an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EventLevel {
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

impl EventLevel {
    /// The `[LEVEL]` tag used in the plaintext log (nginx error.log style).
    pub fn tag(self) -> &'static str {
        match self {
            EventLevel::Debug => "DEBUG",
            EventLevel::Info => "INFO",
            EventLevel::Warn => "WARN",
            EventLevel::Error => "ERROR",
        }
    }
}

/// Machine-readable event identity.
///
/// Carries a numeric code (written into the token) and a short tag for the
/// plaintext log. The `level` lets the formatter decide whether an event
/// survives the plaintext subset (WARN/ERROR + key INFO).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventCode {
    /// Numeric code written into the CBOR token.
    pub code: u16,
    /// Severity.
    pub level: EventLevel,
    /// Human tag for the plaintext line.
    pub tag: &'static str,
}

impl EventCode {
    pub const STOW_ACCEPTED: EventCode = EventCode {
        code: 1,
        level: EventLevel::Info,
        tag: "STOW accepted",
    };
    pub const STOW_REJECTED: EventCode = EventCode {
        code: 2,
        level: EventLevel::Error,
        tag: "STOW rejected",
    };
    pub const STUDY_UID_GENERATED: EventCode = EventCode {
        code: 3,
        level: EventLevel::Info,
        tag: "StudyInstanceUID generated",
    };
    pub const BTREE_WRITE_FAILED: EventCode = EventCode {
        code: 4,
        level: EventLevel::Warn,
        tag: "BTREE write failed",
    };
    pub const PATIENT_ID_GENERATED: EventCode = EventCode {
        code: 5,
        level: EventLevel::Info,
        tag: "PatientID generated",
    };
    pub const PATIENT_NAME_GENERATED: EventCode = EventCode {
        code: 6,
        level: EventLevel::Info,
        tag: "PatientName generated",
    };
    pub const INDEX_ROLLOVER: EventCode = EventCode {
        code: 7,
        level: EventLevel::Info,
        tag: "Index rollover",
    };
    pub const INDEX_PRELOADED: EventCode = EventCode {
        code: 8,
        level: EventLevel::Info,
        tag: "Index preloaded",
    };
    pub const SHADOW_MISMATCH: EventCode = EventCode {
        code: 9,
        level: EventLevel::Error,
        tag: "SHADOW mismatch",
    };
    /// STOW per-request counters (STOW statuses).
    ///
    /// A SEPARATE code-10 token emitted after a STOW request finalizes; the
    /// original code-1 `STOW_ACCEPTED` (study UID) stays unchanged so the
    /// 38-byte payload contract is respected. Payload is 8 binary bytes
    /// `[stored u32 LE][idempotent u32 LE]`, rendered by `format_plaintext`
    /// as `stored=N idempotent=M`.
    pub const STOW_COUNTS: EventCode = EventCode {
        code: 10,
        level: EventLevel::Info,
        tag: "STOW accepted",
    };

    /// Recover a full [`EventCode`] from its numeric identity.
    ///
    /// The token stores only `(code, level)`; the descriptive tag is
    /// reconstructed here for plaintext formatting. Unknown codes fall back
    /// to a generic entry so a forward-written token still formats.
    pub fn from_code(code: u16, level: EventLevel) -> EventCode {
        match (code, level) {
            (1, _) => EventCode::STOW_ACCEPTED,
            (2, _) => EventCode::STOW_REJECTED,
            (3, _) => EventCode::STUDY_UID_GENERATED,
            (4, _) => EventCode::BTREE_WRITE_FAILED,
            (5, _) => EventCode::PATIENT_ID_GENERATED,
            (6, _) => EventCode::PATIENT_NAME_GENERATED,
            (7, _) => EventCode::INDEX_ROLLOVER,
            (8, _) => EventCode::INDEX_PRELOADED,
            (9, _) => EventCode::SHADOW_MISMATCH,
            (10, _) => EventCode::STOW_COUNTS,
            _ => EventCode {
                code,
                level,
                tag: "event",
            },
        }
    }
}

/// A fixed-size, single-writer single-reader ring of 64-byte tokens.
///
/// The writer owns [`RingWriter`], the reader owns [`RingReader`]. Tokens are
/// copied (never heap-allocated) into a preallocated boxed array. This
/// primitive is deliberately unsafe-free SPSC: it does not guard against two
/// concurrent writers - the caller must wrap it when shared across threads.
pub struct SpscRing {
    /// Fixed token slots.
    slots: Box<[JournalToken]>,
    /// Write index (mod capacity), owned by the writer.
    write: usize,
    /// Read index (mod capacity), owned by the reader.
    read: usize,
    /// Capacity as a power-of-two mask.
    mask: usize,
}

/// A single journal token - a raw 64-byte cell.
///
/// Built with [`make_token`] and pushed into a ring. Public so the server can
/// construct and move tokens across the writer/reader boundary.
///
/// `#[repr(align(64))]` keeps the 64-byte cell on one cache line so the
/// producer writing a token never splits a write across two lines. Since
/// `size_of == align_of == 64`, the `Box<[JournalToken]>` ring stride is
/// unchanged - zero extra memory for the aligned array.
///
/// Justification (P3, in place of a bench): adjacent ring cells written by
/// different producers sharing one cache line would false-share - every store
/// invalidates the line another core is writing, serialising the ring. Aligning
/// each cell is the standard remedy; a false-sharing micro-bench is
/// timing-flaky and would not change the layout it is meant to defend.
#[derive(Debug, Clone, Copy)]
#[repr(align(64))]
pub struct JournalToken {
    bytes: [u8; TOKEN_LEN],
}

impl Default for JournalToken {
    fn default() -> Self {
        Self {
            bytes: [0u8; TOKEN_LEN],
        }
    }
}

impl Default for SpscRing {
    fn default() -> Self {
        Self::new()
    }
}

impl SpscRing {
    /// Allocate a fresh ring of [`RING_CAPACITY`] tokens.
    pub fn new() -> Self {
        Self {
            slots: vec![JournalToken::default(); RING_CAPACITY].into_boxed_slice(),
            write: 0,
            read: 0,
            mask: RING_CAPACITY - 1,
        }
    }

    /// Number of tokens currently buffered.
    ///
    /// `write`/`read` are monotonic counters; their difference is the live
    /// count regardless of how many times the ring has wrapped.
    pub fn len(&self) -> usize {
        self.write.wrapping_sub(self.read)
    }

    pub fn is_empty(&self) -> bool {
        self.write == self.read
    }

    /// True if the ring is full (writer ahead of reader by capacity).
    pub fn is_full(&self) -> bool {
        self.write.wrapping_sub(self.read) >= RING_CAPACITY
    }

    /// Push a token, dropping the oldest if the ring is full.
    ///
    /// `drop_oldest` is the fail-open policy for the journal: a busy server
    /// never blocks a STOW because the log is full - it sacrifices the oldest
    /// buffered event instead. Returns `false` when an older token was dropped.
    pub fn push(&mut self, token: JournalToken) -> bool {
        let evicted = self.is_full();
        if evicted {
            // Advance the reader's watermark to evict the oldest buffered
            // token, keeping the live count bounded at RING_CAPACITY.
            self.read = self.read.wrapping_add(1);
        }
        self.slots[self.write & self.mask] = token;
        self.write = self.write.wrapping_add(1);
        // The caller needs to know an event was LOST: returning `true`
        // unconditionally made the fail-open eviction invisible, so an audit
        // trail with a hole looked identical to a complete one.
        !evicted
    }

    /// Pop the oldest token, if any, for the reader.
    pub fn pop(&mut self) -> Option<JournalToken> {
        if self.is_empty() {
            return None;
        }
        let t = self.slots[self.read & self.mask];
        self.read = self.read.wrapping_add(1);
        Some(t)
    }
}

/// Writer half of the ring - the handler hotpath.
pub struct RingWriter<'a> {
    ring: &'a mut SpscRing,
}

/// Reader half of the ring - drains tokens for durable output.
pub struct RingReader<'a> {
    ring: &'a mut SpscRing,
}

impl<'a> RingWriter<'a> {
    pub fn new(ring: &'a mut SpscRing) -> Self {
        Self { ring }
    }

    /// Push a token, allocating nothing.
    pub fn push(&mut self, token: JournalToken) -> bool {
        self.ring.push(token)
    }
}

impl<'a> RingReader<'a> {
    pub fn new(ring: &'a mut SpscRing) -> Self {
        Self { ring }
    }

    pub fn pop(&mut self) -> Option<JournalToken> {
        self.ring.pop()
    }
}

/// Build a token from its parts.
///
/// Allocates nothing: the payload slice is copied (up to [`MAX_PAYLOAD`]).
/// `ip` must be 4 (IPv4) or 16 (IPv6) bytes; anything else is stored empty.
pub fn make_token(code: EventCode, user_token: u32, ip: &[u8], payload: &[u8]) -> JournalToken {
    let mut t = JournalToken::default();
    t.bytes[0..2].copy_from_slice(&code.code.to_le_bytes());
    t.bytes[2] = code.level as u8;
    t.bytes[3..7].copy_from_slice(&user_token.to_le_bytes());
    match ip.len() {
        4 | 16 => {
            t.bytes[7] = ip.len() as u8;
            t.bytes[8..8 + ip.len()].copy_from_slice(ip);
        }
        _ => t.bytes[7] = 0,
    }
    let n = payload.len().min(MAX_PAYLOAD);
    t.bytes[24..26].copy_from_slice(&(n as u16).to_le_bytes());
    t.bytes[26..26 + n].copy_from_slice(&payload[..n]);
    t
}

/// The fields of a token, decoded for output.
#[derive(Debug, Clone)]
pub struct DecodedEvent {
    pub code: EventCode,
    pub user_token: u32,
    pub ip: [u8; 16],
    pub ip_len: u8,
    pub payload: [u8; MAX_PAYLOAD],
    pub payload_len: usize,
}

impl JournalToken {
    /// Decode a token back into its logical fields.
    pub fn decode(&self) -> DecodedEvent {
        let code = u16::from_le_bytes([self.bytes[0], self.bytes[1]]);
        let level = match self.bytes[2] {
            2 => EventLevel::Info,
            3 => EventLevel::Warn,
            4 => EventLevel::Error,
            _ => EventLevel::Debug,
        };
        let user_token =
            u32::from_le_bytes([self.bytes[3], self.bytes[4], self.bytes[5], self.bytes[6]]);
        let ip_len = self.bytes[7];
        let mut ip = [0u8; 16];
        ip[..ip_len.min(16) as usize].copy_from_slice(&self.bytes[8..8 + ip_len.min(16) as usize]);
        let payload_len = u16::from_le_bytes([self.bytes[24], self.bytes[25]]) as usize;
        let payload_len = payload_len.min(MAX_PAYLOAD);
        let mut payload = [0u8; MAX_PAYLOAD];
        payload[..payload_len].copy_from_slice(&self.bytes[26..26 + payload_len]);
        DecodedEvent {
            code: EventCode::from_code(code, level),
            user_token,
            ip,
            ip_len,
            payload,
            payload_len,
        }
    }
}

/// Format a decoded event as a single plaintext line (B-008b).
///
/// nginx error.log style: `YYYY/MM/DD HH:MM:SS [LEVEL] tag: field=value ...`
/// The output reuses `out` (a caller-provided `Vec<u8>`) to avoid churn.
pub fn format_plaintext(ev: &DecodedEvent, user_name: Option<&str>, out: &mut Vec<u8>) {
    out.clear();
    // Timestamp - LOCAL wall clock, the same source clog uses
    // (clarus_core::time::local_now), so every line in clarus.log shares one
    // timezone. The CBOR token is unaffected (epoch-based, no stamp).
    let now = crate::time::local_now();
    fmt_date(
        out,
        now.year as u32,
        now.month,
        now.day,
        now.hour,
        now.minute,
        now.second,
    );

    out.extend_from_slice(b" [");
    out.extend_from_slice(ev.code.level.tag().as_bytes());
    out.extend_from_slice(b"] ");
    out.extend_from_slice(ev.code.tag.as_bytes());

    // STOW_COUNTS (code 10): the payload is 8 binary bytes
    // `[stored u32 LE][idempotent u32 LE]`; render as `stored=N idempotent=M`
    // instead of raw payload bytes.
    if ev.code.code == 10 && ev.payload_len >= 8 {
        let stored =
            u32::from_le_bytes([ev.payload[0], ev.payload[1], ev.payload[2], ev.payload[3]]);
        let idempotent =
            u32::from_le_bytes([ev.payload[4], ev.payload[5], ev.payload[6], ev.payload[7]]);
        out.extend_from_slice(b" stored=");
        push_dec(out, stored);
        out.extend_from_slice(b" idempotent=");
        push_dec(out, idempotent);
    } else if ev.payload_len > 0 {
        // Payload (study UID / reason).
        out.push(b' ');
        out.extend_from_slice(&ev.payload[..ev.payload_len]);
    }

    // Client IP - dotted-quad for IPv4, colon-form for IPv6.
    if ev.ip_len == 4 {
        out.extend_from_slice(b" ip=");
        for (i, &b) in ev.ip[..4].iter().enumerate() {
            if i > 0 {
                out.push(b'.');
            }
            push_dec(out, b as u32);
        }
    } else if ev.ip_len == 16 {
        out.extend_from_slice(b" ip=");
        for (i, &b) in ev.ip[..16].iter().enumerate() {
            if i > 0 {
                out.push(b':');
            }
            push_hex(out, b);
        }
    }

    // User resolved from the registry for plaintext readability.
    if let Some(name) = user_name {
        out.extend_from_slice(b" user=");
        out.extend_from_slice(name.as_bytes());
    }
    out.push(b'\n');
}

/// Append `v` in decimal (used for dotted-quad IP octets and STOW counts).
///
/// The buffer is sized for `u32::MAX` (10 digits), not for an IP octet (3).
/// The same helper renders the STOW_COUNTS payload, where `stored` routinely
/// reaches four or more digits: with a 3-byte buffer `i` underflowed past zero
/// and the write panicked on the first count of 1000 or more, taking the reader
/// thread down.
fn push_dec(out: &mut Vec<u8>, v: u32) {
    let mut buf = [0u8; 10];
    let mut n = v;
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[i..]);
}

/// Append one byte as two lowercase hex digits (used for IPv6 groups).
fn push_hex(out: &mut Vec<u8>, b: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(HEX[(b >> 4) as usize]);
    out.push(HEX[(b & 0x0f) as usize]);
}

fn fmt_date(out: &mut Vec<u8>, y: u32, mo: u32, d: u32, h: u32, mi: u32, s: u32) {
    out.push(b'0' + (y / 1000 % 10) as u8);
    out.push(b'0' + (y / 100 % 10) as u8);
    out.push(b'0' + (y / 10 % 10) as u8);
    out.push(b'0' + (y % 10) as u8);
    out.push(b'/');
    push_two(out, mo);
    out.push(b'/');
    push_two(out, d);
    out.push(b' ');
    push_two(out, h);
    out.push(b':');
    push_two(out, mi);
    out.push(b':');
    push_two(out, s);
}

fn push_two(out: &mut Vec<u8>, v: u32) {
    out.push(b'0' + (v / 10 % 10) as u8);
    out.push(b'0' + (v % 10) as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_roundtrip() {
        let t = make_token(
            EventCode::STOW_ACCEPTED,
            0xDEAD_BEEF,
            &[192, 168, 1, 5],
            b"1.2.840.1.2.3",
        );
        let d = t.decode();
        assert_eq!(d.code.code, EventCode::STOW_ACCEPTED.code);
        assert_eq!(d.code.level, EventLevel::Info);
        assert_eq!(d.user_token, 0xDEAD_BEEF);
        assert_eq!(d.ip_len, 4);
        assert_eq!(&d.ip[..4], &[192, 168, 1, 5]);
        assert_eq!(&d.payload[..d.payload_len], b"1.2.840.1.2.3");
    }

    #[test]
    fn ring_buffers_fifo() {
        let mut ring = SpscRing::new();
        {
            let mut w = RingWriter::new(&mut ring);
            w.push(make_token(EventCode::STOW_ACCEPTED, 1, &[0; 4], b"a"));
            w.push(make_token(EventCode::STOW_REJECTED, 2, &[0; 4], b"b"));
        }
        let mut r = RingReader::new(&mut ring);
        let a = r.pop().expect("a");
        let b = r.pop().expect("b");
        assert!(r.pop().is_none());
        assert_eq!(a.decode().user_token, 1);
        assert_eq!(b.decode().user_token, 2);
    }

    #[test]
    fn plaintext_line_format() {
        let t = make_token(EventCode::STOW_REJECTED, 7, &[10, 0, 0, 1], b"disk full");
        let d = t.decode();
        let mut out = Vec::new();
        format_plaintext(&d, Some("dr.smith"), &mut out);
        let line = String::from_utf8(out).expect("utf8");
        assert!(line.contains("[ERROR]"), "got: {line}");
        assert!(line.contains("STOW rejected"));
        assert!(line.contains("disk full"));
        assert!(line.contains("user=dr.smith"));
        assert!(line.ends_with('\n'));
    }

    /// STOW_COUNTS (code 10): roundtrip the 8-byte `[stored u32][idempotent u32]`
    /// payload; plaintext renders `stored=N idempotent=M`.
    #[test]
    fn stow_counts_token_roundtrip_and_plaintext() {
        let mut payload = [0u8; 8];
        payload[0..4].copy_from_slice(&7u32.to_le_bytes()); // stored
        payload[4..8].copy_from_slice(&3u32.to_le_bytes()); // idempotent
        let t = make_token(EventCode::STOW_COUNTS, 42, &[127, 0, 0, 1], &payload);
        let d = t.decode();
        assert_eq!(d.code.code, 10);
        assert_eq!(d.code.level, EventLevel::Info);
        assert_eq!(&d.payload[..d.payload_len], &payload);

        let mut out = Vec::new();
        format_plaintext(&d, Some("alice"), &mut out);
        let line = String::from_utf8(out).expect("utf8");
        assert!(
            line.contains("[INFO] STOW accepted stored=7 idempotent=3"),
            "got: {line}"
        );
        assert!(line.contains("ip=127.0.0.1"));
        assert!(line.contains("user=alice"));
        assert!(line.ends_with('\n'));
    }

    /// code 10 decodes through `from_code` back to STOW_COUNTS.
    #[test]
    fn stow_counts_from_code() {
        let c = EventCode::from_code(10, EventLevel::Info);
        assert_eq!(c.code, 10);
        assert_eq!(c.tag, EventCode::STOW_COUNTS.tag);
    }

    /// A STOW of 1000+ instances must render, not panic.
    ///
    /// `push_dec` sized its buffer for an IP octet (3 digits). The same helper
    /// renders the STOW_COUNTS payload, where `stored` passes 999 as soon as a
    /// study is large; `i` then underflowed past zero and the writer thread
    /// panicked on the plaintext pass.
    #[test]
    fn stow_counts_above_999_render() {
        let mut payload = [0u8; 8];
        payload[0..4].copy_from_slice(&1234u32.to_le_bytes());
        payload[4..8].copy_from_slice(&1000u32.to_le_bytes());
        let t = make_token(EventCode::STOW_COUNTS, 7, &[10, 0, 0, 1], &payload);
        let mut out = Vec::new();
        format_plaintext(&t.decode(), None, &mut out);
        let line = String::from_utf8(out).expect("utf8");
        assert!(
            line.contains("stored=1234 idempotent=1000"),
            "four-digit counts must render: {line}"
        );
    }

    /// The ring reports an eviction instead of always answering `true`.
    #[test]
    fn push_reports_an_evicted_token() {
        let mut ring = SpscRing::new();
        let t = make_token(EventCode::STOW_REJECTED, 1, &[127, 0, 0, 1], b"x");
        for i in 0..RING_CAPACITY {
            assert!(ring.push(t), "no eviction while there is room (slot {i})");
        }
        assert!(
            !ring.push(t),
            "the token that evicts the oldest must report the loss"
        );
    }

    #[test]
    fn journal_token_is_one_cache_line() {
        use core::mem::{align_of, size_of};
        // size == align == 64: the 64-byte cell stays on a single cache line
        // and the ring array stride is unchanged (no extra memory).
        assert_eq!(align_of::<JournalToken>(), 64);
        assert_eq!(size_of::<JournalToken>(), 64);
        // Every field span (here the single bytes[64] buffer) must lie
        // entirely inside one 64B line - the raw bytes never straddle two.
        let spans = ferrite_core::cache_line_fields!(JournalToken, 64, bytes: [u8; TOKEN_LEN]);
        assert_eq!(spans.len(), 1, "expected exactly the bytes field");
        let span = spans[0];
        assert!(
            !span.crosses_line(),
            "JournalToken.{} spans two cache lines",
            span.name
        );
    }
}
