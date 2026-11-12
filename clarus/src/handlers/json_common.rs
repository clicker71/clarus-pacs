// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Shared DICOMweb JSON serialization helpers.
//!
//! Extracted from `qido_json.rs` and `ups/json.rs` to eliminate ~40 lines
//! of duplicate code.

#![forbid(unsafe_code)]

// ---------------------------------------------------------------------------
// Native (serde-free) JSON writers -.
//
// These write JSON bytes into a caller-owned `&mut Vec<u8>`. They are the
// canonical byte-writers shared by admin/pure.rs, ups/json.rs,
// stow_pure.rs, dispatch_streams.rs, audit_log.rs, stow_response.rs and
// capabilities.rs (each of which previously had a local copy).
//
// BYTE CONTRACTS (do not drift without re-freezing):
// - `push_json_str` is serde_json-exact for the positional/control range these
//   bodies emit: `\b`/`\f` become `\b`/`\f`, other C0 controls become `\u00xx`
//   lowercase hex, UTF-8 passes through verbatim.
// - `push_json_str_loose` is the legacy Family-B escaper (audit_log,
//   dispatch_streams, capabilities): it does NOT special-case `\b`/`\f`, so
//   those bytes become `\u0008`/`\u000c`. It is byte-identical to
//   `push_json_str` EXCEPT for inputs containing 0x08/0x0C. Kept separate
// until proves those never occur in Family-B inputs.
// ---------------------------------------------------------------------------

/// Append `s` as a JSON string (serde_json-exact; escapes `\b`/`\f`, lowercase
/// `\u00xx` for other C0 controls, UTF-8 verbatim).
pub fn push_json_str(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    for &b in s.as_bytes() {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0C => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x00..=0x1F => {
                out.extend_from_slice(b"\\u00");
                const LHEX: &[u8; 16] = b"0123456789abcdef";
                out.push(LHEX[(b >> 4) as usize]);
                out.push(LHEX[(b & 0xf) as usize]);
            }
            _ => out.push(b),
        }
    }
    out.push(b'"');
}

/// Legacy Family-B escaper (does NOT special-case `\b`/`\f`; those become
/// `\u0008`/`\u000c`). Byte-identical to `push_json_str` except for inputs
/// containing 0x08/0x0C. Used by the audit-log / logs-SSE / capabilities
/// writers whose frozen inputs never contain those bytes.
pub fn push_json_str_loose(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    for &b in s.as_bytes() {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x00..=0x1F => {
                out.extend_from_slice(b"\\u00");
                const LHEX: &[u8; 16] = b"0123456789abcdef";
                out.push(LHEX[(b >> 4) as usize]);
                out.push(LHEX[(b & 0xf) as usize]);
            }
            _ => out.push(b),
        }
    }
    out.push(b'"');
}

/// Append a `u64` in decimal (no leading zeros).
pub fn push_u64(out: &mut Vec<u8>, mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[i..]);
}

/// Append `v` as a JSON value, recursively.
///
/// The writer exists for the one place the server carries a subtree it does
/// not interpret: a UPS Scheduled Processing Parameters element (0074,1210)
/// is stored as JSON and handed back unchanged. Serializing the PARSED tree
/// is the only way to keep it without modelling every Content Item a site
/// may invent, and a canonical form is enough - the value that comes out is
/// what a re-parse of what came in would produce, not a byte copy of the
/// client's whitespace.
///
/// `Obj` keeps DOCUMENT ORDER rather than sorting: DICOM JSON attributes have
/// no required order, but a reader diffing two answers of the same workitem
/// expects them to match, and the parser already preserves the input order.
pub fn write_json_val(out: &mut Vec<u8>, v: &JsonVal) {
    match v {
        JsonVal::Null => out.extend_from_slice(b"null"),
        JsonVal::Bool(true) => out.extend_from_slice(b"true"),
        JsonVal::Bool(false) => out.extend_from_slice(b"false"),
        JsonVal::U64(n) => push_u64(out, *n),
        JsonVal::Str(s) => push_json_str(out, s),
        JsonVal::Arr(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_json_val(out, item);
            }
            out.push(b']');
        }
        JsonVal::Obj(pairs) => {
            out.push(b'{');
            for (index, (key, value)) in pairs.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                push_json_str(out, key);
                out.push(b':');
                write_json_val(out, value);
            }
            out.push(b'}');
        }
    }
}

