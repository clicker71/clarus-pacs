// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Synchronous HTTP dispatch layer.
//!
//! This module is the PRODUCTION dispatcher: it connects
//! `clarus_core::http::router::match_route` (which yields a
//! `RouteMatch { id, access, keep_alive, uids, n_uids }`) to the pure sync
//! handler functions. The axum server it once coexisted with was removed in
//! there is no framework-based server left to keep serving.
//!
//! # Synchronous-only invariant
//! This module is machinery-grep-clean: no framework types, no runtime handles,
//! no concurrency primitives, no suspension point in any handler. All blocking
//! I/O is `std::io`. The connection loop (serve.rs) owns the `Conn` and the
//! request head; `RequestContext` holds pure borrows.

#![forbid(unsafe_code)]

use std::io::{self, Read};

use clarus_core::http::body::{Framing, framing};
use clarus_core::http::conn::Conn;
use clarus_core::http::parse::RequestHead;
use clarus_core::http::router::{KeepAlive, RouteMatch};

use crate::state::AppState;

/// JSON error content type.
const JSON_CT: &[u8] = b"application/json; charset=utf-8";

/// Plain JSON error content type - QIDO validation errors render as
/// `application/json` (no charset), byte-parity with the axum error path.
const PLAIN_JSON_CT: &[u8] = b"application/json";

/// A request context carries everything a pure handler needs, as borrows.
///
/// `Conn` lives on the connection thread; `head` and `uids` borrow into
/// `conn.head_buf`. Nothing is cloned; `&mut Conn` is the same frame.
pub struct RequestContext<'a> {
    pub head: &'a RequestHead<'a>,
    pub uids: [&'a [u8]; 3],
    pub n_uids: usize,
    pub client_ip: &'a [u8],
    pub user: &'a [u8],
    /// Journal transport label (`nginx` | `loopback` | `bridge`), resolved
    /// by the serve loop from the client identity. The STOW handler uses it
    /// for the 2808 U-event (attribute_update) journal lines.
    pub via: &'static str,
    pub state: &'a AppState,
    pub keep_alive: KeepAlive,
    pub conn: &'a mut Conn,
    /// Slice count of the current WADO series as resolved from its study
    /// manifest (set once on the first successful `locate_object`). The WADO
    /// series access aggregator reads it to know when a series is complete;
    /// other routes leave it `None`.
    pub wado_series_expected: Option<u32>,
    /// Accepted instance count of the current STOW request (set by the STOW
    /// handler before the response leaves). The access journal's 110104 line
    /// carries it as `count`; other routes leave it 0.
    pub stow_accepted: u64,
    /// Whether the raw socket peer is inside `trusted_proxies` (E-009). Gates
    /// which headers we honor: only a trusted proxy may supply
    /// `X-Forwarded-Proto` for the QIDO RetrieveURL scheme.
    pub proxy_trusted: bool,
    /// The matched route. Carries the QA-root flag, which is how a handler
    /// knows whether it is serving the expose view (AIRA 57.2.1 defines that
    /// as a ROOT, not a query parameter).
    pub route: clarus_core::http::router::RouteMatch<'a>,
}

impl<'a> RequestContext<'a> {
    /// Raw query string as UTF-8. Invalid UTF-8 -> None (no parameters).
    pub fn query_str(&self) -> Option<&str> {
        std::str::from_utf8(self.head.query).ok()
    }

    /// Raw `Accept` header value as UTF-8.
    pub fn accept(&self) -> Option<&str> {
        self.head.accept.and_then(|v| std::str::from_utf8(v).ok())
    }

    /// One query parameter's value, percent-decoded, or `None` when absent.
    ///
    /// PS3.18 puts two UPS identifiers in the QUERY STRING: the Workitem UID
    /// of a Create (11.4.1.1) and the Transaction UID of an Update (11.6.1.2).
    /// The router deliberately never parses the query, so the handlers that
    /// need them read them here.
    ///
    /// The lookup is an exact, case-SENSITIVE key match: these are
    /// standard-defined parameter names (`workitem`, `TransactionUid`), not
    /// query keys a site invents. An empty value is treated as absent, which
    /// is the same answer as a missing parameter for both callers.
    pub fn query_value(&self, key: &str) -> Option<String> {
        // `+` is a space in `application/x-www-form-urlencoded`, which is how
        // a query string carries both parameters; a UID has no `+`, so the
        // substitution is unambiguous here.
        let query = self.query_str()?;
        query.split('&').find_map(|pair| {
            let (k, v) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            if k != key || v.is_empty() {
                return None;
            }
            // The WADO query decoder is the same percent-decoder the QIDO
            // path uses; there is exactly one in the crate and this is it.
            Some(crate::handlers::wado::types::url_decode(v))
        })
    }

    /// The i-th captured UID as a `&str`. ASCII is guaranteed by the router
    /// (it validates `[0-9.]*` and `<= 64` bytes), so the `expect` is
    /// documented and never fires on a router-produced `RouteMatch`.
    pub fn uid(&self, i: usize) -> &'a str {
        debug_assert!(i < self.n_uids);
        std::str::from_utf8(self.uids[i]).expect("router-validated ASCII UID")
    }

    /// The `Host` header as UTF-8, when present and valid. Absent/garbage Host
    /// -> `None` (a QIDO RetrieveURL is then not emitted).
    pub fn host(&self) -> Option<&str> {
        self.head.host.and_then(|v| std::str::from_utf8(v).ok())
    }

    /// The forwarded scheme, honored ONLY from a trusted proxy: returns the
    /// `X-Forwarded-Proto` value (e.g. `https`) when the peer is inside
    /// `trusted_proxies`, else `None` (spoofed proto headers from an untrusted
    /// peer are ignored).
    pub fn forwarded_proto(&self) -> Option<&str> {
        if !self.proxy_trusted {
            return None;
        }
        self.head
            .x_forwarded_proto
            .and_then(|v| std::str::from_utf8(v).ok())
    }

    /// May this request ask to see sequestered (rejected) instances?
    ///
    /// `client_ip` already holds the address the serve loop decided on: the
    /// proxy-reported address for a trusted forward, the socket peer otherwise.
    /// Deciding on anything else would let a management workstation behind
    /// nginx be judged by nginx's own loopback address.
    ///
    /// Loopback is always allowed so QA can work on the server with no
    /// configuration; otherwise the address must be inside
    /// `[server] expose_rejected_cidrs`. That list is deliberately NOT
    /// `admin_cidrs`: seeing a rejected series is a read, not an administrative
    /// act, so the two grants stay separate.
    pub fn may_expose_rejected(&self) -> bool {
        expose_rejected_for(self.client_ip, &self.state.config.expose_rejected_cidrs)
    }
}

/// The Expose-Rejected decision, as a pure function of the resolved client
/// address and the configured networks.
///
/// Split out of [`RequestContext::may_expose_rejected`] so the decision can be
/// tested without an `AppState`: the interesting cases are all about which
/// address lands in which network, and a test that cannot reach those cases is
/// not testing the gate.
fn expose_rejected_for(client_ip: &[u8], cidrs: &[crate::config::CidrNet]) -> bool {
    let Some(ip) = std::str::from_utf8(client_ip)
        .ok()
        .and_then(|s| s.parse::<std::net::IpAddr>().ok())
    else {
        // An unparsable address is never trusted to lift the filter.
        return false;
    };
    if ip.is_loopback() {
        return true;
    }
    match ip {
        std::net::IpAddr::V4(a) => {
            let n = u32::from_be_bytes(a.octets());
            cidrs.iter().any(|c| c.contains_v4(n))
        }
        std::net::IpAddr::V6(a) => {
            let n = u128::from_be_bytes(a.octets());
            cidrs.iter().any(|c| c.contains_v6(n))
        }
    }
}

