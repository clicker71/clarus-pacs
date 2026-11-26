// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! HTTP/1.1 request head parser (E-001 request line, E-002 headers).
//!
//! Zero allocation: borrows slices from the caller's read buffer. The buffer
//! is filled by `conn.rs`; on parse success the caller copies out any header
//! values it needs before the next `read()` overwrites the buffer.

use crate::http::conn::MAX_HEADERS;

/// HTTP method as a bare `u8`. Kept `#[repr(u8)]` so matching is a single load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Method {
    Get = 0,
    Post = 1,
    Put = 2,
    Delete = 3,
    Unknown = 4,
    /// CORS preflight (answered in the serve loop before routing).
    Options = 5,
    /// HEAD: routed like GET, body suppressed at the connection (curl -I).
    Head = 6,
}

/// Result of parsing a request head.
pub struct RequestHead<'a> {
    pub method: Method,
    /// Path portion of the request target (before `?`). Borrowed slice.
    pub path: &'a [u8],
    /// Query portion (after `?`), may be empty. Borrowed slice.
    pub query: &'a [u8],
    pub content_type: Option<&'a [u8]>,
    pub content_length: Option<u64>,
    pub transfer_encoding: Option<&'a [u8]>,
    pub x_real_ip: Option<&'a [u8]>,
    pub x_forwarded_for: Option<&'a [u8]>,
    /// `X-Forwarded-Proto` value (e.g. `https`). Borrowed; only honored when
    /// the peer is a trusted proxy (see `auth.rs` / `serve.rs`). Used to build
    /// QIDO RetrieveURL bases.
    pub x_forwarded_proto: Option<&'a [u8]>,
    /// `Host` header value. Borrowed. Used to build QIDO RetrieveURL bases;
    /// abscent -> RetrieveURL is not emitted.
    pub host: Option<&'a [u8]>,
    pub x_auth_user: Option<&'a [u8]>,
    /// The embedded viewer echoes the server's `x-clarus: definitum semel`
    /// stamp back on its requests (journal label only, not security).
    pub x_clarus: Option<&'a [u8]>,
    pub accept: Option<&'a [u8]>,
    pub range: Option<&'a [u8]>,
    /// `Upgrade` header value (RFC 9110 s7.8). The WebSocket handshake
    /// (RFC 6455 4.2.1) needs it to tell an upgrade request from a plain GET
    /// on the same path.
    pub upgrade: Option<&'a [u8]>,
    /// `Sec-WebSocket-Key` - the client nonce. Validated, not trusted: RFC
    /// 6455 4.2.1 requires exactly 16 bytes after base64 decoding.
    pub sec_websocket_key: Option<&'a [u8]>,
    /// `Sec-WebSocket-Version`. A value other than 13 draws a 426
    /// (RFC 6455 4.2.2), so an unsupported client learns what to retry with.
    pub sec_websocket_version: Option<&'a [u8]>,
    /// `Sec-WebSocket-Protocol` - the client's subprotocol offers. Recorded so
    /// the response can be checked against them; this server negotiates none.
    pub sec_websocket_protocol: Option<&'a [u8]>,
    /// `Connection: close` present?
    pub connection_close: bool,
    /// `Connection` header contains the `upgrade` token (RFC 6455 4.2.1).
    ///
    /// A separate flag rather than the raw value: the header is a comma list
    /// in which `upgrade` is one token among several, and the only question
    /// asked of it is membership - the same shape as `connection_close`.
    pub connection_upgrade: bool,
    /// `Expect: 100-continue` present (RFC 9110 s10.1.1).
    pub expect_100_continue: bool,
    /// HTTP/1.0 (keep-alive is opt-in; we default to close for 1.0 when no
    /// `Connection: keep-alive`).
    pub http10: bool,
}

impl RequestHead<'_> {
    /// Infallible default - borrows for the error/empty path.
    pub fn borrow_empty() -> RequestHead<'static> {
        RequestHead {
            method: Method::Unknown,
            path: &[],
            query: &[],
            content_type: None,
            content_length: None,
            transfer_encoding: None,
            x_real_ip: None,
            x_forwarded_for: None,
            x_forwarded_proto: None,
            host: None,
            x_auth_user: None,
            x_clarus: None,
            accept: None,
            range: None,
            upgrade: None,
            sec_websocket_key: None,
            sec_websocket_version: None,
            sec_websocket_protocol: None,
            connection_close: false,
            connection_upgrade: false,
            expect_100_continue: false,
            http10: false,
        }
    }
}