/// Append `v` as a JSON number. Byte-exact with serde_json/ryu for the
/// positional-notation range these bodies emit; integral floats keep a `.0`
/// suffix (std `{}` drops it, so we add it back).
///
/// A non-finite value has no JSON spelling (ECMA-404 has no NaN/Infinity
/// literals), and emitting one would make the whole body unparseable, so it
/// becomes `null`. A width above the stack buffer is written straight from
/// the `String` instead of panicking: `debug_assert` is compiled out in
/// release, where a copy would have aborted the connection on a pathological
/// render.
pub fn push_f64(out: &mut Vec<u8>, v: f64) {
    if !v.is_finite() {
        out.extend_from_slice(b"null");
        return;
    }
    let s = format!("{}", v);
    if !s.contains('.') && !s.contains('e') && !s.contains('E') {
        out.extend_from_slice(s.as_bytes());
        out.extend_from_slice(b".0");
    } else {
        out.extend_from_slice(s.as_bytes());
    }
}

/// Append an `i64` in decimal (handles negatives; byte-exact number shape).
pub fn push_i64(out: &mut Vec<u8>, v: i64) {
    if v < 0 {
        out.push(b'-');
    }
    // `unsigned_abs` rather than `saturating_neg`: the latter clamps
    // `i64::MIN` to `i64::MAX`, so the smallest value in range printed as
    // 9223372036854775807 - a wrong number, not a clamped one. The magnitude
    // of a negative i64 fits a u64 exactly, which is what this returns.
    push_u64(out, v.unsigned_abs());
}

/// Append a JSON boolean (`true`/`false`).
pub fn push_bool(out: &mut Vec<u8>, v: bool) {
    out.extend_from_slice(if v { b"true" } else { b"false" });
}

/// Append a JSON `Option<u64>` as the number or `null`.
pub fn push_opt_u64(out: &mut Vec<u8>, v: Option<u64>) {
    match v {
        Some(n) => push_u64(out, n),
        None => out.extend_from_slice(b"null"),
    }
}

/// Append a JSON `Option<&str>` as a string or `null`.
pub fn push_opt_str(out: &mut Vec<u8>, v: Option<&str>) {
    match v {
        Some(s) => push_json_str(out, s),
        None => out.extend_from_slice(b"null"),
    }
}

// -- Minimal JSON object parser (request bodies) --
//
// Replaces `serde_json::from_slice` for the admin PUT/POST request bodies
// (MaintenanceRequest / MigrateRequest / DeleteRequest) and the UPS form-url
// decoder. This is intentionally a minimal, dependency-free JSON parser that
// reads only the top-level object fields the contracts need; it is NOT a full
// JSON DOM. Malformed JSON yields `None` (the caller falls back to defaults,
// matching the previous `.unwrap_or(default)` behaviour).

/// A parsed JSON value (native DOM; replaces the serde `Value` on
/// request-body paths). Recursive, so `extract_tag`/`extract_pn` can navigate
/// nested `{tag}.Value[0].Alphabetic`.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonVal {
    Null,
    Bool(bool),
    U64(u64),
    Str(String),
    Arr(Vec<JsonVal>),
    Obj(Vec<(String, JsonVal)>),
}

impl JsonVal {
    /// `JsonVal::Obj` -> `&JsonVal` for a top-level key (linear scan).
    pub fn obj_get<'a>(&'a self, key: &str) -> Option<&'a JsonVal> {
        match self {
            JsonVal::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    /// The top-level keys of an object, in document order.
    ///
    /// Used to check a request payload against the set of attributes a
    /// handler accepts: walking what the request actually carries is what
    /// turns "unknown element" into a refusal instead of a silent drop. A
    /// non-object yields an empty slice.
    #[must_use]
    pub fn keys(&self) -> Vec<&str> {
        match self {
            JsonVal::Obj(pairs) => pairs.iter().map(|(k, _)| k.as_str()).collect(),
            _ => Vec::new(),
        }
    }
    /// Whether an object has no top-level keys. A non-object counts as empty.
    #[must_use]
    pub fn is_empty_obj(&self) -> bool {
        match self {
            JsonVal::Obj(pairs) => pairs.is_empty(),
            _ => true,
        }
    }
    /// `JsonVal::Str` -> the string (if present).
    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonVal::Str(s) => Some(s),
            _ => None,
        }
    }
    /// `JsonVal::Arr` -> the first element (if present).
    pub fn first_of_array(&self) -> Option<&JsonVal> {
        match self {
            JsonVal::Arr(items) => items.first(),
            _ => None,
        }
    }
    /// `JsonVal::Arr` with a first string element -> that string.
    pub fn as_str_array_first(&self) -> Option<&str> {
        self.first_of_array()?.as_str()
    }
    /// `JsonVal::Arr` -> the element vector (if present).
    pub fn as_array(&self) -> Option<&Vec<JsonVal>> {
        match self {
            JsonVal::Arr(items) => Some(items),
            _ => None,
        }
    }
    /// `JsonVal::U64` -> the unsigned integer (if present).
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            JsonVal::U64(n) => Some(*n),
            _ => None,
        }
    }
    /// `JsonVal::U64` -> the value as `f64` (if present).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            JsonVal::U64(n) => Some(*n as f64),
            _ => None,
        }
    }
    /// `JsonVal::Bool` -> the boolean (if present).
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            JsonVal::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