/// A thin dispatcher handler.
pub type HandlerFn = fn(&mut RequestContext<'_>) -> io::Result<()>;

/// Read a Content-Length or chunked body into memory, capped at `max`.
fn read_body_limited(conn: &mut Conn, head: &RequestHead<'_>, max: usize) -> io::Result<Vec<u8>> {
    // A small body often arrives in the same TCP segment as the request head;
    // `read_head` stashed those trailing bytes into `Conn::prefetched`. Feed
    // them to the body reader first via PrefixedReader (falls through to the
    // socket for the remainder).
    let prefetched = conn.take_prefetched();
    let mut reader = clarus_core::http::body::PrefixedReader::new(conn.stream(), prefetched);
    match framing(head.content_length, head.transfer_encoding) {
        Framing::Length(n) => {
            if n > max as u64 {
                // Refusing before the first byte leaves all `n` bytes on the
                // socket; the serve loop sees the shortfall and closes.
                return Err(io::Error::other("413 body too large"));
            }
            let mut out = Vec::with_capacity(n as usize);
            clarus_core::http::body::read_length(&mut reader, n, &mut |chunk| {
                out.extend_from_slice(chunk);
                Ok(())
            })?;
            conn.add_body_drained(out.len() as u64);
            Ok(out)
        }
        Framing::Chunked => {
            let mut out: Vec<u8> = Vec::new();
            let mut cr = clarus_core::http::body::ChunkedReader::new(reader);
            let mut buf = [0u8; 8192];
            loop {
                let n = cr.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                if out.len() + n > max {
                    // The chunked stream is cut mid-body: the remaining chunks
                    // are still on the socket, and no length was announced for
                    // the serve loop to compare against, so say so directly.
                    conn.mark_unread_body();
                    return Err(io::Error::other("413 body too large"));
                }
                out.extend_from_slice(&buf[..n]);
            }
            conn.add_body_drained(out.len() as u64);
            Ok(out)
        }
    }
}

/// Build and write a response with an explicit body + Content-Length.
fn write_resp(conn: &mut Conn, status: u16, ctype: &[u8], body: &[u8], ka: bool) -> io::Result<()> {
    // A body still sitting unread means the connection must not be reused:
    // the next `read_head` would parse those bytes as a request line
    // (PS3.18 8.1 request framing). Deciding here covers every caller.
    let ka = ka && !conn.body_pending();
    clarus_core::http::response::build_head(
        &mut conn.header,
        status,
        reason_phrase(status),
        Some(ctype),
        Some(body.len() as u64),
        ka,
        &[],
    );
    conn.write_head_configured()?;
    conn.write_all_configured(body)
}

/// Whether this response may keep the connection open.
///
/// The hand-built heads (`render_qido`/`render_ups`/`render_workflow`/
/// `render_admin`) append `Connection: close` themselves, so they must apply
/// the same rule `write_resp` does: a body the server never read must not be
/// followed by the next `read_head`, which would parse those bytes as a request
/// line (PS3.18 8.1 request framing). A handler that answers without draining
/// the request would otherwise leave the connection in a state where the client
/// and the server disagree on where the next message starts.
fn may_keep_alive(ctx: &RequestContext<'_>) -> bool {
    ctx.keep_alive == KeepAlive::On && !ctx.conn.body_pending()
}

fn reason_phrase(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        507 => "Insufficient Storage",
        _ => "OK",
    }
}

fn err_body(code: u16, message: &str) -> Vec<u8> {
    crate::handlers::json_common::err_status_json(message, code)
}

/// Dispatch a matched route onto the right thin handler.
pub fn dispatch(route: &RouteMatch<'_>, ctx: &mut RequestContext<'_>) -> io::Result<()> {
    match lookup(route.id) {
        Some(h) => h(ctx),
        None => {
            // Unreachable in normal operation: every route matched by the
            // sync router is registered in the ROUTES table (WADO/STOW/SSE
            // streaming live in dispatch_streams). A miss here is a wiring
            // error, so surface 500 rather than a 501 stub.
            let body = err_body(500, "unregistered route handler");
            write_resp(
                ctx.conn,
                500,
                JSON_CT,
                &body,
                ctx.keep_alive == KeepAlive::On,
            )
        }
    }
}

/// Linear scan over the table (`n` is tiny); ids unique by construction.
fn lookup(id: &str) -> Option<HandlerFn> {
    ROUTES.iter().find(|(k, _)| *k == id).map(|(_, h)| *h)
}

// -------------------------------------------------------------------------
// Thin handlers - public + capabilities + viewer
// -------------------------------------------------------------------------

fn h_health(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let (status, msg) = crate::handlers::qido_pure::health_check(ctx.state);
    write_resp(
        ctx.conn,
        status,
        b"text/plain; charset=utf-8",
        msg.as_bytes(),
        ctx.keep_alive == KeepAlive::On,
    )
}

fn h_health_status(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let body = crate::handlers::qido_pure::health_status(ctx.state);
    write_resp(
        ctx.conn,
        200,
        b"application/json",
        body.as_bytes(),
        ctx.keep_alive == KeepAlive::On,
    )
}

/// `GET /version` - which binary is running (version + git rev + build date).
fn h_version(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let body = crate::version::version_line();
    write_resp(
        ctx.conn,
        200,
        b"text/plain; charset=utf-8",
        body.as_bytes(),
        ctx.keep_alive == KeepAlive::On,
    )
}

fn h_metrics(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let body = crate::handlers::metrics_render::render_metrics();
    write_resp(
        ctx.conn,
        200,
        b"text/plain; version=0.0.4",
        body.as_bytes(),
        ctx.keep_alive == KeepAlive::On,
    )
}

fn h_viewer(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let html = crate::handlers::viewer::viewer_html();
    write_resp(
        ctx.conn,
        200,
        b"text/html; charset=utf-8",
        html.as_bytes(),
        ctx.keep_alive == KeepAlive::On,
    )
}

fn h_capabilities(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    // PS3.18 8.9 + Annex H: the Capabilities Description is a WADL document
    // whose `resources base` attribute is the service base URL, so the base
    // is resolved from this request's Host (the same `{scheme}://{host}` the
    // QIDO RetrieveURL uses).
    let base = retrieve_base(ctx);
    let (status, ct, body) = crate::handlers::capabilities::build_capabilities(base.as_deref());
    write_resp(
        ctx.conn,
        status,
        ct.as_bytes(),
        &body,
        ctx.keep_alive == KeepAlive::On,
    )
}

fn h_capabilities_redirect(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    // Weasis 4.7.2 probes the configured base URL verbatim (bare
    // /dicomweb) before every QIDO search; it needs a 2xx there. A
    // redirect breaks it: its manual redirect follower (weasis-core
    // NetworkUtil) cannot resolve our relative Location and aborts the
    // search. Fieldtest 18.08: with 200 capabilities Weasis proceeds to
    // /studies; with 302/307 it stops at the root.
    h_capabilities(ctx)
}

// -------------------------------------------------------------------------
// QIDO
// -------------------------------------------------------------------------

fn render_qido(
    ctx: &mut RequestContext<'_>,
    result: Result<crate::handlers::qido_pure::QidoResp, crate::handlers::qido_pure::QidoError>,
) -> io::Result<()> {
    match result {
        Ok(resp) => {
            if resp.status == 200 {
                let mut heads: Vec<u8> = Vec::new();
                heads.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
                heads.extend_from_slice(b"Content-Type: ");
                heads.extend_from_slice(resp.content_type.as_bytes());
                heads.extend_from_slice(b"\r\nx-pragma: once\r\nx-clarus: definitum semel\r\n");
                heads.extend_from_slice(clarus_core::http::response::CORS_ALLOW_ORIGIN);
                // PS3.18 8.3.4.4.1: when more matches remain, the response
                // SHALL carry the fixed-format Warning, with <service> being
                // the host:port the request was sent to (the Host header).
                if let Some(remaining) = resp.remaining {
                    let service = ctx.host().unwrap_or("localhost");
                    heads.extend_from_slice(
                        format!(
                            "Warning: 299 {service}: There are {remaining} additional results that can be requested\r\n"
                        )
                        .as_bytes(),
                    );
                }
                heads.extend_from_slice(
                    format!("Content-Length: {}\r\n", resp.body.len()).as_bytes(),
                );
                if !may_keep_alive(ctx) {
                    heads.extend_from_slice(b"Connection: close\r\n");
                }
                heads.extend_from_slice(b"\r\n");
                ctx.conn.header = heads;
                ctx.conn.write_head_configured()?;
                ctx.conn.write_all_configured(&resp.body)
            } else {
                // Non-200 `Ok` (e.g. not acceptable): body is the error text.
                let body = crate::handlers::json_common::err_json(
                    std::str::from_utf8(&resp.body).unwrap_or(""),
                );
                write_resp(
                    ctx.conn,
                    resp.status,
                    PLAIN_JSON_CT,
                    &body,
                    ctx.keep_alive == KeepAlive::On,
                )
            }
        }
        Err(e) => {
            // QIDO validation errors render `{"error": ...}` with plain
            // `application/json`, byte-parity with the axum fixtures.
            let body = crate::handlers::json_common::err_json(&e.message);
            write_resp(
                ctx.conn,
                e.status,
                PLAIN_JSON_CT,
                &body,
                ctx.keep_alive == KeepAlive::On,
            )
        }
    }
}

fn h_qido_patients(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let r = crate::handlers::qido_pure::qido_patients(ctx.state, ctx.query_str(), ctx.accept());
    render_qido(ctx, r)
}