/// Error codes the parser can produce. The connection loop maps these to
/// HTTP status replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Line/head longer than the fixed cap - 431.
    HeadTooLarge,
    /// Line longer than 4 KB - 431.
    LineTooLarge,
    /// Malformed request line (bad method/version/target) - 400.
    BadRequestLine,
    /// `Content-Length` alongside `Transfer-Encoding` (smuggling) - 400.
    ConflictingLength,
    /// A header the parser requires to be unique appeared twice
    /// (`Content-Length`, `Transfer-Encoding`) - 400. RFC 9112 Section 6.3
    /// allows a server to reject a repeated `Content-Length` outright instead
    /// of trying to decide whether the two values agree.
    DuplicateHeader,
    /// Too many headers - 400.
    TooManyHeaders,
    /// Not CRLF-terminated / stray CR in FWS - 400.
    BadCrlf,
    /// `Content-Length` not a valid decimal - 400.
    /// A `Content-Length` value that is not a plain decimal number - 400.
    BadContentLength,
    /// A `Transfer-Encoding` value other than `chunked` - 400. This server
    /// decodes exactly one transfer coding (RFC 9112 6.1); anything else would
    /// have been framed as chunked and decoded as garbage.
    BadTransferEncoding,
    /// Incomplete head - caller must `read()` more.
    Incomplete,
}

/// Parse the request line from `buf[..len]`. Returns the parsed head and the
/// offset just past the CRLF that ends the line (so the caller can continue
/// parsing headers from that offset).
pub fn parse_request_line(buf: &[u8]) -> Result<(RequestHead<'_>, usize), ParseError> {
    // Find the CRLF that ends the request line (single pass, SIMD).
    let line_end = find_line_end(buf).ok_or_else(|| line_end_error(buf))?;
    let line = &buf[..line_end];
    if line.len() > crate::http::conn::HTTP_LINE_LIMIT {
        return Err(ParseError::LineTooLarge);
    }

    // method SP target SP version
    let (method, rest) = split_space(line).ok_or(ParseError::BadRequestLine)?;
    let (target, version) = split_space(rest).ok_or(ParseError::BadRequestLine)?;

    // RFC 9112 3: method and request-target are both nonempty. An empty method
    // fell through to `Method::Unknown` and an empty target parsed cleanly, so
    // `" HTTP/1.1"` was a valid request line - a shape no client sends and no
    // route can answer. An unknown but WELL-FORMED method token (PATCH, say)
    // stays distinct: it is unsupported, not malformed.
    if method.is_empty() || target.is_empty() {
        return Err(ParseError::BadRequestLine);
    }

    let method = match method {
        b"GET" => Method::Get,
        b"POST" => Method::Post,
        b"PUT" => Method::Put,
        b"DELETE" => Method::Delete,
        b"OPTIONS" => Method::Options,
        b"HEAD" => Method::Head,
        _ => Method::Unknown,
    };

    let http10 = match version {
        b"HTTP/1.1" => false,
        b"HTTP/1.0" => true,
        _ => return Err(ParseError::BadRequestLine), // incl. HTTP/0.9 (no version)
    };

    // RFC 9112 3.2.2: a server MUST accept absolute-form. Keeping the scheme
    // and authority in `path` meant `GET http://host/health` routed to
    // `//host/health`, which every route rejected as a malformed path. The
    // authority is what the Host field would have carried, so it becomes the
    // effective host; origin-form (`/health`) is untouched.
    let mut head = RequestHead::borrow_empty();
    let target = strip_absolute_form(target, &mut head);

    // Split target into path + query on '?'.
    let (path, query) = match target.iter().position(|&b| b == b'?') {
        Some(i) => (&target[..i], &target[i + 1..]),
        None => (target, &[][..]),
    };

    head.method = method;
    head.path = path;
    head.query = query;
    head.http10 = http10;
    Ok((head, line_end))
}