/// Extract `JsonVal::Arr` of strings as `Vec<String>` (if every element is a
/// string); otherwise `None`.
pub fn json_str_array(v: &JsonVal) -> Option<Vec<String>> {
    match v {
        JsonVal::Arr(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(it.as_str()?.to_string());
            }
            Some(out)
        }
        _ => None,
    }
}

// -- application/x-www-form-urlencoded -----------

/// Encode a `&str` as a form-urlencoded field value (alnum + `-._~` kept;
/// space -> `+`; every other byte -> `%XX` uppercase). Matches serde_urlencoded
/// (which uses `url::form_urlencoded` under the hood) for the request
/// contracts used here.
pub fn form_encode_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0F) as usize] as char);
            }
        }
    }
    out
}

/// Decode a form-urlencoded field value (`+` -> space, `%XX` -> byte, then
/// UTF-8). Serde_urlencoded / `url::form_urlencoded` decode invalid UTF-8 as
/// the replacement character; we mirror that lossiness.
pub fn form_decode_str(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_val(bytes[i + 1]);
                let lo = hex_val(bytes[i + 2]);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decode one DICOMweb QUERY-parameter value: `%XX` only, `+` stays `+`.
///
/// PS3.18 8.3.1 lists `+` in `qp-special`, so it is a legal LITERAL character
/// in a query value and not the form-urlencoded space escape. Decoding it to a
/// space (`form_decode_str`, the WHATWG application/x-www-form-urlencoded rule)
/// corrupts every value that carries a plus - a PatientID of `A+B`, a signed
/// expression in a DT match - and the server then searches for a string the
/// requester never sent. A space travels as `%20`.
pub fn form_decode_query_str(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_val(bytes[i + 1]);
                let lo = hex_val(bytes[i + 2]);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Encode one DICOMweb QUERY-parameter value: everything outside the RFC 3986
/// unreserved set becomes `%XX`, spaces included.
///
/// The counterpart of `form_decode_query_str`: a space must travel as `%20`
/// (never `+`), because PS3.18 8.3.1 makes `+` a literal `qp-special`
/// character. Encoding it as `+` would decode back as a literal plus on any
/// conformant reader, so the pair would not round-trip.
pub fn form_encode_query_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0F) as usize] as char);
            }
        }
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// -- Error-body JSON writers --------------------------
//
// Byte-exact with the serde-era `serde_json::to_vec(&json!({...}))` error
// responses: compact JSON, keys sorted alphabetically (error, message,
// status), values escaped with `push_json_str` (Family A / serde_json-exact).
// These replace the residual live-path serde writers in ups_pure.rs and
// dispatch*.rs.

/// `{"error":"<err>"}` (no message).
pub fn err_json(err: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(err.len() + 16);
    out.extend_from_slice(b"{\"error\":");
    push_json_str(&mut out, err);
    out.push(b'}');
    out
}

/// `{"error":"<err>","message":"<msg>"}` (alphabetical key order).
pub fn err_msg_json(err: &str, msg: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(err.len() + msg.len() + 32);
    out.extend_from_slice(b"{\"error\":");
    push_json_str(&mut out, err);
    out.extend_from_slice(b",\"message\":");
    push_json_str(&mut out, msg);
    out.push(b'}');
    out
}

/// `{"error":"<err>","status":<status>}` (alphabetical key order).
pub fn err_status_json(err: &str, status: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(err.len() + 24);
    out.extend_from_slice(b"{\"error\":");
    push_json_str(&mut out, err);
    out.extend_from_slice(b",\"status\":");
    push_u64(&mut out, status as u64);
    out.push(b'}');
    out
}