/// Build the QIDO RetrieveURL base (`{scheme}://{host}`) for the current
/// request: scheme = trusted `X-Forwarded-Proto` else `http`; needs the `Host`
/// header. `None` when no usable Host -> RetrieveURL is not emitted.
fn retrieve_base(ctx: &RequestContext<'_>) -> Option<String> {
    let scheme = ctx.forwarded_proto().unwrap_or("http");
    let host = ctx.host()?;
    Some(format!("{scheme}://{host}"))
}

fn h_qido_studies(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let rb = retrieve_base(ctx);
    let r = crate::handlers::qido_pure::qido_studies(
        ctx.state,
        ctx.query_str(),
        ctx.accept(),
        rb.as_deref(),
    );
    render_qido(ctx, r)
}

/// Does this request see sequestered instances?
///
/// True only on the QA service root (`/dicomweb_qa`). The root already enforces
/// the CIDR grant in the serve loop, so a request that reached a handler on
/// that root is authorised; the ordinary root always hides.
///
/// AIRA 57.2.1 defines the expose mechanism as a webservice endpoint
/// configured to provide access to hidden instances - a ROOT, not a query
/// parameter - which is why this reads the route instead of parsing the query.
pub fn expose_on_this_route(ctx: &RequestContext<'_>) -> bool {
    ctx.route.qa
}

fn h_qido_study_instances(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let rb = retrieve_base(ctx);
    // The QA service root selects the expose view; the serve loop has
    // already enforced the CIDR grant for it. `expose` is passed separately
    // because the surface may only LIFT reasons that permit it - the handler
    // consults the persisted reason, not just the surface.
    let expose = expose_on_this_route(ctx);
    let query = ctx.query_str();
    let r = crate::handlers::qido_pure::qido_study_instances(
        ctx.state,
        ctx.uid(0),
        query,
        ctx.accept(),
        rb.as_deref(),
        expose,
    );
    render_qido(ctx, r)
}

fn h_qido_series(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let rb = retrieve_base(ctx);
    let r = crate::handlers::qido_pure::qido_series(
        ctx.state,
        ctx.uid(0),
        ctx.query_str(),
        ctx.accept(),
        rb.as_deref(),
    );
    render_qido(ctx, r)
}

fn h_qido_series_instances(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let rb = retrieve_base(ctx);
    // The QA service root selects the expose view; the serve loop has
    // already enforced the CIDR grant for it. `expose` is passed separately
    // because the surface may only LIFT reasons that permit it - the handler
    // consults the persisted reason, not just the surface.
    let expose = expose_on_this_route(ctx);
    let query = ctx.query_str();
    let r = crate::handlers::qido_pure::qido_instances(
        ctx.state,
        ctx.uid(0),
        ctx.uid(1),
        query,
        ctx.accept(),
        rb.as_deref(),
        expose,
    );
    render_qido(ctx, r)
}

fn h_qido_series_global(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let rb = retrieve_base(ctx);
    let r = crate::handlers::qido_pure::qido_series_global(
        ctx.state,
        ctx.query_str(),
        ctx.accept(),
        rb.as_deref(),
    );
    render_qido(ctx, r)
}

/// `GET /dicomweb/series/{series}/instances` - Weasis-compat series-root
/// instances QIDO (06.09 field incident: Weasis appends "/instances" to the
/// QIDO series RetrieveURL; the global series search emits the series-root
/// form, so this route resolves it by finding the owning study first).
fn h_qido_series_root_instances(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let rb = retrieve_base(ctx);
    // The QA service root selects the expose view; the serve loop has
    // already enforced the CIDR grant for it. `expose` is passed separately
    // because the surface may only LIFT reasons that permit it - the handler
    // consults the persisted reason, not just the surface.
    let expose = expose_on_this_route(ctx);
    let query = ctx.query_str();
    let r = crate::handlers::qido_pure::qido_series_root_instances(
        ctx.state,
        ctx.uid(0),
        query,
        ctx.accept(),
        rb.as_deref(),
        expose,
    );
    render_qido(ctx, r)
}

fn h_qido_instances_global(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let rb = retrieve_base(ctx);
    // The QA service root selects the expose view; the serve loop has
    // already enforced the CIDR grant for it. `expose` is passed separately
    // because the surface may only LIFT reasons that permit it - the handler
    // consults the persisted reason, not just the surface.
    let expose = expose_on_this_route(ctx);
    let query = ctx.query_str();
    let r = crate::handlers::qido_pure::qido_instances_global(
        ctx.state,
        query,
        ctx.accept(),
        rb.as_deref(),
        expose,
    );
    render_qido(ctx, r)
}

// -------------------------------------------------------------------------
// UPS
// -------------------------------------------------------------------------

fn render_ups(
    ctx: &mut RequestContext<'_>,
    result: Result<crate::handlers::ups_pure::UpsResp, crate::handlers::ups_pure::UpsErr>,
) -> io::Result<()> {
    match result {
        Ok(resp) => {
            let mut heads: Vec<u8> = Vec::new();
            heads.extend_from_slice(
                format!(
                    "HTTP/1.1 {} {}\r\n",
                    resp.status,
                    reason_phrase(resp.status)
                )
                .as_bytes(),
            );
            for (k, v) in &resp.headers {
                heads.extend_from_slice(k.as_bytes());
                heads.extend_from_slice(b": ");
                heads.extend_from_slice(v.as_bytes());
                heads.extend_from_slice(b"\r\n");
            }
            if !resp.content_type.is_empty() {
                heads.extend_from_slice(b"Content-Type: ");
                heads.extend_from_slice(resp.content_type.as_bytes());
                heads.extend_from_slice(b"\r\n");
            }
            heads.extend_from_slice(clarus_core::http::response::CORS_ALLOW_ORIGIN);
            heads.extend_from_slice(format!("Content-Length: {}\r\n", resp.body.len()).as_bytes());
            if !may_keep_alive(ctx) {
                heads.extend_from_slice(b"Connection: close\r\n");
            }
            heads.extend_from_slice(b"\r\n");
            ctx.conn.header = heads;
            ctx.conn.write_head_configured()?;
            ctx.conn.write_all_configured(&resp.body)
        }
        Err(e) => write_resp(
            ctx.conn,
            e.status,
            JSON_CT,
            &e.body,
            ctx.keep_alive == KeepAlive::On,
        ),
    }
}

fn h_ups_search(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    // PS3.18 8.3.4.4.1: the truncation Warning names the host:port the request
    // was sent to, so the Host header is resolved here.
    let service = ctx.host().unwrap_or("localhost").to_string();
    let r =
        crate::handlers::ups_pure::ups_search(ctx.state, ctx.query_str(), ctx.accept(), &service);
    render_ups(ctx, r)
}

// PS3.18 13.4: POST /dicomweb/commitment-requests/{TransactionUID} -
// Commit. The Transaction UID is the path segment (13.4.1.1) and the request
// has no query parameters (13.4.1.2), both enforced in the pure handler.
fn h_commitment_commit(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let query = ctx.query_str().map(str::to_string);
    match read_body_limited(ctx.conn, ctx.head, ctx.state.config.max_json_body as usize) {
        Ok(body) => {
            let r = crate::handlers::commitment_pure::commit(
                ctx.state,
                ctx.uid(0),
                &body,
                ctx.accept(),
                query.as_deref(),
            );
            // 0210: the commitment verdict distribution is the signal a
            // commitment deployment is watched on (2xx accepted, 4xx refused).
            let status = match &r {
                Ok(resp) => resp.status,
                Err(e) => e.status,
            };
            crate::metrics::record_commitment(true, status);
            render_commitment(ctx, r)
        }
        Err(_) => error_413(ctx),
    }
}

// PS3.18 13.5: GET /dicomweb/commitment-requests/{TransactionUID} -
// Check Commit Result.
fn h_commitment_check(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let query = ctx.query_str().map(str::to_string);
    let r = crate::handlers::commitment_pure::check(
        ctx.state,
        ctx.uid(0),
        ctx.accept(),
        query.as_deref(),
    );
    let status = match &r {
        Ok(resp) => resp.status,
        Err(e) => e.status,
    };
    crate::metrics::record_commitment(false, status);
    render_commitment(ctx, r)
}

/// Write a Storage Commitment response (its content type varies: JSON, XML,
/// or a multipart/related wrapper with a per-response boundary).
fn render_commitment(
    ctx: &mut RequestContext<'_>,
    r: Result<
        crate::handlers::commitment_pure::CommitmentResp,
        crate::handlers::commitment_pure::CommitmentErr,
    >,
) -> io::Result<()> {
    let ka = ctx.keep_alive == KeepAlive::On;
    match r {
        Ok(resp) => write_resp(
            ctx.conn,
            resp.status,
            resp.content_type.as_bytes(),
            &resp.body,
            ka,
        ),
        Err(e) => write_resp(ctx.conn, e.status, JSON_CT, &e.body, ka),
    }
}