/// Rewrite an absolute-form request target into origin-form.
///
/// `scheme://authority/path` becomes `/path`, and the authority is recorded as
/// the effective host. A target that is not absolute-form is returned
/// unchanged. An absolute-form target with no path yields `/`, which is what
/// RFC 9110 4.2.3 defines as the empty-path absolute URI.
fn strip_absolute_form<'a>(target: &'a [u8], head: &mut RequestHead<'a>) -> &'a [u8] {
    let Some(scheme_end) = target.windows(3).position(|w| w == b"://") else {
        return target;
    };
    let scheme = &target[..scheme_end];
    // RFC 3986 3.1: the scheme is `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`.
    // Requiring it keeps a path that merely contains `://` (a DICOM query
    // value can) from being rewritten.
    if scheme.is_empty()
        || !scheme[0].is_ascii_alphabetic()
        || !scheme
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.')
    {
        return target;
    }
    let rest = &target[scheme_end + 3..];
    // The authority ends at the first `/`, `?` or `#`.
    let auth_end = rest
        .iter()
        .position(|&b| b == b'/' || b == b'?' || b == b'#')
        .unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    if authority.is_empty() {
        return target;
    }
    head.host = Some(authority);
    let after = &rest[auth_end..];
    if after.first() == Some(&b'/') {
        after
    } else {
        // No path: `?query` starts an empty path at the root.
        b"/".as_slice()
    }
}

/// Parse headers from `headers[..len]` into a fixed array.
///
/// `headers` must begin immediately after the request line's CRLF.
/// Returns the parsed headers and how many bytes of `headers` were consumed
/// (the position right after the final empty line's CRLF).
pub fn parse_headers<'a>(
    head: &mut RequestHead<'a>,
    headers: &'a [u8],
) -> Result<usize, ParseError> {
    let mut pos = 0usize;
    let mut count = 0usize;
    // Which of the two framing headers have been seen. A `u8` bitfield: no
    // allocation, and it makes the smuggling gates below a pair of bit tests
    // instead of a scan of headers already parsed.
    let mut seen = SeenFraming::default();
    while count < MAX_HEADERS {
        let line_end = match find_line_end(&headers[pos..]) {
            Some(e) => pos + e,
            None => return Err(line_end_error(&headers[pos..])),
        };
        let line = &headers[pos..line_end];
        if line.is_empty() {
            // Blank line: end of headers. Consume CRLF too.
            return Ok(line_end + 2);
        }
        apply_header(head, line, &mut seen)?;
        count += 1;
        pos = line_end + 2;
    }
    // The loop stops with `MAX_HEADERS` parsed. The next line decides: a blank
    // line is the END of a valid `MAX_HEADERS`-header request, anything else is
    // a 65th header. Treating the blank line as a header made an EXACTLY
    // `MAX_HEADERS` request fail with TooManyHeaders - the off-by-one.
    match find_line_end(&headers[pos..]) {
        Some(e) if headers[pos..pos + e].is_empty() => Ok(pos + e + 2),
        Some(_) => Err(ParseError::TooManyHeaders),
        None => Err(line_end_error(&headers[pos..])),
    }
}

/// Framing headers already seen, as a bitfield.
///
/// Both `Content-Length` and `Transfer-Encoding` are "singleton" fields: a
/// request that repeats one, or presents both, is a request-smuggling shape
/// (RFC 9112 Section 6.3, Section 7.1). Tracking them here keeps the check on
/// the header path free of allocation and free of extra passes.
#[derive(Default, Clone, Copy)]
struct SeenFraming {
    content_length: bool,
    transfer_encoding: bool,
}