/// Parse a JSON document into a [`JsonVal`].
///
/// Returns `None` on malformed JSON or invalid UTF-8. The top level may be any
/// value; callers that expect an object use [`JsonVal::Obj`].
pub fn parse_json(body: &[u8]) -> Option<JsonVal> {
    let text = std::str::from_utf8(body).ok()?;
    let mut p = JsonObjParser {
        bytes: text.as_bytes(),
        pos: 0,
        depth: 0,
    };
    p.skip_ws();
    let v = p.parse_value()?;
    p.skip_ws();
    // Must consume the whole input.
    if p.pos != text.len() {
        return None;
    }
    Some(v)
}

/// Parse a JSON object's top-level `"key": value` pairs from `body`.
///
/// Returns `None` on malformed JSON or a non-object top level (the previous
/// serde `.unwrap_or(default)` behaviour: bad bodies fall back to defaults).
pub fn parse_json_object(body: &[u8]) -> Option<Vec<(String, JsonVal)>> {
    match parse_json(body)? {
        JsonVal::Obj(pairs) => Some(pairs),
        _ => None,
    }
}

struct JsonObjParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    /// Current container nesting depth (`{` / `[`). Guarded against
    /// `MAX_JSON_DEPTH` so a deeply nested body cannot overflow the worker
    /// stack (see `MAX_JSON_DEPTH`).
    depth: usize,
}

/// Maximum nesting depth accepted by the recursive-descent parser.
///
/// The parser recurses once per `{`/`[`, so a body of `[[[[...` consumes one
/// stack frame per byte. The worker stack is 512 KiB (`serve.rs`
/// `WORKER_STACK`), and this cap keeps the deepest accepted document far
/// inside it. It is the same shape as the limit `serde_json` applied here
/// before the hand-written parser replaced it: a too-deep document is a
/// malformed document, not a crash.
const MAX_JSON_DEPTH: usize = 128;