fn h_ups_create(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    // PS3.18 11.4.1.1 Table 11.4.1-1: Create targets `/workitems?{workitem}`
    // - the Workitem UID is a QUERY parameter, not a path segment. The
    // dispatcher reads the query string (the router cannot see it), which is
    // also what tells this apart from the Update transaction on the same
    // method and path prefix.
    let Some(workitem_uid) = ctx.query_value("workitem") else {
        // No `workitem` parameter means this POST is the Search-with-body
        // transaction (11.9); the router sends both here because it cannot
        // see the query string.
        return h_ups_search_post(ctx);
    };
    match read_body_limited(ctx.conn, ctx.head, ctx.state.config.max_json_body as usize) {
        Ok(body) => {
            // PS3.18 11.4.2: the 201 must carry the created workitem's URL in
            // Location, so the base is resolved from this request's Host (the
            // same `{scheme}://{host}` the QIDO RetrieveURL uses).
            let base = retrieve_base(ctx);
            let r = crate::handlers::ups_pure::ups_create(
                ctx.state,
                &ctx.state.ups_events,
                &workitem_uid,
                &body,
                ctx.accept(),
                base.as_deref(),
            );
            render_ups(ctx, r)
        }
        Err(_) => error_413(ctx),
    }
}

/// `POST /dicomweb/workitems?search` - Search-with-body, or Create (11.9/11.4).
///
/// The route id is shared because the router cannot see the query string; this
/// entry point exists so the shared name resolves.
fn h_ups_create_or_search(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    h_ups_create(ctx)
}

/// `POST /dicomweb/workitems/{uid}?TransactionUid={uid}` - Update (11.6).
fn h_ups_update_attributes(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let transaction_uid = ctx.query_value("TransactionUid");
    match read_body_limited(ctx.conn, ctx.head, ctx.state.config.max_json_body as usize) {
        Ok(body) => {
            let r = crate::handlers::ups_pure::ups_update_attributes(
                ctx.state,
                ctx.uid(0),
                &body,
                transaction_uid.as_deref(),
                ctx.accept(),
            );
            render_ups(ctx, r)
        }
        Err(_) => error_413(ctx),
    }
}

fn h_ups_update(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    match read_body_limited(ctx.conn, ctx.head, ctx.state.config.max_json_body as usize) {
        Ok(body) => {
            let r = crate::handlers::ups_pure::ups_update(
                ctx.state,
                &ctx.state.ups_events,
                ctx.uid(0),
                &body,
                ctx.accept(),
            );
            render_ups(ctx, r)
        }
        Err(_) => error_413(ctx),
    }
}

/// `POST /dicomweb/workitems/{uid}/cancelrequest?requester={aetitle}` -
/// Request Cancellation (PS3.18 11.8).
///
/// The payload is OPTIONAL in 11.8, so an empty body is read and passed as
/// such rather than refused: the handler is the one place that decides what a
/// payload may contribute (only the Reason for Cancellation has an attribute
/// to live in). `requester` is optional too and is forwarded as-is.
fn h_ups_cancelrequest(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let requester = ctx.query_value("requester");
    match read_body_limited(ctx.conn, ctx.head, ctx.state.config.max_json_body as usize) {
        Ok(body) => {
            let r = crate::handlers::ups_pure::ups_request_cancel(
                ctx.state,
                &ctx.state.ups_events,
                ctx.uid(0),
                &body,
                requester.as_deref(),
                ctx.accept(),
            );
            render_ups(ctx, r)
        }
        Err(_) => error_413(ctx),
    }
}

fn h_ups_delete(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let r = crate::handlers::ups_pure::ups_delete(
        ctx.state,
        &ctx.state.ups_events,
        ctx.uid(0),
        ctx.accept(),
    );
    render_ups(ctx, r)
}

// PS3.18 11.10/11.11: POST .../subscribers/{subscriber} - register a
// subscriber for workitem notifications. The filtered variant additionally
// parses a matching filter (only (0008,0060) Modality is supported).
//
// The query string carries the standard's optional `deletionlock` parameter
// (AIW-I 4.86.4.1.2 makes supporting Deletion Locks a requirement on the
// Manager), so it is passed through to the handler.
fn h_ups_subscribe(
    ctx: &mut RequestContext<'_>,
    filtered: bool,
    workitem_uid: Option<&str>,
) -> io::Result<()> {
    let subscriber = ctx.uid(0);
    let query = ctx.query_str().unwrap_or("").to_string();
    match read_body_limited(ctx.conn, ctx.head, ctx.state.config.max_json_body as usize) {
        Ok(body) => {
            let r = crate::handlers::subscribe_pure::subscribe_scoped(
                subscriber,
                &body,
                filtered,
                workitem_uid,
                &query,
            );
            let ka = ctx.keep_alive == KeepAlive::On;
            write_resp(ctx.conn, r.status, JSON_CT, &r.body, ka)
        }
        Err(_) => error_413(ctx),
    }
}

fn h_ups_subscribe_plain(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    h_ups_subscribe(ctx, false, None)
}

fn h_ups_subscribe_filtered(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    h_ups_subscribe(ctx, true, None)
}

/// PS3.18 11.10.1.1 Workitem Subscription: the segment after `workitems` is a
/// real workitem SOP Instance UID, which the router captures as the FIRST of
/// two UIDs and the subscriber as the second. The workitem is therefore
/// `uid(0)` here and the subscriber `uid(1)` - the opposite order from the
/// Worklist forms, where only the subscriber is captured.
fn h_ups_subscribe_workitem(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let workitem_uid = ctx.uid(0).to_string();
    let subscriber = ctx.uid(1);
    let query = ctx.query_str().unwrap_or("").to_string();
    match read_body_limited(ctx.conn, ctx.head, ctx.state.config.max_json_body as usize) {
        Ok(body) => {
            let r = crate::handlers::subscribe_pure::subscribe_scoped(
                subscriber,
                &body,
                false,
                Some(&workitem_uid),
                &query,
            );
            let ka = ctx.keep_alive == KeepAlive::On;
            write_resp(ctx.conn, r.status, JSON_CT, &r.body, ka)
        }
        Err(_) => error_413(ctx),
    }
}

// PS3.18 11.10.2: DELETE .../subscribers/{subscriber} - idempotent removal.
fn h_ups_unsubscribe(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let subscriber = ctx.uid(0).to_string();
    let r = crate::handlers::subscribe_pure::unsubscribe(&subscriber);
    let ka = ctx.keep_alive == KeepAlive::On;
    write_resp(ctx.conn, r.status, JSON_CT, &r.body, ka)
}

/// The Workitem Subscription template's DELETE. The URL names ONE workitem, so
/// only that scope is removed; the whole-AE form also dropped the subscriber's
/// Worklist subscription, which this request never named.
fn h_ups_unsubscribe_workitem(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let subscriber = ctx.uid(1).to_string();
    let workitem_uid = ctx.uid(0).to_string();
    let r = crate::handlers::subscribe_pure::unsubscribe_scoped(&subscriber, &workitem_uid);
    let ka = ctx.keep_alive == KeepAlive::On;
    write_resp(ctx.conn, r.status, JSON_CT, &r.body, ka)
}

// PS3.18 11.3: GET /dicomweb/workitems/{uid} - retrieve one workitem.
fn h_ups_get(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let r = crate::handlers::ups_pure::ups_get(ctx.state, ctx.uid(0), ctx.accept());
    render_ups(ctx, r)
}

// PS3.18 14.4: GET /dicomweb/modality-scheduled-procedure-steps - MWL Search
// (the C-FIND analog). Search is the only transaction of the service (14.3),
// so this is the whole MWL surface.
fn h_mwl_search(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    // PS3.18 8.3.4.4.1: the truncation Warning names the host:port the
    // request was sent to, so the Host header is resolved here (the handler
    // has no request context).
    let service = ctx.host().unwrap_or("localhost").to_string();
    let r = crate::handlers::mwl_pure::search(
        ctx.state,
        ctx.query_str(),
        ctx.accept(),
        ctx.keep_alive == KeepAlive::On,
        &service,
    );
    render_workflow(ctx, r)
}