/// Apply one `Name: value` header line to the parsed head. Case-insensitive
/// name match. Unknown headers are ignored (we only keep what we need).
///
/// This is where the smuggling gates live: a second `Content-Length`, a second
/// `Transfer-Encoding`, or `Content-Length` and `Transfer-Encoding` together
/// are all rejected, so no later code can be handed two disagreeing framings.
fn apply_header<'a>(
    head: &mut RequestHead<'a>,
    line: &'a [u8],
    seen: &mut SeenFraming,
) -> Result<(), ParseError> {
    let colon = line
        .iter()
        .position(|&b| b == b':')
        .ok_or(ParseError::BadCrlf)?;
    let name = &line[..colon];
    // RFC 9110 5.1: a field name is a nonempty `token`. A line whose first
    // byte is `:` has an EMPTY name, which used to pass the whitespace check
    // below and then match no branch - the line was silently dropped, so the
    // only record of it was the absence of any effect.
    if name.is_empty() {
        return Err(ParseError::BadCrlf);
    }
    // RFC 9110 5.1: no whitespace between the field name and the colon. This
    // server sits behind proxies that normalise the head, so a padded name is
    // never something an honest client produced - it is the classic way to make
    // one parser see `Content-Length` and another see nothing at all.
    if name
        .iter()
        .any(|&b| b == b' ' || b == b'\t' || b == b'\r' || b == b'\n' || b == b'\0')
    {
        return Err(ParseError::BadCrlf);
    }
    let mut value = &line[colon + 1..];
    // Trim leading/trailing OWS (SP/HTAB).
    while value.first() == Some(&b' ') || value.first() == Some(&b'\t') {
        value = &value[1..];
    }
    while value.last() == Some(&b' ') || value.last() == Some(&b'\t') {
        value = &value[..value.len() - 1];
    }

    // RFC 9110 5.1: field names are case-insensitive. Matching them
    // case-sensitively meant an HTTP/2-style lowercase head (`content-length`,
    // `transfer-encoding`, `host`, `x-real-ip`) was silently IGNORED: the
    // framing headers vanished, so the Content-Length/Transfer-Encoding
    // smuggling check never fired and the identity headers were dropped.
    if name.eq_ignore_ascii_case(b"Content-Type") {
        head.content_type = Some(value);
    } else if name.eq_ignore_ascii_case(b"Content-Length") {
        // A repeated Content-Length is refused whether or not the two
        // values agree: agreeing values are still ambiguous proxying
        // material, and RFC 9112 Section 6.3 explicitly permits the
        // reject.
        if seen.content_length {
            return Err(ParseError::DuplicateHeader);
        }
        if seen.transfer_encoding {
            return Err(ParseError::ConflictingLength);
        }
        seen.content_length = true;
        head.content_length =
            Some(parse_content_length(value).ok_or(ParseError::BadContentLength)?);
    } else if name.eq_ignore_ascii_case(b"Transfer-Encoding") {
        if seen.transfer_encoding {
            return Err(ParseError::DuplicateHeader);
        }
        if seen.content_length {
            return Err(ParseError::ConflictingLength);
        }
        seen.transfer_encoding = true;
        // RFC 9112 6.1/7.1: `chunked` is the only transfer coding a server
        // decodes, and it must be the FINAL coding. Accepting the header
        // without reading its value meant any spelling - `identity`,
        // `gzip`, `chunked, gzip`, an empty value - switched the body reader
        // into chunked framing. A body that was never chunked then failed
        // deep inside the reader as a malformed chunk, and a body with a
        // declared coding this server cannot decode was read as if the
        // coding were absent.
        if !value.eq_ignore_ascii_case(b"chunked") {
            return Err(ParseError::BadTransferEncoding);
        }
        head.transfer_encoding = Some(value);
    } else if name.eq_ignore_ascii_case(b"X-Real-IP") {
        head.x_real_ip = Some(value);
    } else if name.eq_ignore_ascii_case(b"X-Forwarded-For") {
        head.x_forwarded_for = Some(value);
    } else if name.eq_ignore_ascii_case(b"X-Forwarded-Proto") {
        head.x_forwarded_proto = Some(value);
    } else if name.eq_ignore_ascii_case(b"Host") {
        // RFC 9112 3.2: a request with more than one Host field is a 400, even
        // when the values agree - two Host fields are two different routing
        // decisions for two parsers, which is the shape request smuggling
        // relies on. Choosing one (the old behaviour) hid the disagreement.
        if head.host.is_some() {
            return Err(ParseError::DuplicateHeader);
        }
        head.host = Some(value);
    } else if name.eq_ignore_ascii_case(b"X-Authenticated-User") {
        head.x_auth_user = Some(value);
    } else if name.eq_ignore_ascii_case(b"x-clarus") {
        head.x_clarus = Some(value);
    } else if name.eq_ignore_ascii_case(b"Accept") {
        head.accept = Some(value);
    } else if name.eq_ignore_ascii_case(b"Range") {
        head.range = Some(value);
    } else if name.eq_ignore_ascii_case(b"Upgrade") {
        head.upgrade = Some(value);
    } else if name.eq_ignore_ascii_case(b"Sec-WebSocket-Key") {
        head.sec_websocket_key = Some(value);
    } else if name.eq_ignore_ascii_case(b"Sec-WebSocket-Version") {
        head.sec_websocket_version = Some(value);
    } else if name.eq_ignore_ascii_case(b"Sec-WebSocket-Protocol") {
        head.sec_websocket_protocol = Some(value);
    } else if name.eq_ignore_ascii_case(b"Connection") {
        // RFC 9110: repeated `Connection` fields combine into ONE token list,
        // so each occurrence ADDS its tokens. Assigning (`=`) made a later
        // field clear the flag an earlier one set - `Connection: close` then
        // `Connection: keep-alive` dropped `close` and vice versa.
        head.connection_close |= has_token(value, b"close");
        head.connection_upgrade |= has_token(value, b"upgrade");
    } else if name.eq_ignore_ascii_case(b"Expect") {
        // RFC 9110 10.1.1: a repeated `Expect` field combines into one list.
        // Assigning cleared the flag a previous field set, so
        // `Expect: 100-continue` followed by any other `Expect` made the server
        // stop waiting while the client still waited for the interim response.
        head.expect_100_continue |= has_token(value, b"100-continue");
    }
    Ok(())
}