impl<'a> JsonObjParser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }
    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }
    fn parse_string(&mut self) -> Option<String> {
        if self.peek() != Some(b'"') {
            return None;
        }
        self.pos += 1;
        let mut out = String::new();
        loop {
            let b = self.peek()?;
            match b {
                b'"' => {
                    self.pos += 1;
                    return Some(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let esc = self.peek()?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            if self.pos + 4 > self.bytes.len() {
                                return None;
                            }
                            let hex = &self.bytes[self.pos..self.pos + 4];
                            self.pos += 4;
                            let cp = hex_digits(hex)?;
                            // RFC 8259 7: a character above U+FFFF is written as
                            // a surrogate PAIR, and each half alone is not a
                            // scalar - `char::from_u32` rejects both, so a
                            // perfectly valid `\ud83d\ude00` (U+1F600) was
                            // refused as malformed JSON.
                            if (0xD800..=0xDBFF).contains(&cp) {
                                // High surrogate: the low half must follow
                                // immediately, or the pair is not a pair.
                                if self.bytes.get(self.pos..self.pos + 2) != Some(b"\\u") {
                                    return None;
                                }
                                if self.pos + 6 > self.bytes.len() {
                                    return None;
                                }
                                let lo = hex_digits(&self.bytes[self.pos + 2..self.pos + 6])?;
                                if !(0xDC00..=0xDFFF).contains(&lo) {
                                    return None;
                                }
                                self.pos += 6;
                                let combined = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                out.push(char::from_u32(combined)?);
                            } else if (0xDC00..=0xDFFF).contains(&cp) {
                                // A low surrogate with no high half before it.
                                return None;
                            } else {
                                out.push(char::from_u32(cp)?);
                            }
                        }
                        _ => return None,
                    }
                }
                0x80..=0xFF => {
                    // Multi-byte UTF-8: decode ONE scalar from the current
                    // offset.
                    //
                    // The width comes from the LEAD byte, so only that slice is
                    // validated. Decoding `&self.bytes[self.pos..]` - the whole
                    // remainder - is O(remaining) per non-ASCII byte, which
                    // makes a string of N Cyrillic/CJK characters O(N^2): a few
                    // MB of either held a worker thread for minutes. JSON is an
                    // untrusted body, so that was a cheap CPU DoS.
                    //
                    // RFC 3629 Table 3-7: 110xxxxx = 2 bytes, 1110xxxx = 3,
                    // 11110xxx = 4. Anything else (0x80..=0xBF continuation,
                    // 0xF8..=0xFF) is not a legal lead and must be refused
                    // here, before `from_utf8` could report it as a lone
                    // continuation.
                    let width = match b {
                        0xC0..=0xDF => 2,
                        0xE0..=0xEF => 3,
                        0xF0..=0xF7 => 4,
                        _ => return None,
                    };
                    let end = self.pos.checked_add(width)?;
                    let slice = self.bytes.get(self.pos..end)?;
                    // `from_utf8` on exactly one scalar's worth of bytes is
                    // O(1) and still rejects overlongs, surrogates and
                    // out-of-range values (RFC 3629).
                    let s = std::str::from_utf8(slice).ok()?;
                    let ch = s.chars().next()?;
                    out.push(ch);
                    self.pos = end;
                }
                _ => {
                    // A raw ASCII byte. Bytes below 0x80 are their own UTF-8
                    // encoding, so pushing the byte IS correct here - the
                    // 0x80..=0xFF arm above handles the rest.
                    out.push(b as char);
                    self.pos += 1;
                }
            }
        }
    }
    fn parse_value(&mut self) -> Option<JsonVal> {
        // Depth guard: everything below recurses, so the check sits at the
        // single recursion entry point. Scalars never reach it.
        if self.depth >= MAX_JSON_DEPTH {
            return None;
        }
        match self.peek()? {
            b'"' => Some(JsonVal::Str(self.parse_string()?)),
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b't' => {
                if self.bytes[self.pos..].starts_with(b"true") {
                    self.pos += 4;
                    Some(JsonVal::Bool(true))
                } else {
                    None
                }
            }
            b'f' => {
                if self.bytes[self.pos..].starts_with(b"false") {
                    self.pos += 5;
                    Some(JsonVal::Bool(false))
                } else {
                    None
                }
            }
            b'n' => {
                if self.bytes[self.pos..].starts_with(b"null") {
                    self.pos += 4;
                    Some(JsonVal::Null)
                } else {
                    None
                }
            }
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => None,
        }
    }
    fn parse_object(&mut self) -> Option<JsonVal> {
        self.pos += 1; // '{'
        self.skip_ws();
        let mut out = Vec::new();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Some(JsonVal::Obj(out));
        }
        // Entering a container costs one level; every exit path below must
        // give it back, or sibling containers would accumulate depth and a
        // long-but-shallow document would be rejected (see the module note on
        // `depth`).
        self.depth += 1;
        let result = loop {
            self.skip_ws();
            let Some(key) = self.parse_string() else {
                break None;
            };
            self.skip_ws();
            if self.peek() != Some(b':') {
                break None;
            }
            self.pos += 1;
            self.skip_ws();
            let Some(val) = self.parse_value() else {
                break None;
            };
            out.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break Some(JsonVal::Obj(out));
                }
                _ => break None,
            }
        };
        self.depth -= 1;
        result
    }
    fn parse_array(&mut self) -> Option<JsonVal> {
        self.pos += 1; // '['
        self.skip_ws();
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Some(JsonVal::Arr(items));
        }
        // See `parse_object`: the level is returned on every exit path.
        self.depth += 1;
        let result = loop {
            self.skip_ws();
            let Some(item) = self.parse_value() else {
                break None;
            };
            items.push(item);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    break Some(JsonVal::Arr(items));
                }
                _ => break None,
            }
        };
        self.depth -= 1;
        result
    }
    fn parse_number(&mut self) -> Option<JsonVal> {
        let start = self.pos;
        let mut is_unsigned = true;
        if self.peek() == Some(b'-') {
            is_unsigned = false;
            self.pos += 1;
        }
        while let Some(b) = self.peek() {
            match b {
                b'0'..=b'9' => self.pos += 1,
                b'.' | b'e' | b'E' | b'+' => {
                    is_unsigned = false;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let slice = &self.bytes[start..self.pos];
        let text = std::str::from_utf8(slice).ok()?;
        if matches!(
            self.peek(),
            Some(b'0'..=b'9') | Some(b'.') | Some(b'e') | Some(b'E')
        ) {
            // A malformed exponent/mantissa tail; refuse.
            return None;
        }
        if is_unsigned && let Ok(v) = text.parse::<u64>() {
            return Some(JsonVal::U64(v));
        }
        // Negative or float numbers aren't consumed by the request contracts
        // (they only read u64/bool/string/array), so treat them as unhandled.
        None
    }
}

/// Parse 4 uppercase/lowercase hex bytes into a u32 codepoint.
fn hex_digits(b: &[u8]) -> Option<u32> {
    let mut v: u32 = 0;
    for &c in b {
        let d = match c {
            b'0'..=b'9' => (c - b'0') as u32,
            b'a'..=b'f' => (c - b'a' + 10) as u32,
            b'A'..=b'F' => (c - b'A' + 10) as u32,
            _ => return None,
        };
        v = v * 16 + d;
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest i64 is the one value whose magnitude does not fit the
    /// signed type, so a negate-then-cast sequence clamps it: `saturating_neg`
    /// turned it into i64::MAX and the encoder printed a number 1.8e19 away
    /// from the input.
    #[test]
    fn push_i64_writes_the_smallest_value_exactly() {
        let mut out = Vec::new();
        push_i64(&mut out, i64::MIN);
        assert_eq!(String::from_utf8(out).unwrap(), "-9223372036854775808");
    }

    /// The ordinary negative path is unchanged by that fix.
    #[test]
    fn push_i64_writes_ordinary_negatives() {
        for (value, expected) in [
            (0i64, "0"),
            (-1, "-1"),
            (7, "7"),
            (-9223372036854775807, "-9223372036854775807"),
        ] {
            let mut out = Vec::new();
            push_i64(&mut out, value);
            assert_eq!(String::from_utf8(out).unwrap(), expected);
        }
    }

    // native JSON parser byte-parity with serde_json for the request
    // bodies the parsers consume (captured 19.08).
    #[test]
    fn parse_json_object_reads_flat_fields() {
        let body = br#"{"enable":true,"message":"Disk full","eta_minutes":15}"#;
        let pairs = parse_json_object(body).unwrap();
        let mut out = Vec::new();
        for (k, v) in &pairs {
            out.push(k.clone());
            match v {
                JsonVal::Bool(b) => out.push(format!("bool:{}", b)),
                JsonVal::U64(n) => out.push(format!("u64:{}", n)),
                JsonVal::Str(s) => out.push(format!("str:{}", s)),
                _ => out.push("other".to_string()),
            }
        }
        assert_eq!(
            out,
            vec![
                "enable".to_string(),
                "bool:true".to_string(),
                "message".to_string(),
                "str:Disk full".to_string(),
                "eta_minutes".to_string(),
                "u64:15".to_string(),
            ]
        );
    }

    #[test]
    fn parse_json_object_nested_and_array() {
        // UPS-style tag object: {"Value":[{"Alphabetic":"..."}],"vr":"PN"}.
        let body =
            br#"{"00100010":{"Value":[{"Alphabetic":"SMIRNOV^IVAN"}],"vr":"PN"},"00080060":{"Value":["CT"],"vr":"CS"}}"#;
        let obj = parse_json_object(body)
            .unwrap()
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        let tag = obj.get("00100010").unwrap();
        assert_eq!(
            tag.obj_get("Value")
                .unwrap()
                .first_of_array()
                .unwrap()
                .obj_get("Alphabetic")
                .unwrap()
                .as_str(),
            Some("SMIRNOV^IVAN")
        );
        let mod_tag = obj.get("00080060").unwrap();
        assert_eq!(
            mod_tag.obj_get("Value").unwrap().as_str_array_first(),
            Some("CT")
        );
    }

    #[test]
    fn json_str_array_flat_strings_only() {
        assert_eq!(
            json_str_array(&parse_json(br#"["1.2.3","4.5.6"]"#).unwrap()),
            Some(vec!["1.2.3".to_string(), "4.5.6".to_string()])
        );
        // Non-string element -> None (serde would reject a UID array too).
        let bad = br#"["1.2.3",5]"#;
        assert_eq!(json_str_array(&parse_json(bad).unwrap()), None);
    }

    #[test]
    fn parse_json_malformed_yields_none() {
        assert!(parse_json(b"{").is_none());
        assert!(parse_json(b"").is_none());
        assert!(parse_json(br#"[1,2"#).is_none());
        // Trailing garbage is rejected.
        assert!(parse_json(br#"{"a":1} x"#).is_none());
        // Non-object top level -> None for parse_json_object.
        assert!(parse_json_object(br#"[1,2]"#).is_none());
    }

    #[test]
    fn parse_json_deep_nesting_is_rejected_not_fatal() {
        // A body of 200k unclosed '[' used to recurse once per byte and blow
        // the 512 KiB worker stack (process abort). It must simply be
        // malformed JSON now.
        let bomb = vec![b'['; 200_000];
        assert!(parse_json(&bomb).is_none());
        // The same for objects, closed at the end so the failure is purely
        // the depth guard.
        let mut obj_bomb = vec![b'['; 200_000];
        obj_bomb.extend_from_slice(&vec![b']'; 200_000]);
        assert!(parse_json(&obj_bomb).is_none());
        // Depth just under the limit still parses (boundary check).
        let mut ok = vec![b'['; MAX_JSON_DEPTH - 1];
        ok.push(b'1');
        ok.extend_from_slice(&[b']'; MAX_JSON_DEPTH - 1]);
        assert!(parse_json(&ok).is_some());
        // One level deeper than the limit is refused.
        let mut over = vec![b'['; MAX_JSON_DEPTH];
        over.push(b'1');
        over.extend_from_slice(&[b']'; MAX_JSON_DEPTH]);
        assert!(parse_json(&over).is_none());
    }

    #[test]
    fn parse_json_long_shallow_document_is_not_depth_limited() {
        // Regression: the depth counter measures NESTING, not size. A body
        // with thousands of sibling containers - a QIDO response, a UPS tag
        // object - sits at depth 2-3 no matter how long it is, and must parse.
        // When the counter was incremented per container without returning the
        // level on exit, a 3 KB QIDO body of ~50 tag objects tripped the
        // limit and every QIDO-level test failed.
        let mut body = String::from("[");
        for n in 0..5_000 {
            if n > 0 {
                body.push(',');
            }
            body.push_str(
                r#"{"00080060":{"Value":["CT"],"vr":"CS"},"00100010":{"Value":[{"Alphabetic":"A^B"}],"vr":"PN"}}"#,
            );
        }
        body.push(']');
        assert!(
            body.len() > 20 * MAX_JSON_DEPTH,
            "the body must be far longer than the depth limit for this to mean anything"
        );
        let parsed = parse_json(body.as_bytes()).expect("long shallow body must parse");
        assert_eq!(parsed.as_array().map(|a| a.len()), Some(5_000));
    }

    #[test]
    fn parse_json_escapes() {
        // \u escapes, \n, escaped quote.
        let body = br#"{"msg":"a\u0041\n\"b\""}"#;
        let pairs = parse_json_object(body).unwrap();
        assert!(
            pairs
                .iter()
                .any(|(k, v)| k == "msg" && *v == JsonVal::Str("aA\n\"b\"".to_string()))
        );
    }

    /// A long non-ASCII string must decode in TIME proportional to its length.
    ///
    /// The decoder used to call `from_utf8` on the whole remaining slice for
    /// every non-ASCII byte, re-validating the tail each time: O(N^2). A JSON
    /// body is untrusted input, so a few MB of Cyrillic or CJK text pinned a
    /// worker for minutes. The width now comes from the lead byte and only that
    /// slice is validated, which is O(N).
    ///
    /// The assertion is a TIME bound on the whole parse, with a wide margin:
    /// the quadratic form needs many seconds for this input, the linear one
    /// milliseconds. A wall-clock bound can flake on a loaded machine, so the
    /// limit sits far above the linear cost and far below the quadratic one.
    #[test]
    fn parse_json_decodes_a_long_non_ascii_string_in_linear_time() {
        // 200k two-byte scalars (U+0410-U+042F, Cyrillic capitals) = ~400 KB.
        let mut unit = String::with_capacity(2);
        unit.push('\u{0410}');
        // Alternate to defeat any run-length shortcut and stay two-byte wide.
        let text: String = std::iter::repeat_n(unit.as_str(), 200_000).collect();
        assert_eq!(text.len(), 400_000);
        let body = format!("{{\"msg\":\"{text}\"}}");

        let started = std::time::Instant::now();
        let parsed = parse_json(body.as_bytes()).expect("long Cyrillic body parses");
        let elapsed = started.elapsed();

        match parsed {
            JsonVal::Obj(pairs) => {
                let msg = pairs.iter().find(|(k, _)| k == "msg").expect("msg key");
                assert_eq!(msg.1, JsonVal::Str(text.clone()));
            }
            other => panic!("expected an object, got {other:?}"),
        }
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "decoding 400 KB took {elapsed:?}: the per-byte re-validation is back"
        );
    }

    /// A lone continuation byte and an illegal lead are both refused.
    ///
    /// Taking the width from the lead byte put the rejection ON the width
    /// match, so the error cases must be pinned here: 0x80 alone is a
    /// continuation, 0xF8 is a 5-byte lead neither RFC 3629 nor this parser
    /// accepts, and a 2-byte lead whose tail is truncated must fail rather than
    /// silently consume the following bytes.
    #[test]
    fn parse_json_refuses_malformed_utf8_in_a_string() {
        // 0x80 (lone continuation) inside an otherwise valid string.
        let mut body = br#"{"msg":""#.to_vec();
        body.push(0x80);
        body.extend_from_slice(br#""}"#);
        assert!(parse_json(&body).is_none(), "lone continuation");

        // 0xF8 is a 5-byte lead: illegal in UTF-8.
        let mut body = br#"{"msg":""#.to_vec();
        body.push(0xF8);
        body.extend_from_slice(br#""}"#);
        assert!(parse_json(&body).is_none(), "5-byte lead");

        // A 2-byte lead with no room for its tail.
        let mut body = br#"{"msg":""#.to_vec();
        body.push(0xD0);
        body.extend_from_slice(br#""}"#);
        assert!(parse_json(&body).is_none(), "truncated sequence");

        // Overlong encoding of '/' (0xC0 0xAF) must be refused.
        let mut body = br#"{"msg":""#.to_vec();
        body.extend_from_slice(&[0xC0, 0xAF]);
        body.extend_from_slice(br#""}"#);
        assert!(parse_json(&body).is_none(), "overlong");
    }

    // category C: error-body writers are byte-exact with the serde-era    // `serde_json::to_vec(&json!({...}))` responses (captured 19.08 - compact,
    // alphabetical key order, values escaped via push_json_str = Family A).
    #[test]
    fn err_json_matches_serde_render() {
        assert_eq!(
            err_json("Internal server error"),
            br#"{"error":"Internal server error"}"#
        );
        assert_eq!(
            err_json("Workitem not found"),
            br#"{"error":"Workitem not found"}"#
        );
        // Message with a quote/backslash must be escaped exactly as serde.
        assert_eq!(err_json("a\"b\\c"), br#"{"error":"a\"b\\c"}"#);
    }

    #[test]
    fn err_msg_json_matches_serde_render() {
        assert_eq!(
            err_msg_json("Bad Request", "Invalid workitem UID"),
            br#"{"error":"Bad Request","message":"Invalid workitem UID"}"#
        );
        // Two-key body keeps alphabetical serde order (error < message).
        assert_eq!(
            err_msg_json("Conflict", "Workitem '1.2.3' already exists"),
            br#"{"error":"Conflict","message":"Workitem '1.2.3' already exists"}"#
        );
        // Escaped message (single quotes are literal; newline becomes \n).
        assert_eq!(
            err_msg_json("Bad Request", "line1\nline2\t\"q\""),
            br#"{"error":"Bad Request","message":"line1\nline2\t\"q\""}"#
        );
    }

    #[test]
    fn err_status_json_matches_serde_render() {
        assert_eq!(
            err_status_json("temp write: EIO", 500),
            br#"{"error":"temp write: EIO","status":500}"#
        );
        // Alphabetical serde order (error < status).
        assert_eq!(
            err_status_json("boom", 413),
            br#"{"error":"boom","status":413}"#
        );
    }

    /// ECMA-404 has no NaN or Infinity literal, so a non-finite value must
    /// become `null` - emitting it would make the body unparseable.
    #[test]
    fn a_non_finite_f64_is_null() {
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut out = Vec::new();
            push_f64(&mut out, v);
            assert_eq!(out, b"null", "{v} has no JSON spelling");
        }
    }

    /// A render wider than the old 32-byte stack buffer must not abort: the
    /// `debug_assert` guarding the copy is compiled out in release, where the
    /// copy would have panicked on a pathological value.
    #[test]
    fn a_wide_f64_renders_without_panicking() {
        let mut out = Vec::new();
        // std `{}` renders this in positional notation - several hundred
        // digits, well past the old 32-byte buffer.
        push_f64(&mut out, 1.234_567_890_123_456_7e-300);
        assert!(
            out.len() > 32,
            "the case under test must be wider than the stack buffer, got {}",
            out.len()
        );
        let text = std::str::from_utf8(&out).expect("valid UTF-8");
        assert!(
            text.starts_with("0.000000000000"),
            "the full positional form is emitted: {text}"
        );
        // The bytes are a JSON scalar with no nan/inf spelling.
        assert!(text.parse::<f64>().is_ok(), "still parseable: {text}");
    }

    /// The ordinary shape is unchanged: integral floats keep the `.0` suffix
    /// that std `{}` drops.
    #[test]
    fn an_integral_f64_keeps_its_fraction() {
        let mut out = Vec::new();
        push_f64(&mut out, 3.0);
        assert_eq!(out, b"3.0");
    }
}