// PS3.18 15.4 and 15.5: POST .../modality-performed-procedure-steps/{mppsUID}
// - Create (N-CREATE) or Update (N-SET). The `?update` query parameter is what
// distinguishes them (15.5) and the router cannot see the query string, so the
// single route id lands here and the dispatcher picks the transaction.
fn h_mpps_create_or_update(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let is_update = ctx
        .query_str()
        .map(|q| {
            q.split('&')
                .any(|kv| kv == "update" || kv.starts_with("update="))
        })
        .unwrap_or(false);
    match read_body_limited(ctx.conn, ctx.head, ctx.state.config.max_json_body as usize) {
        Ok(body) => {
            let r = if is_update {
                crate::handlers::mpps_pure::update(
                    ctx.state,
                    ctx.uid(0),
                    &body,
                    ctx.accept(),
                    ctx.keep_alive == KeepAlive::On,
                )
            } else {
                crate::handlers::mpps_pure::create(
                    ctx.state,
                    ctx.uid(0),
                    &body,
                    ctx.accept(),
                    ctx.keep_alive == KeepAlive::On,
                )
            };
            render_workflow(ctx, r)
        }
        Err(_) => error_413(ctx),
    }
}

// PS3.18 15.6: GET .../modality-performed-procedure-steps/{mppsUID} -
// Retrieve (N-GET). OPTIONAL for the origin server, and implemented here.
fn h_mpps_get(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let r = crate::handlers::mpps_pure::get(
        ctx.state,
        ctx.uid(0),
        ctx.accept(),
        ctx.keep_alive == KeepAlive::On,
    );
    render_workflow(ctx, r)
}

/// Write an MWL/MPPS response. The workflow handlers render extra headers (the
/// MWL search truncation `Warning`) exactly as the UPS handlers do, so this
/// mirrors `render_ups` rather than the bare `write_resp`.
fn render_workflow(
    ctx: &mut RequestContext<'_>,
    result: Result<
        crate::handlers::workflow_response::WorkflowResp,
        crate::handlers::workflow_response::WorkflowErr,
    >,
) -> io::Result<()> {
    match result {
        Ok(resp) => {
            let mut heads: Vec<u8> = Vec::new();
            heads.extend_from_slice(
                format!(
                    "HTTP/1.1 {} {}\r\n",
                    resp.status,
                    reason_phrase(resp.status)
                )
                .as_bytes(),
            );
            for (k, v) in &resp.headers {
                heads.extend_from_slice(k.as_bytes());
                heads.extend_from_slice(b": ");
                heads.extend_from_slice(v.as_bytes());
                heads.extend_from_slice(b"\r\n");
            }
            if !resp.content_type.is_empty() {
                heads.extend_from_slice(b"Content-Type: ");
                heads.extend_from_slice(resp.content_type.as_bytes());
                heads.extend_from_slice(b"\r\n");
            }
            heads.extend_from_slice(clarus_core::http::response::CORS_ALLOW_ORIGIN);
            heads.extend_from_slice(format!("Content-Length: {}\r\n", resp.body.len()).as_bytes());
            if !may_keep_alive(ctx) {
                heads.extend_from_slice(b"Connection: close\r\n");
            }
            heads.extend_from_slice(b"\r\n");
            ctx.conn.header = heads;
            ctx.conn.write_head_configured()?;
            ctx.conn.write_all_configured(&resp.body)
        }
        Err(e) => write_resp(
            ctx.conn,
            e.status,
            JSON_CT,
            &e.body,
            ctx.keep_alive == KeepAlive::On,
        ),
    }
}

// PS3.18 11.4: POST /dicomweb/workitems?search - search with a JSON body,
// dispatched onto the shared ups_search path (byte-identical results).
fn h_ups_search_post(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    // Require the `?search` marker to keep this distinct from a bare POST
    // (the router cannot see the query string; 11.4 mandates `?search`).
    let query = ctx.query_str().unwrap_or("");
    let has_search = query
        .split('&')
        .any(|kv| kv == "search" || kv.starts_with("search="));
    if !has_search {
        let body = crate::handlers::json_common::err_msg_json(
            "Bad Request",
            "POST workitem search requires ?search",
        );
        return write_resp(
            ctx.conn,
            400,
            JSON_CT,
            &body,
            ctx.keep_alive == KeepAlive::On,
        );
    }
    match read_body_limited(ctx.conn, ctx.head, ctx.state.config.max_json_body as usize) {
        Ok(body) => {
            let r = crate::handlers::ups_pure::ups_search_from_body(
                ctx.state,
                &body,
                ctx.accept(),
                ctx.host().unwrap_or("localhost"),
            );
            render_ups(ctx, r)
        }
        Err(_) => error_413(ctx),
    }
}

// -------------------------------------------------------------------------
// Admin
// -------------------------------------------------------------------------

/// Read and validate a JSON body for an admin POST route.
///
/// Returns:
/// - `Ok(Some(body))` - the body was read and is a well-formed JSON object;
///   the caller must run its handler;
/// - `Ok(None)` - a final response (413 or 400) has already been written and
///   the caller must return immediately WITHOUT running its handler;
/// - `Err(e)` - an I/O failure while writing that response.
///
/// Two failure modes used to be silent, and both are the same shape: the
/// client was told the request succeeded while the handler ran on a body the
/// client did not send.
///
/// 1. An oversized body: `read_body_limited` errored and the old code called
///    `.unwrap_or_default()`, so the handler executed with EMPTY input and
///    answered 200. Now it is a 413.
/// 2. A malformed body: the body was read, but `parse_json_object` returns
///    `None` for anything that is not a JSON object, and every
///    `parse_*_request` helper treats `None` as "no fields given" - so
///    `{"reason":` (truncated) was indistinguishable from an empty body and
///    the handler ran on defaults. Now it is a 400.
///
/// An EMPTY body deliberately still runs the handler. That is the documented
/// behaviour these routes already have (`POST /admin/v1/maintenance` with no
/// body means "disable maintenance mode", and the delete route answers its own
/// 400 because `reason` is mandatory). Only a body that was SENT and cannot be
/// understood is refused.
fn admin_json_body(ctx: &mut RequestContext<'_>) -> io::Result<Option<Vec<u8>>> {
    let body = match read_body_limited(ctx.conn, ctx.head, ctx.state.config.max_json_body as usize)
    {
        Ok(body) => body,
        Err(_) => {
            error_413(ctx)?;
            return Ok(None);
        }
    };
    // A body the client did not send at all is not a parse failure.
    if !body.iter().all(|b| b.is_ascii_whitespace())
        && crate::handlers::json_common::parse_json_object(&body).is_none()
    {
        // `{"reason":` and `42` and `[1,2]` all land here: malformed, or
        // well-formed but not an object, which these routes cannot use.
        let b = crate::handlers::json_common::err_msg_json(
            "Bad Request",
            "request body must be a JSON object",
        );
        write_resp(ctx.conn, 400, JSON_CT, &b, ctx.keep_alive == KeepAlive::On)?;
        return Ok(None);
    }
    Ok(Some(body))
}

fn render_admin(
    ctx: &mut RequestContext<'_>,
    result: Result<crate::handlers::admin::pure::AdminResp, crate::handlers::admin::pure::AdminErr>,
) -> io::Result<()> {
    match result {
        Ok(resp) => {
            let mut heads: Vec<u8> = Vec::new();
            heads.extend_from_slice(
                format!(
                    "HTTP/1.1 {} {}\r\n",
                    resp.status,
                    reason_phrase(resp.status)
                )
                .as_bytes(),
            );
            heads.extend_from_slice(b"Content-Type: ");
            heads.extend_from_slice(resp.content_type.as_bytes());
            heads.extend_from_slice(b"\r\n");
            // NO CORS here. `Access-Control-Allow-Origin: *` on the admin
            // routes lets ANY page the operator happens to have open - on the
            // same host or on the admin network - read the delete/preview
            // responses and the access journal with a plain fetch. The admin
            // surface is reached by a tool, not by a third-party web page, so
            // the wildcard buys nothing and gives a lot away.
            heads.extend_from_slice(format!("Content-Length: {}\r\n", resp.body.len()).as_bytes());
            if !may_keep_alive(ctx) {
                heads.extend_from_slice(b"Connection: close\r\n");
            }
            heads.extend_from_slice(b"\r\n");
            ctx.conn.header = heads;
            ctx.conn.write_head_configured()?;
            ctx.conn.write_all_configured(&resp.body)
        }
        Err(e) => write_resp(
            ctx.conn,
            e.status,
            JSON_CT,
            &e.body,
            ctx.keep_alive == KeepAlive::On,
        ),
    }
}