/// Parse a decimal Content-Length, rejecting junk and signs.
///
/// RFC 9110 8.6 defines the value as `1*DIGIT`, so `01` denotes one octet and
/// is legal: the leading-zero rule this used to apply came from an older
/// anti-smuggling reading and rejected a conforming request. Signs stay
/// rejected, checked arithmetic stays, and repeated or conflicting framings
/// are refused by [`apply_header`] before this runs.
fn parse_content_length(v: &[u8]) -> Option<u64> {
    if v.is_empty() {
        return None;
    }
    let mut n: u64 = 0;
    for &b in v {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as u64)?;
    }
    Some(n)
}

/// Find the `\r\n` terminator for the line starting at `buf`.
/// Returns the index of the `\r` (the line content length), or `None`.
fn find_line_end(buf: &[u8]) -> Option<usize> {
    let lf = memchr::memchr(b'\n', buf)?;
    if lf == 0 || buf[lf - 1] != b'\r' {
        // A lone LF (no preceding CR) is a strict-CRLF violation.
        return None;
    }
    // Reject any stray CR/LF before the terminator (header injection).
    if memchr::memchr(b'\r', &buf[..lf - 1]).is_some() {
        return None;
    }
    Some(lf - 1)
}

/// Classify why [`find_line_end`] found no terminator.
///
/// Both public entry points used to `assert!` that the caller's slice fit the
/// header buffer and then report `Incomplete` for everything else. A caller
/// that passed `HEADER_BUF_SIZE + 3` bytes panicked on input that was merely
/// long; and a line that already carried a bare LF was reported `Incomplete`,
/// telling the caller to read more when appending bytes could never repair it.
fn line_end_error(buf: &[u8]) -> ParseError {
    if buf.len() > crate::http::conn::HEADER_BUF_SIZE + 2 {
        return ParseError::LineTooLarge;
    }
    // A terminator that exists but is not CRLF, or a stray CR before it, is
    // malformed NOW - more input cannot make it valid.
    if let Some(lf) = memchr::memchr(b'\n', buf)
        && (lf == 0 || buf[lf - 1] != b'\r' || memchr::memchr(b'\r', &buf[..lf - 1]).is_some())
    {
        return ParseError::BadCrlf;
    }
    ParseError::Incomplete
}

/// Split at the first ASCII SP. Returns `(left, right_after_sp)` or `None`.
fn split_space(buf: &[u8]) -> Option<(&[u8], &[u8])> {
    let i = buf.iter().position(|&b| b == b' ')?;
    Some((&buf[..i], &buf[i + 1..]))
}

/// Does a comma-separated header value contain `token` (case-insensitive)?
fn has_token(value: &[u8], token: &[u8]) -> bool {
    for part in value.split(|&b| b == b',') {
        let part = trim_ows(part);
        if part.eq_ignore_ascii_case(token) {
            return true;
        }
    }
    false
}

fn trim_ows(mut v: &[u8]) -> &[u8] {
    while v.first() == Some(&b' ') || v.first() == Some(&b'\t') {
        v = &v[1..];
    }
    while v.last() == Some(&b' ') || v.last() == Some(&b'\t') {
        v = &v[..v.len() - 1];
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_get_request_line() {
        let (h, line_end) =
            parse_request_line(b"GET /dicomweb/studies?limit=5 HTTP/1.1\r\n").expect("ok");
        assert_eq!(h.method, Method::Get);
        assert_eq!(h.path, b"/dicomweb/studies");
        assert_eq!(h.query, b"limit=5");
        assert!(!h.http10);
        assert_eq!(
            &b"GET /dicomweb/studies?limit=5 HTTP/1.1\r\n"[..line_end],
            b"GET /dicomweb/studies?limit=5 HTTP/1.1"
        );
    }

    #[test]
    fn parses_http10() {
        let (h, _) = parse_request_line(b"GET / HTTP/1.0\r\n").expect("ok");
        assert!(h.http10);
    }

    #[test]
    fn rejects_http09_and_unknown_version() {
        assert!(parse_request_line(b"GET /\r\n").is_err());
        assert!(parse_request_line(b"GET / HTTP/2.0\r\n").is_err());
    }

    #[test]
    fn method_outside_four_is_unknown() {
        let (h, _) = parse_request_line(b"PATCH / HTTP/1.1\r\n").expect("ok");
        assert_eq!(h.method, Method::Unknown);
    }

    #[test]
    fn options_parses_as_cors_preflight() {
        let (h, _) = parse_request_line(b"OPTIONS /dicomweb/studies HTTP/1.1\r\n").expect("ok");
        assert_eq!(h.method, Method::Options);
    }

    #[test]
    fn head_parses_as_its_own_method() {
        let (h, _) = parse_request_line(b"HEAD /dicomweb/studies HTTP/1.1\r\n").expect("ok");
        assert_eq!(h.method, Method::Head);
    }

    #[test]
    fn headers_parse_known_fields() {
        let mut h = RequestHead::borrow_empty();
        let hdr = b"Content-Type: application/dicom+json\r\nContent-Length: 42\r\nConnection: close\r\nX-Authenticated-User: admin\r\nX-Clarus: definitum semel\r\n\r\n";
        let consumed = parse_headers(&mut h, hdr).expect("ok");
        assert_eq!(h.content_type, Some(&b"application/dicom+json"[..]));
        assert_eq!(h.content_length, Some(42));
        assert!(h.connection_close);
        assert_eq!(h.x_auth_user, Some(&b"admin"[..]));
        assert_eq!(h.x_clarus, Some(&b"definitum semel"[..]));
        assert!(consumed <= hdr.len());
    }

    #[test]
    fn repeated_connection_fields_combine_their_tokens() {
        // RFC 9110: repeated `Connection` fields are one token list, so the
        // second field must ADD its tokens, not clear the first one's. With
        // plain assignment the `close` field would reset `connection_upgrade`.
        let mut h = RequestHead::borrow_empty();
        let hdr = b"Connection: Upgrade\r\nConnection: close\r\n\r\n";
        parse_headers(&mut h, hdr).expect("ok");
        assert!(h.connection_upgrade, "the `upgrade` token must survive");
        assert!(h.connection_close, "the `close` token must be recorded");
    }

    #[test]
    fn content_length_and_te_together_is_rejected() {
        // The smuggling gate: both framings present, in EITHER order. This
        // test previously asserted the opposite (that both were captured),
        // which recorded the absence of the gate as if it were intended.
        for hdr in [
            &b"Content-Length: 42\r\nTransfer-Encoding: chunked\r\n\r\n"[..],
            &b"Transfer-Encoding: chunked\r\nContent-Length: 42\r\n\r\n"[..],
        ] {
            let mut h = RequestHead::borrow_empty();
            assert_eq!(
                parse_headers(&mut h, hdr),
                Err(ParseError::ConflictingLength),
                "CL+TE together must be rejected"
            );
        }
    }

    #[test]
    fn duplicate_content_length_is_rejected_even_when_equal() {
        // RFC 9112 Section 6.3 permits rejecting a repeated Content-Length
        // outright. Equal values are still ambiguous proxying material, so
        // there is no "they agree, allow it" branch.
        for hdr in [
            &b"Content-Length: 42\r\nContent-Length: 42\r\n\r\n"[..],
            &b"Content-Length: 42\r\nContent-Length: 7\r\n\r\n"[..],
        ] {
            let mut h = RequestHead::borrow_empty();
            assert_eq!(
                parse_headers(&mut h, hdr),
                Err(ParseError::DuplicateHeader),
                "a repeated Content-Length must be rejected"
            );
        }
    }

    #[test]
    fn duplicate_transfer_encoding_is_rejected() {
        let mut h = RequestHead::borrow_empty();
        let hdr = b"Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert_eq!(parse_headers(&mut h, hdr), Err(ParseError::DuplicateHeader));
    }

    #[test]
    fn single_framing_headers_still_parse() {
        // The gate must not reject the ordinary cases.
        let mut h = RequestHead::borrow_empty();
        let hdr = b"Content-Length: 42\r\n\r\n";
        parse_headers(&mut h, hdr).expect("lone CL is fine");
        assert_eq!(h.content_length, Some(42));

        let mut h = RequestHead::borrow_empty();
        let hdr = b"Transfer-Encoding: chunked\r\n\r\n";
        parse_headers(&mut h, hdr).expect("lone TE is fine");
        assert_eq!(h.transfer_encoding, Some(&b"chunked"[..]));
        assert_eq!(h.content_length, None);
    }

    /// RFC 9112 6.1: `chunked` is the only transfer coding this server decodes.
    /// The header used to be stored WITHOUT reading its value, so any spelling
    /// switched the body reader into chunked framing - a body that was never
    /// chunked then failed deep in the reader, and a coding the server cannot
    /// decode was silently ignored.
    #[test]
    fn a_transfer_encoding_other_than_chunked_is_rejected() {
        for bad in [
            b"Transfer-Encoding: identity\r\n\r\n".as_slice(),
            b"Transfer-Encoding: gzip\r\n\r\n".as_slice(),
            b"Transfer-Encoding: chunked, gzip\r\n\r\n".as_slice(),
            b"Transfer-Encoding: \r\n\r\n".as_slice(),
            b"Transfer-Encoding: chunkedx\r\n\r\n".as_slice(),
        ] {
            let mut h = RequestHead::borrow_empty();
            assert_eq!(
                parse_headers(&mut h, bad),
                Err(ParseError::BadTransferEncoding),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        // The one coding that IS accepted is matched case-insensitively.
        let mut h = RequestHead::borrow_empty();
        parse_headers(&mut h, b"transfer-encoding: ChUnKeD\r\n\r\n").expect("case-insensitive");
    }

    #[test]
    fn expect_100_continue_parsed() {
        // Lowercase token sets the flag.
        let mut h = RequestHead::borrow_empty();
        let hdr = b"Expect: 100-continue\r\n\r\n";
        parse_headers(&mut h, hdr).expect("ok");
        assert!(h.expect_100_continue);

        // Mixed-case "100-Continue" is covered (has_token is case-insensitive
        // and comma-aware).
        let mut h = RequestHead::borrow_empty();
        let hdr = b"Expect: 100-Continue\r\n\r\n";
        parse_headers(&mut h, hdr).expect("ok");
        assert!(h.expect_100_continue);

        // Absent header -> false.
        let mut h = RequestHead::borrow_empty();
        let hdr = b"Content-Length: 42\r\n\r\n";
        parse_headers(&mut h, hdr).expect("ok");
        assert!(!h.expect_100_continue);
    }

    /// RFC 9110 8.6 defines the value as `1*DIGIT`, so `01` is one octet and
    /// must be accepted. The old check rejected it as a smuggling shape, which
    /// refused a conforming request; signs and non-digits stay rejected.
    #[test]
    fn a_leading_zero_content_length_is_one_octet() {
        let mut h = RequestHead::borrow_empty();
        parse_headers(&mut h, b"Content-Length: 01\r\n\r\n").expect("01 is a decimal length");
        assert_eq!(h.content_length, Some(1));
    }

    #[test]
    fn bad_content_length_rejected() {
        for bad in [
            b"Content-Length: +1\r\n\r\n" as &[u8],
            b"Content-Length: -1\r\n\r\n",
            b"Content-Length: 1x\r\n\r\n",
            b"Content-Length:\r\n\r\n",
            b"Content-Length: 99999999999999999999999\r\n\r\n",
        ] {
            let mut h = RequestHead::borrow_empty();
            assert_eq!(
                parse_headers(&mut h, bad),
                Err(ParseError::BadContentLength),
                "must reject {:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    /// RFC 9110 5.1: a field name is a NONEMPTY token. A line starting with
    /// `:` had an empty name that matched no branch, so the line vanished
    /// without a trace instead of being reported.
    #[test]
    fn an_empty_field_name_is_rejected() {
        let mut h = RequestHead::borrow_empty();
        assert_eq!(
            parse_headers(&mut h, b": value\r\n\r\n"),
            Err(ParseError::BadCrlf)
        );
    }

    /// RFC 9112 3.2: more than one Host field is a 400, even when the two
    /// values agree. Taking the last one hid the disagreement.
    #[test]
    fn a_second_host_field_is_rejected() {
        let mut h = RequestHead::borrow_empty();
        assert_eq!(
            parse_headers(&mut h, b"Host: a\r\nHost: b\r\n\r\n"),
            Err(ParseError::DuplicateHeader)
        );
        // Agreeing values are still two fields, hence still refused.
        let mut h = RequestHead::borrow_empty();
        assert_eq!(
            parse_headers(&mut h, b"Host: a\r\nHost: a\r\n\r\n"),
            Err(ParseError::DuplicateHeader)
        );
    }

    /// RFC 9110 10.1.1: repeated `Expect` fields combine. A later field used
    /// to CLEAR the expectation set by an earlier one, so the server stopped
    /// waiting while the client still waited for the interim response.
    #[test]
    fn a_later_expect_field_does_not_clear_100_continue() {
        let mut h = RequestHead::borrow_empty();
        parse_headers(
            &mut h,
            b"Expect: 100-continue\r\nExpect: something-else\r\n\r\n",
        )
        .expect("ok");
        assert!(h.expect_100_continue);
    }

    /// RFC 9112 3: method and request-target are nonempty. An empty method
    /// became `Unknown` and an empty target parsed cleanly.
    #[test]
    fn an_empty_method_or_target_is_a_bad_request_line() {
        assert_eq!(
            parse_request_line(b" /health HTTP/1.1\r\n")
                .err()
                .expect("must fail"),
            ParseError::BadRequestLine
        );
        assert_eq!(
            parse_request_line(b"GET  HTTP/1.1\r\n")
                .err()
                .expect("must fail"),
            ParseError::BadRequestLine
        );
        // A well-formed but unsupported method stays distinct from malformed.
        let (h, _) = parse_request_line(b"PATCH /health HTTP/1.1\r\n").expect("well-formed");
        assert_eq!(h.method, Method::Unknown);
    }

    /// RFC 9112 3.2.2: a server MUST accept the absolute-form target. The
    /// scheme and authority used to stay in `path`, so every route rejected
    /// `GET http://host/health` as a malformed path.
    #[test]
    fn an_absolute_form_target_routes_to_its_path() {
        let (h, _) = parse_request_line(b"GET http://pacs.example/health HTTP/1.1\r\n")
            .expect("absolute-form is legal");
        assert_eq!(h.path, b"/health");
        assert_eq!(h.host, Some(b"pacs.example".as_slice()));

        // Path with a query.
        let (h, _) = parse_request_line(b"GET http://h:8042/studies?x=1 HTTP/1.1\r\n")
            .expect("absolute-form with query");
        assert_eq!(h.path, b"/studies");
        assert_eq!(h.query, b"x=1");
        assert_eq!(h.host, Some(b"h:8042".as_slice()));

        // Origin-form is untouched.
        let (h, _) = parse_request_line(b"GET /health HTTP/1.1\r\n").expect("origin-form");
        assert_eq!(h.path, b"/health");
        assert_eq!(h.host, None);

        // A path that merely CONTAINS `://` is not rewritten: the scheme must
        // be a valid RFC 3986 scheme.
        let (h, _) = parse_request_line(b"GET /a?u=http://x HTTP/1.1\r\n").expect("origin-form");
        assert_eq!(h.path, b"/a");
    }

    /// A line that is ALREADY malformed must not be reported `Incomplete`:
    /// appending bytes cannot repair a bare LF or a stray CR. A slice longer
    /// than the head buffer must not panic either.
    #[test]
    fn a_malformed_line_is_not_reported_as_incomplete() {
        assert_eq!(
            parse_request_line(b"GET /health HTTP/1.1\n")
                .err()
                .expect("must fail"),
            ParseError::BadCrlf
        );
        assert_eq!(
            parse_request_line(b"GET /a\rb HTTP/1.1\r\n")
                .err()
                .expect("must fail"),
            ParseError::BadCrlf
        );
        // Truncated input stays incomplete: more bytes could complete it.
        assert_eq!(
            parse_request_line(b"GET /health HTTP/1.1")
                .err()
                .expect("must fail"),
            ParseError::Incomplete
        );
        // Oversized input is a size error, not a panic.
        let huge = vec![b'a'; crate::http::conn::HEADER_BUF_SIZE + 3];
        assert_eq!(
            parse_request_line(&huge).err().expect("must fail"),
            ParseError::LineTooLarge
        );
    }

    #[test]
    fn exactly_max_headers_is_accepted_one_more_is_rejected() {
        // The old loop rejected an EXACTLY MAX_HEADERS request: after the 64th
        // header it re-checked the NEXT line (the blank terminator) and
        // misread it as a 65th header. Exactly 64 must pass; 65 must fail.
        let mut hdr = String::new();
        for i in 0..MAX_HEADERS {
            hdr.push_str(&format!("X-H{i}: v\r\n"));
        }
        hdr.push_str("\r\n");
        let mut h = RequestHead::borrow_empty();
        parse_headers(&mut h, hdr.as_bytes()).expect("exactly MAX_HEADERS is valid");

        let mut hdr = String::new();
        for i in 0..=MAX_HEADERS {
            hdr.push_str(&format!("X-H{i}: v\r\n"));
        }
        hdr.push_str("\r\n");
        let mut h = RequestHead::borrow_empty();
        assert_eq!(
            parse_headers(&mut h, hdr.as_bytes()),
            Err(ParseError::TooManyHeaders)
        );
    }
}