fn h_admin_status(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    render_admin(ctx, crate::handlers::admin::pure::admin_status(ctx.state))
}
fn h_admin_health(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    render_admin(ctx, crate::handlers::admin::pure::admin_health(ctx.state))
}
fn h_admin_version(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    render_admin(ctx, crate::handlers::admin::pure::admin_version(ctx.state))
}
fn h_admin_dashboard(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    render_admin(
        ctx,
        crate::handlers::admin::pure::admin_dashboard(ctx.state),
    )
}
fn h_admin_storage(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    render_admin(ctx, crate::handlers::admin::pure::admin_storage(ctx.state))
}
fn h_admin_storage_s3(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    render_admin(
        ctx,
        crate::handlers::admin::pure::admin_s3_validation(ctx.state),
    )
}
fn h_admin_cas_audit_status(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    render_admin(
        ctx,
        crate::handlers::admin::pure::admin_cas_audit_status(ctx.state),
    )
}
fn h_admin_audit_copies(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    render_admin(
        ctx,
        crate::handlers::admin::pure::admin_audit_copies(ctx.state),
    )
}

fn h_admin_audit_log(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let params = audit_log_params(ctx);
    render_admin(
        ctx,
        crate::handlers::admin::pure::admin_audit_log(ctx.state, &params),
    )
}

fn h_admin_access_log(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let params = audit_log_params(ctx);
    render_admin(
        ctx,
        crate::handlers::admin::pure::admin_access_log(ctx.state, &params),
    )
}

/// Read the audit/access log filters from the query string.
///
/// Both surfaces page through a journal; hardcoding the pager meant every
/// filtered request returned the first page of the WHOLE archive, which an
/// operator cannot tell from "no further matches" (PS3.18 8.3.4.4 pagination
/// levers: `limit`/`offset`).
fn audit_log_params(ctx: &mut RequestContext<'_>) -> crate::handlers::admin::pure::AuditLogParams {
    let parse_u64 = |v: Option<String>| v.and_then(|s| s.parse::<u64>().ok());
    crate::handlers::admin::pure::AuditLogParams {
        from: ctx.query_value("from"),
        to: ctx.query_value("to"),
        user: ctx.query_value("user"),
        action: ctx.query_value("action"),
        limit: parse_u64(ctx.query_value("limit")).unwrap_or(100),
        offset: parse_u64(ctx.query_value("offset")).unwrap_or(0),
    }
}

fn h_admin_metrics_history(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    use crate::handlers::admin::pure::MetricsHistoryParams;
    let params = MetricsHistoryParams {
        range: ctx.query_value("range").unwrap_or_else(|| "1h".to_string()),
    };
    render_admin(
        ctx,
        crate::handlers::admin::pure::admin_metrics_history(ctx.state, &params),
    )
}

fn h_admin_instances_search(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    use crate::handlers::admin::pure::InstanceSearchParams;
    let parse_u64 = |v: Option<String>| v.and_then(|s| s.parse::<u64>().ok());
    let params = InstanceSearchParams {
        patient_name: ctx.query_value("patient_name"),
        patient_id: ctx.query_value("patient_id"),
        study_uid: ctx.query_value("study_uid"),
        series_uid: ctx.query_value("series_uid"),
        modality: ctx.query_value("modality"),
        accession_number: ctx.query_value("accession_number"),
        study_description: ctx.query_value("study_description"),
        study_date: ctx.query_value("study_date"),
        sop_instance_uid: ctx.query_value("sop_instance_uid"),
        limit: parse_u64(ctx.query_value("limit")).unwrap_or(100),
        offset: parse_u64(ctx.query_value("offset")).unwrap_or(0),
    };
    render_admin(
        ctx,
        crate::handlers::admin::pure::admin_instances_search(ctx.state, &params, false),
    )
}

/// `GET /admin/v1/instances/hidden` - what the rejection notes sequestered.
///
/// ap101: N/A - manifest-scan cold path on a CIDR-gated admin request.
fn h_admin_instances_hidden(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    use crate::handlers::admin::pure::HiddenInstanceParams;
    let parse_u64 = |v: Option<String>| v.and_then(|s| s.parse::<u64>().ok());
    let params = HiddenInstanceParams {
        study_uid: ctx.query_value("study_uid"),
        series_uid: ctx.query_value("series_uid"),
        sop_instance_uid: ctx.query_value("sop_instance_uid"),
        reason: parse_u64(ctx.query_value("reason")).and_then(|v| u32::try_from(v).ok()),
        limit: parse_u64(ctx.query_value("limit")).unwrap_or(100),
        offset: parse_u64(ctx.query_value("offset")).unwrap_or(0),
    };
    // Masking follows the existing admin convention: the `is_auditor` flag
    // comes from the wrapper, and this server has no role header today (the
    // sibling `h_admin_instances_search` passes `false` for the same reason).
    // The surface is already `admin_cidrs`-gated, and PatientID - the
    // identifier - is always returned, so an operator can act on a row.
    let is_auditor = false;
    render_admin(
        ctx,
        crate::handlers::admin::pure::admin_instances_hidden(ctx.state, &params, is_auditor),
    )
}

/// `GET /admin/v1/instances/raw?sop_instance_uid=<uid>` - retained bytes.
///
/// Deliberately not gated by `expose_rejected_cidrs`: this is the admin
/// surface, and an operator investigating a rejection needs the referenced
/// images too, not only the hidden object.
///
/// The read is written to the access journal before the bytes leave: an admin
/// reading a sequestered instance is exactly the event an auditor must be able
/// to find afterwards, and the journal is the trail that answers "who looked".
///
/// ap101: N/A - one CAS read on a per-operator admin request.
fn h_admin_instances_raw(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    use crate::handlers::admin::pure::admin_instance_raw;

    let Some(sop_uid) = ctx.query_value("sop_instance_uid") else {
        let mut body = Vec::new();
        body.extend_from_slice(b"{\"error\":\"Bad Request\",\"message\":");
        crate::handlers::json_common::push_json_str(
            &mut body,
            "The 'sop_instance_uid' query parameter is required",
        );
        body.push(b'}');
        return write_resp(
            ctx.conn,
            400,
            JSON_CT,
            &body,
            ctx.keep_alive == KeepAlive::On,
        );
    };

    match admin_instance_raw(ctx.state, &sop_uid) {
        Ok(raw) => {
            // Self-audit BEFORE the response leaves, so a client that
            // disconnects mid-body still leaves the read recorded. An admin
            // reading a sequestered instance is exactly the event an auditor
            // must be able to find afterwards, and the journal is the trail
            // that answers "who looked".
            crate::access_log::emit(&crate::access_log::AccessEvent {
                event: "access",
                ts: crate::audit_log::utc_now_iso(),
                user: std::str::from_utf8(ctx.user)
                    .unwrap_or("anonymous")
                    .to_string(),
                src_ip: std::str::from_utf8(ctx.client_ip).unwrap_or("").to_string(),
                // The transport the request actually arrived on: a hardcoded
                // "loopback" labelled an admin read over the network as a local
                // one, which is the wrong answer to an auditor's "how did this
                // data leave" question.
                via: ctx.via,
                action: "R",
                atna: "110101",
                status: 200,
                study: raw.study_uid,
                series: raw.series_uid,
                sop: sop_uid,
                patient: raw.patient_id,
                obj_type: "110180",
                q: String::new(),
                count: 1,
                changed_attrs: 0,
                bytes_replaced: 0,
                reason: "",
            });
            write_resp(
                ctx.conn,
                200,
                b"application/dicom",
                &raw.bytes,
                ctx.keep_alive == KeepAlive::On,
            )
        }
        Err(e) => write_resp(
            ctx.conn,
            e.status,
            JSON_CT,
            &e.body,
            ctx.keep_alive == KeepAlive::On,
        ),
    }
}

/// `POST /admin/v1/instances/restore` - clear a rejection (Clarus extension).
///
/// ap101: N/A - manifest write on a CIDR-gated admin request.
fn h_admin_instances_restore(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let Some(body) = admin_json_body(ctx)? else {
        return Ok(());
    };
    let user = std::str::from_utf8(ctx.user).unwrap_or("anonymous");
    let result = crate::handlers::admin::pure::admin_restore(ctx.state, &body, user);
    if let Err(ref e) = result {
        crate::clog_warn!("admin restore refused (status {})", e.status);
    }
    render_admin(ctx, result)
}

/// Read `?dry_run=` as a boolean, or reject it.
///
/// `None` (the parameter is absent) is `false` - the historical default. A
/// substring test cannot tell `dry_run=false` from `dry_run=true`, and the
/// dangerous direction is the one that reads `dry_run=no` as TRUE: a
/// destructive maintenance or audit run then becomes a no-op while reporting
/// success. Anything not recognisably boolean is a 400, because guessing is
/// what turned the value into its opposite.
fn parse_dry_run(ctx: &RequestContext<'_>) -> Result<bool, String> {
    // A bare `?dry_run` is the conventional HTTP spelling of a true flag, and
    // `query_value` cannot see it: it skips a pair whose value is empty, which
    // makes a valueless flag look absent. Presence is therefore checked on the
    // raw query, and only a value that is actually present is parsed.
    let Some(raw) = ctx.query_str() else {
        return Ok(false);
    };
    let Some(value) = raw.split('&').find_map(|pair| match pair.split_once('=') {
        Some(("dry_run", v)) => Some(v),
        None if pair == "dry_run" => Some(""),
        _ => None,
    }) else {
        return Ok(false);
    };
    if value.is_empty() {
        return Ok(true);
    }
    let v = crate::handlers::wado::types::url_decode(value);
    if v.eq_ignore_ascii_case("true") || v == "1" {
        return Ok(true);
    }
    if v.eq_ignore_ascii_case("false") || v == "0" {
        return Ok(false);
    }
    Err(format!("dry_run must be true or false, got '{v}'"))
}

fn h_admin_maintenance(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let Some(body) = admin_json_body(ctx)? else {
        return Ok(());
    };
    let user = std::str::from_utf8(ctx.user).unwrap_or("anonymous");
    let dry_run = match parse_dry_run(ctx) {
        Ok(v) => v,
        Err(msg) => {
            return render_admin(
                ctx,
                Err(crate::handlers::admin::pure::AdminErr {
                    status: 400,
                    body: crate::handlers::json_common::err_msg_json("Bad Request", &msg),
                }),
            );
        }
    };
    render_admin(
        ctx,
        crate::handlers::admin::pure::admin_maintenance(ctx.state, &body, user, dry_run),
    )
}

fn h_admin_storage_migrate(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let Some(body) = admin_json_body(ctx)? else {
        return Ok(());
    };
    render_admin(
        ctx,
        crate::handlers::admin::pure::admin_migrate(ctx.state, &body),
    )
}

fn h_admin_instances_delete(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let Some(body) = admin_json_body(ctx)? else {
        return Ok(());
    };
    let user = std::str::from_utf8(ctx.user).unwrap_or("anonymous");
    let result = crate::handlers::admin::pure::admin_delete(ctx.state, &body, user);
    // FR-014: a rejected deletion is itself operator-relevant (who tried, why
    // it was refused). WARN always passes the level filter.
    if let Err(ref e) = result {
        crate::clog_warn!(
            "admin deletion rejected status={} user={} ip={}: {}",
            e.status,
            user,
            std::str::from_utf8(ctx.client_ip).unwrap_or("<bad-utf8>"),
            std::str::from_utf8(&e.body).unwrap_or("<non-utf8 body>"),
        );
    }
    render_admin(ctx, result)
}

fn h_admin_cas_audit(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let dry_run = match parse_dry_run(ctx) {
        Ok(v) => v,
        Err(msg) => {
            return render_admin(
                ctx,
                Err(crate::handlers::admin::pure::AdminErr {
                    status: 400,
                    body: crate::handlers::json_common::err_msg_json("Bad Request", &msg),
                }),
            );
        }
    };
    let result = crate::handlers::admin::pure::admin_cas_audit_start(ctx.state, dry_run);
    // when the pure side accepted (202), run the actual CAS walk on a
    // background std thread (no tokio, per) and stream progress back
    // into the shared AUDIT_STATE. The already-running guard (409) stays in
    // pure, so at most one walker is alive here.
    if let Ok(resp) = &result
        && resp.status == 202
    {
        let cas_root = ctx.state.cas.root_dir().to_path_buf();
        let temp_dir = ctx.state.cas.temp_dir();
        let manifest_dir = std::path::PathBuf::from(&ctx.state.config.btree.manifest_dir);
        std::thread::Builder::new()
            .name("cas-audit".into())
            .spawn(move || {
                crate::cas_audit_worker::run_cas_audit(
                    cas_root,
                    manifest_dir,
                    temp_dir,
                    &crate::handlers::admin::pure::AUDIT_STATE,
                )
            })
            .expect("Failed to spawn cas-audit thread");
    }
    render_admin(ctx, result)
}

// cas->manifests rebuild. `POST /admin/v1/recovery/rebuild`.
// On 202 (accepted by pure), run `run_rebuild` on a background std thread
// (no tokio, per), streaming progress into the shared REBUILD_STATE.
// The 409 already-running guard stays in pure, so at most one rebuild walker
// is alive. `AppState: Clone` shares the RAM study index / S3 OnceLock, so the
// worker reflects committed studies into the live query path.
fn h_admin_recovery_rebuild(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let Some(body) = admin_json_body(ctx)? else {
        return Ok(());
    };
    let dry_run = match parse_dry_run(ctx) {
        Ok(v) => v,
        Err(msg) => {
            return render_admin(
                ctx,
                Err(crate::handlers::admin::pure::AdminErr {
                    status: 400,
                    body: crate::handlers::json_common::err_msg_json("Bad Request", &msg),
                }),
            );
        }
    };
    let result = crate::handlers::admin::pure::admin_recovery_rebuild(ctx.state, &body, dry_run);
    if let Ok(resp) = &result
        && resp.status == 202
    {
        let state_owned = ctx.state.clone();
        let sources = {
            let st = crate::handlers::admin::pure::REBUILD_STATE
                .read()
                .expect("Rebuild state lock poisoned");
            st.sources.clone()
        };
        let dry = dry_run;
        std::thread::Builder::new()
            .name("rebuild".into())
            .spawn(move || {
                crate::rebuild_worker::run_rebuild(
                    &state_owned,
                    &sources,
                    dry,
                    &crate::handlers::admin::pure::REBUILD_STATE,
                )
            })
            .expect("Failed to spawn rebuild thread");
    }
    render_admin(ctx, result)
}

// `GET /admin/v1/recovery/rebuild/status` - live rebuild progress.
fn h_admin_recovery_rebuild_status(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    render_admin(
        ctx,
        crate::handlers::admin::pure::admin_recovery_rebuild_status(ctx.state),
    )
}

// -------------------------------------------------------------------------
// WADO metadata (representative, buffered, item-missing -> 404)
// -------------------------------------------------------------------------

fn h_wado_metadata(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let ka = ctx.keep_alive == KeepAlive::On;
    // Resolve the instance from the manifest (locate_object) + read the CAS
    // `.dcm`, then render metadata natively (no SQLite metadata_blob). 404 if
    // absent or invalid. The series UID is not needed for lookup, so `None`.
    //
    // The QA root answers metadata for sequestered instances too: it is the
    // one place a reviewer can inspect what was hidden, and the same lookup
    // rule every other QA read follows. A hardcoded `false` here made
    // `/dicomweb_qa` hide its own evidence - the instance appeared in the QA
    // search results and then answered 404 on the very next request.
    let oref = match crate::handlers::wado_pure::locate_object(
        ctx.state,
        ctx.uid(0),
        None,
        ctx.uid(2),
        expose_on_this_route(ctx),
    ) {
        Ok(v) => v,
        Err(_) => {
            let body = err_body(404, "Not Found");
            return write_resp(ctx.conn, 404, JSON_CT, &body, ka);
        }
    };
    let sop_uid = ctx.uid(2);
    // 0310 F3: bounded header read - the metadata renderer stops at PixelData,
    // so only `max_part_header` bytes of a possibly multi-GB instance are read.
    let head = match crate::dispatch_streams::read_cas_head(
        ctx.state,
        &oref.0.sha256,
        oref.0.storage_tier,
        ctx.state.config.max_part_header,
    ) {
        Ok(b) => b,
        Err(_) => {
            let body = err_body(404, "Not Found");
            return write_resp(ctx.conn, 404, JSON_CT, &body, ka);
        }
    };
    // DICOM -> DICOMweb JSON + BulkDataURI injection via the native
    // (serde-free) renderer, byte-identical to the old blob path.
    let resp = match crate::handlers::wado_pure::metadata_from_dcm_bounded(
        &head,
        ctx.uid(0),
        ctx.uid(1),
        sop_uid,
    ) {
        Ok(r) => r,
        Err(_e) => {
            let body = err_body(500, "Internal Server Error");
            return write_resp(ctx.conn, 500, JSON_CT, &body, ka);
        }
    };
    // Accept negotiation (PS3.18 10.4.4): XML is Required, JSON the Default.
    if crate::handlers::qido_pure::negotiate(ctx.accept())
        == crate::handlers::DICOM_XML_CONTENT_TYPE
    {
        // Re-render the DICOMweb JSON as DICOM XML (PS3.19, byte-parity with
        // the qido_xml oracle) and return a buffered multipart/related with a
        // single `application/dicom+xml` part. Render failure -> 500 (like the
        // JSON branch).
        let xml = match crate::handlers::instance_xml::json_metadata_to_xml(&resp.body) {
            Ok(x) => x,
            Err(_) => {
                let body = err_body(500, "Internal Server Error");
                return write_resp(ctx.conn, 500, JSON_CT, &body, ka);
            }
        };
        let boundary = clarus_adapters::boundary_id();
        let ct = format!(
            "multipart/related; type=\"{}\"; boundary=\"{boundary}\"",
            crate::handlers::DICOM_XML_CONTENT_TYPE
        );
        let mut body = Vec::with_capacity(xml.len() + 256);
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Type: application/dicom+xml\r\n");
        body.extend_from_slice(format!("Content-Length: {}\r\n\r\n", xml.len()).as_bytes());
        body.extend_from_slice(&xml);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        clarus_core::http::response::build_head(
            &mut ctx.conn.header,
            resp.status,
            reason_phrase(resp.status),
            Some(ct.as_bytes()),
            Some(body.len() as u64),
            ka,
            &[],
        );
        ctx.conn.write_head_configured()?;
        return ctx.conn.write_all_configured(&body);
    }
    clarus_core::http::response::build_head(
        &mut ctx.conn.header,
        resp.status,
        reason_phrase(resp.status),
        Some(resp.content_type.as_bytes()),
        Some(resp.body.len() as u64),
        ka,
        &[],
    );
    ctx.conn.write_head_configured()?;
    ctx.conn.write_all_configured(&resp.body)
}

fn error_413(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let body = err_body(413, "Payload Too Large");
    write_resp(
        ctx.conn,
        413,
        JSON_CT,
        &body,
        ctx.keep_alive == KeepAlive::On,
    )
}

// -------------------------------------------------------------------------
// Route table (ids unique - proven by the integrity test in dispatch_smoke.rs;
// lookup is linear over ~30 entries, binary_search would gain nothing)
// -------------------------------------------------------------------------

pub static ROUTES: &[(&str, HandlerFn)] = &[
    ("admin_audit_log", h_admin_audit_log),
    ("admin_access_log", h_admin_access_log),
    ("admin_audit_copies", h_admin_audit_copies),
    ("admin_cas_audit", h_admin_cas_audit),
    ("admin_cas_audit_status", h_admin_cas_audit_status),
    ("admin_dashboard", h_admin_dashboard),
    ("admin_health", h_admin_health),
    ("admin_instances_delete", h_admin_instances_delete),
    ("admin_instances_hidden", h_admin_instances_hidden),
    ("admin_instances_raw", h_admin_instances_raw),
    ("admin_instances_restore", h_admin_instances_restore),
    ("admin_instances_search", h_admin_instances_search),
    ("admin_maintenance", h_admin_maintenance),
    ("admin_metrics_history", h_admin_metrics_history),
    ("admin_recovery_rebuild", h_admin_recovery_rebuild),
    (
        "admin_recovery_rebuild_status",
        h_admin_recovery_rebuild_status,
    ),
    ("admin_storage", h_admin_storage),
    ("admin_storage_migrate", h_admin_storage_migrate),
    ("admin_storage_s3", h_admin_storage_s3),
    ("admin_status", h_admin_status),
    ("admin_version", h_admin_version),
    ("capabilities", h_capabilities),
    ("capabilities_redirect", h_capabilities_redirect),
    ("commitment_check", h_commitment_check),
    ("commitment_commit", h_commitment_commit),
    ("health", h_health),
    ("health_status", h_health_status),
    ("metrics", h_metrics),
    ("version", h_version),
    ("mpps_create_or_update", h_mpps_create_or_update),
    ("mpps_get", h_mpps_get),
    ("mwl_search", h_mwl_search),
    ("qido_patients", h_qido_patients),
    ("qido_series", h_qido_series),
    ("qido_series_instances", h_qido_series_instances),
    ("qido_series_global", h_qido_series_global),
    ("qido_series_root_instances", h_qido_series_root_instances),
    ("qido_instances_global", h_qido_instances_global),
    ("qido_studies", h_qido_studies),
    ("qido_study_instances", h_qido_study_instances),
    ("ups_create", h_ups_create_or_search),
    ("ups_delete", h_ups_delete),
    ("ups_get", h_ups_get),
    ("ups_search", h_ups_search),
    ("ups_subscribe", h_ups_subscribe_plain),
    ("ups_subscribe_filtered", h_ups_subscribe_filtered),
    ("ups_subscribe_workitem", h_ups_subscribe_workitem),
    ("ups_unsubscribe", h_ups_unsubscribe),
    ("ups_unsubscribe_workitem", h_ups_unsubscribe_workitem),
    ("ups_update", h_ups_update),
    ("ups_update_attributes", h_ups_update_attributes),
    ("ups_cancelrequest", h_ups_cancelrequest),
    ("viewer", h_viewer),
    ("wado_metadata", h_wado_metadata),
    // Streaming transport handlers (the production wire).
    ("wado_instance", crate::dispatch_streams::h_wado_instance),
    ("wado_uri", crate::dispatch_streams::h_wado_uri),
    ("wado_study", crate::dispatch_streams::h_wado_study),
    ("wado_series", crate::dispatch_streams::h_wado_series),
    (
        "wado_series_metadata",
        crate::dispatch_streams::h_wado_series_metadata,
    ),
    (
        "wado_study_metadata",
        crate::dispatch_streams::h_wado_study_metadata,
    ),
    (
        "wado_series_thumbnail",
        crate::dispatch_streams::h_wado_series_thumbnail,
    ),
    ("wado_thumbnail", crate::dispatch_streams::h_wado_thumbnail),
    ("wado_rendered", crate::dispatch_streams::h_wado_rendered),
    (
        "wado_study_thumbnail",
        crate::dispatch_streams::h_wado_study_thumbnail,
    ),
    (
        "wado_frame_thumbnail",
        crate::dispatch_streams::h_wado_frame_thumbnail,
    ),
    ("wado_frame", crate::dispatch_streams::h_wado_frame),
    ("wado_bulkdata", crate::dispatch_streams::h_wado_bulkdata),
    ("stow_studies", crate::dispatch_streams::h_stow_studies),
    ("stow_study", crate::dispatch_streams::h_stow_study),
    ("ups_events", crate::dispatch_streams::h_ups_events),
    (
        "ups_notification_connect",
        crate::dispatch_streams::h_ups_notification_connect,
    ),
    (
        "admin_logs_stream",
        crate::dispatch_streams::h_admin_logs_stream,
    ),
];

/// Number of entries in the route table (exposed for the integrity test).
pub fn table_len() -> usize {
    ROUTES.len()
}

#[cfg(test)]
mod tests {
    use super::expose_rejected_for;
    use crate::config::parse_cidr;

    fn nets(spec: &[&str]) -> Vec<crate::config::CidrNet> {
        spec.iter()
            .map(|s| parse_cidr(s).expect("valid cidr"))
            .collect()
    }

    #[test]
    fn loopback_may_always_expose_rejected() {
        // The default configuration grants nothing, so QA work on the server
        // itself must not require an explicit network entry.
        assert!(expose_rejected_for(b"127.0.0.1", &[]));
        assert!(expose_rejected_for(b"::1", &[]));
    }

    #[test]
    fn an_unconfigured_remote_client_may_not_expose_rejected() {
        assert!(!expose_rejected_for(b"10.1.2.3", &[]));
        assert!(!expose_rejected_for(b"203.0.113.9", &[]));
    }

    #[test]
    fn a_configured_remote_client_may_expose_rejected() {
        let cidrs = nets(&["10.1.0.0/16"]);
        assert!(expose_rejected_for(b"10.1.99.4", &cidrs));
        // One step outside the prefix stays refused.
        assert!(!expose_rejected_for(b"10.2.0.1", &cidrs));
    }

    #[test]
    fn an_unparsable_client_address_is_refused() {
        // Guessing at a malformed address would be a way to slip past the
        // gate, so it must fail closed rather than fall through to allow.
        assert!(!expose_rejected_for(b"not-an-ip", &nets(&["0.0.0.0/0"])));
        assert!(!expose_rejected_for(b"", &nets(&["0.0.0.0/0"])));
    }

    #[test]
    fn only_the_expose_list_grants_expose_rejected() {
        // The two grants are separate lists; this asserts the separation at
        // the decision point rather than relying on configuration alone.
        let expose = nets(&["192.168.60.0/24"]);
        assert!(expose_rejected_for(b"192.168.60.5", &expose));
        assert!(!expose_rejected_for(b"192.168.50.5", &expose));
    }
}
