// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Synchronous accept loop + per-connection loop.
//!
//! This is the transport: a plain std accept loop, one OS thread per
//! connection, and a keep-alive-capable read/dispatch/write loop on each.
//! axum and tokio are absent from the dependency tree entirely - they are not
//! compiled, not vendored, and not used. All blocking I/O is std; the only
//! coordination primitives are the atomics/condvar in
//! [`clarus_core::shutdown::Shutdown`].
//!
//! # Shutdown contract
//! - A signal thread sets a flag; the accept thread polls it (non-blocking
//!   accept), so no OS-specific unblock trick is needed.
//! - Worker threads finish the in-flight request, then exit (observed via the
//!   active counter).
//! - `serve` returns when shutdown is requested; the caller drains and joins.

#![forbid(unsafe_code)]

use std::io::ErrorKind;
use std::io::Write;
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use clarus_core::http::body::framing;
use clarus_core::http::conn::{Conn, HEADER_BUF_SIZE, HeadRead, Timeouts};
use clarus_core::http::parse::{parse_headers, parse_request_line};
use clarus_core::http::router::{RouterOutcome, match_route};
use clarus_core::shutdown::Shutdown;

use crate::dispatch::{RequestContext, dispatch};
use crate::route_classify::RouteId;
use crate::state::AppState;

/// Worker thread stack size: 512 KiB is ample for the sync stack and avoids
/// the 2 MiB default commit footprint per connection.
const WORKER_STACK: usize = 512 * 1024;

/// Sleep between non-blocking accept retries (both poll cadence and a coarse
/// shutdown-latency bound). A1 (28.08): 75 ms -> 5 ms - an idle server now
/// answers a fresh connection in ~2.5 ms average instead of ~37.5 ms; idle
/// cost is 200 non-blocking accept polls per second.
const ACCEPT_DELAY: Duration = Duration::from_millis(5);

/// Slice of the keep-alive idle deadline after which a parked connection
/// re-checks the stop flag.
///
/// Without the slice an idle keep-alive worker sits in `read_head` until the
/// keep-alive timeout (75 s here) and cannot notice a stop, so the drain waits
/// that long per open connection and a Ctrl+C looks like a hang. 250 ms bounds
/// the extra stop latency while costing an idle connection four wake-ups per
/// second - negligible next to the per-connection 512 KiB stack the worker
/// already holds, and the read itself is the only syscall in that path.
const KEEPALIVE_POLL: Duration = Duration::from_millis(250);

/// Fallback user when no authenticated identity arrived (anonymous).
const ANON_USER: &[u8] = b"anonymous";

/// Journal label for the embedded viewer: a loopback client that echoed the
/// server's `x-clarus: definitum semel` stamp back. Label only, not security.
const VIEWER_USER: &[u8] = b"(miniviewer)";

/// Journal label for any other direct loopback client (curl, bridge,
/// cltranscode) - distinct from nginx-proxied `anonymous` traffic.
const LOOPBACK_USER: &[u8] = b"(loopback)";

/// How long a request may run before the handler logs a WARN about it.
///
/// Fixed, and deliberately well below the client's own read timeout (clients
/// in this deployment time a request out at 30 s): the point is for the
/// operator to see a slow request WHILE it is still running and finishable,
/// not to discover it after the client has already given up and cancelled.
/// Ten seconds is far above any healthy handler on this server - the busiest
/// route answers in microseconds - so every line this produces is a real
/// candidate for investigation, which is why there is no rate limit on it.
const SLOW_REQUEST: Duration = Duration::from_secs(10);

/// First element of an X-Forwarded-For list (nginx overwrites, not appends,
/// but a comma list is accepted; the first hop is the client).
fn first_xff(v: &[u8]) -> &[u8] {
    match v.iter().position(|&b| b == b',') {
        Some(i) => {
            let mut first = &v[..i];
            while first.last() == Some(&b' ') {
                first = &first[..first.len() - 1];
            }
            first
        }
        None => v,
    }
}

/// Is the socket peer inside one of the trusted-proxy networks?
fn peer_is_trusted(peer: &std::net::SocketAddr, nets: &[crate::config::CidrNet]) -> bool {
    // `to_canonical()` first: a dual-stack listener reports an IPv4 client as
    // `::ffff:127.0.0.1`, and matching that v6 address against 127.0.0.0/8
    // fails - so a trusted loopback proxy was not recognised as one and every
    // request it forwarded fell back to the untrusted path.
    match *peer {
        std::net::SocketAddr::V4(a) => {
            let ip = u32::from_be_bytes(a.ip().octets());
            nets.iter().any(|n| n.contains_v4(ip))
        }
        std::net::SocketAddr::V6(a) => match a.ip().to_canonical() {
            std::net::IpAddr::V4(v4) => {
                let ip = u32::from_be_bytes(v4.octets());
                nets.iter().any(|n| n.contains_v4(ip))
            }
            std::net::IpAddr::V6(v6) => {
                let ip = u128::from_be_bytes(v6.octets());
                nets.iter().any(|n| n.contains_v6(ip))
            }
        },
    }
}

/// Emit a rate-limited auth WARN for the two benign-but-suspicious identity
/// cases (see the caller). Returns `true` when a WARN was emitted (for call
/// sites that need the signal); the emission is gated at 1/60s per peer.
fn rate_auth_warn(
    client: &clarus_core::http::auth::Identity,
    sent_proxy_headers: bool,
    peer_key: &[u8],
) -> bool {
    use clarus_core::http::auth::Identity;
    let warn = match client {
        Identity::MissingIdentity => true,
        Identity::UntrustedPeer { .. } => {
            // Only flag actual spoofing attempts (proxy headers shipped), not
            // ordinary anonymous traffic from outside the trusted set.
            sent_proxy_headers
        }
        _ => false,
    };
    if !warn {
        return false;
    }
    if !crate::clog::rate_limited_allow(peer_key) {
        return false;
    }
    let peer = std::str::from_utf8(peer_key).unwrap_or("<bad-utf8>");
    match client {
        Identity::MissingIdentity => crate::clog_warn!(
            peer = %peer,
            "Trusted proxy sent identity headers but no authenticated user (MissingIdentity); request treated as anonymous",
        ),
        _ => crate::clog_warn!(
            peer = %peer,
            "Untrusted peer sent identity (proxy) headers; headers ignored (potential spoofing)",
        ),
    }
    true
}

/// Pick the journal user label for a direct loopback client. The embedded
/// viewer echoes the `x-clarus: definitum semel` stamp back on every request;
/// any other local tool is a plain `(loopback)`. A curl can forge the marker
/// exactly like the auditor role nginx sets - cosmetic label, not security.
fn local_user_label(x_clarus: Option<&[u8]>) -> &'static [u8] {
    if x_clarus == Some(b"definitum semel") {
        VIEWER_USER
    } else {
        LOOPBACK_USER
    }
}

/// Resolved client is a loopback address (LocalhostOnly gate, DoD E).
fn resolved_is_loopback(
    client: &clarus_core::http::auth::Identity,
    peer: &std::net::SocketAddr,
) -> bool {
    let eff = clarus_core::http::auth::effective_ip(client);
    if eff.is_empty() {
        // LocalViewer / MissingIdentity: the address that matters is the peer.
        // Canonicalised for the same reason as `peer_is_trusted`: a dual-stack
        // socket reports a loopback IPv4 client as `::ffff:127.0.0.1`, which
        // `is_loopback()` reports false for.
        return peer.ip().to_canonical().is_loopback();
    }
    std::str::from_utf8(eff)
        .ok()
        .and_then(|s| s.parse::<std::net::IpAddr>().ok())
        .map(|a| a.to_canonical().is_loopback())
        .unwrap_or(false)
}

/// Is `ip` inside one of the configured networks?
fn ip_in_cidrs(ip: std::net::IpAddr, nets: &[crate::config::CidrNet]) -> bool {
    match ip.to_canonical() {
        std::net::IpAddr::V4(a) => {
            let n = u32::from_be_bytes(a.octets());
            nets.iter().any(|c| c.contains_v4(n))
        }
        std::net::IpAddr::V6(a) => {
            let n = u128::from_be_bytes(a.octets());
            nets.iter().any(|c| c.contains_v6(n))
        }
    }
}

/// Is the **resolved** client IP inside one of `nets`?
///
/// `effective_ip` is the single source of truth for "which address is the
/// client" - for a trusted-proxy request it is the address nginx reported, for
/// a direct request it is the socket peer. Matching CIDRs against anything
/// else (the raw socket, say) would 403 a legitimate management workstation
/// that reaches the server through nginx, because the socket peer there is
/// nginx itself on loopback.
///
/// An unparsable or absent resolved IP is never inside a network, so
/// `MissingIdentity` (trusted proxy that sent no identity headers) is denied
/// rather than silently trusted.
fn resolved_ip_in_cidrs(
    client: &clarus_core::http::auth::Identity,
    nets: &[crate::config::CidrNet],
) -> bool {
    let eff = clarus_core::http::auth::effective_ip(client);
    if eff.is_empty() {
        return false;
    }
    std::str::from_utf8(eff)
        .ok()
        .and_then(|s| s.parse::<std::net::IpAddr>().ok())
        .is_some_and(|ip| ip_in_cidrs(ip, nets))
}

/// May this client reach the administrative surface?
///
/// Policy (single definition, used by every `Access::Admin` route):
/// 1. the resolved client is on the local machine - always allowed, so the
///    default configuration (no `admin_cidrs`) behaves exactly like the old
///    loopback-only gate, SSH tunnel included;
/// 2. otherwise the resolved client IP is inside `admin_cidrs`.
fn admin_allowed(
    client: &clarus_core::http::auth::Identity,
    peer: &std::net::SocketAddr,
    admin_cidrs: &[crate::config::CidrNet],
) -> bool {
    resolved_is_loopback(client, peer) || resolved_ip_in_cidrs(client, admin_cidrs)
}

/// May this client ask to see sequestered (rejected) instances?
///
/// Same two-part rule as [`admin_allowed`], against a DIFFERENT network list:
///
/// 1. the resolved client is on the local machine - always allowed, so QA work
///    on the server itself needs no configuration;
/// 2. otherwise the resolved client IP is inside `expose_rejected_cidrs`.
///
/// The lists are deliberately separate. Lifting the sequestering filter is a
/// read; administrative access may change configuration. Conflating them would
/// silently hand configuration rights to whoever needs to review a rejected
/// series, and would make the restrictive default of one surface depend on the
/// other's configuration.
pub fn expose_rejected_allowed(
    client: &clarus_core::http::auth::Identity,
    peer: &std::net::SocketAddr,
    expose_cidrs: &[crate::config::CidrNet],
) -> bool {
    resolved_is_loopback(client, peer) || resolved_ip_in_cidrs(client, expose_cidrs)
}

/// Serve connections until the shutdown flag is raised.
///
/// # Arguments
/// - `listener`: an already-bound listener; the accept loop switches it to
///   non-blocking so shutdown does not depend on an OS-specific unblock.
/// - `state`: shared `AppState` (cloned into each worker via `Arc`).
/// - `shutdown`: coordinated shutdown state (see module docs).
/// - `timeouts`: per-phase socket timeouts (header / body / keepalive / send).
/// - `max_connections`: ceiling on concurrent workers; above it new sockets
///   are closed without being read.
///
/// Returns once shutdown is requested and the accept loop drains. The caller
/// is responsible for joining the spawned worker threads afterwards.
pub fn serve(
    listener: TcpListener,
    state: Arc<AppState>,
    shutdown: Shutdown,
    timeouts: Timeouts,
    max_connections: u32,
) {
    let _ = listener.set_nonblocking(true);
    loop {
        match listener.accept() {
            Ok((stream, peer)) => {
                let state = Arc::clone(&state);
                let shutdown = shutdown.clone();
                let _ = std::thread::Builder::new()
                    .name("conn-worker".into())
                    .stack_size(WORKER_STACK)
                    .spawn(move || {
                        serve_conn(stream, peer, state, shutdown, timeouts, max_connections);
                    });
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                // Non-blocking accept: no socket pending. Poll the shutdown
                // flag so Ctrl+C exits even with no connections arriving.
                if shutdown.stopping() {
                    return;
                }
                std::thread::sleep(ACCEPT_DELAY);
            }
            Err(e) => {
                crate::clog_warn!(error = %e, "accept failed; retrying");
                std::thread::sleep(ACCEPT_DELAY);
            }
        }
    }
}

/// Holds one slot in the connection accounting until dropped.
///
/// The slot was released by hand at each `return`, so a panic inside a handler
/// unwound past every one of them and leaked a slot for the life of the
/// process. Once the leaks reached `max_connections` the server refused every
/// connection while reporting itself healthy - a permanent outage from one
/// crashing request. The guard releases on unwind as well.
struct ConnSlot<'a> {
    shutdown: &'a Shutdown,
}

impl<'a> ConnSlot<'a> {
    fn enter(shutdown: &'a Shutdown) -> Self {
        shutdown.enter();
        Self { shutdown }
    }
}

impl Drop for ConnSlot<'_> {
    fn drop(&mut self) {
        self.shutdown.leave();
    }
}

/// Run one connection: a keep-alive request loop, then exit.
fn serve_conn(
    stream: std::net::TcpStream,
    peer: std::net::SocketAddr,
    state: Arc<AppState>,
    shutdown: Shutdown,
    timeouts: Timeouts,
    max_connections: u32,
) {
    let slot = ConnSlot::enter(&shutdown);
    if shutdown.active() > max_connections as usize {
        // Over the connection ceiling: close without reading a byte. The guard
        // releases the slot as this scope ends.
        return;
    }

    // The listener is non-blocking so the accept loop can poll the shutdown
    // flag; accepted sockets may inherit that flag, so re-enable blocking I/O
    // on the worker's socket explicitly (per-connection blocking reads/writes).
    let _ = stream.set_nonblocking(false);
    let mut conn = Conn::new(stream, peer.ip().to_string().into_bytes());
    conn.set_send_timeout(timeouts.send);

    // First read deadline is the header timeout; after the first request it
    // becomes the keep-alive idle timeout so an idle connection does not hold
    // a worker forever.
    let mut header_deadline = timeouts.header;
    // A2: the request head buffer is a stack local owned by this loop (NOT
    // copied out of the Conn). `handle_one` borrows the slice while using
    // `&mut conn` - a slice derived from the Conn itself would alias it.
    let mut head_buf = [0u8; HEADER_BUF_SIZE];
    loop {
        // While the connection idles in keep-alive, a blocking read parked for
        // the whole idle window kept the worker (and the drain behind it) up
        // to 75 s after Ctrl+C. The read is therefore sliced into short polls:
        // each slice re-checks the stop flag, so an idle connection closes
        // promptly instead of waiting out the timeout. The first request keeps
        // the full header deadline (a client mid-request is not interrupted).
        let mut remaining = header_deadline;
        let mut collected = 0usize;
        let n = loop {
            let slice = remaining.min(KEEPALIVE_POLL);
            // The first slice resets per-request state; every later slice
            // resumes after the bytes already in `head_buf`, so a header that
            // trickles in across polls is neither re-read nor lost.
            let outcome = if collected == 0 {
                conn.begin_head(&mut head_buf, slice)
            } else {
                conn.read_head_from(&mut head_buf, collected)
            };
            match outcome {
                Err(_) => {
                    return;
                }
                Ok(HeadRead::Complete(n)) => break n,
                Ok(HeadRead::Eof(n)) => break n,
                Ok(HeadRead::Idle(used)) => {
                    collected = used;
                    // A stop may only cut off a connection that has NOTHING
                    // buffered. `used > 0` means the peer has already sent part
                    // of its request head: that is a request in progress, and
                    // closing it would answer nothing the client is waiting
                    // for. Such a connection is left to finish or to hit the
                    // header deadline, exactly as a request that had not yet
                    // begun to arrive would.
                    if collected == 0 && shutdown.stopping() {
                        // Idle and asked to stop: end the connection now.
                        return;
                    }
                    remaining = match remaining.checked_sub(slice) {
                        Some(left) if !left.is_zero() => left,
                        _ => {
                            // The real deadline expired: same as before.
                            return;
                        }
                    };
                }
            }
        };
        if n == 0 {
            break; // peer closed cleanly
        }
        // The head may have arrived over short poll slices, leaving the last
        // slice as the socket's receive timeout. Restore the deadline the body
        // reader has always seen on this transport: the header value. The body
        // timer is deliberately not tightened here - a paused-but-recoverable
        // upload must not be killed by a poll slice meant for idle shutdowns.
        let _ = clarus_core::http::conn::set_timeouts(
            conn.stream(),
            timeouts.header,
            conn.send_timeout(),
        );
        if !handle_one(&mut conn, &head_buf[..n], &state, peer) {
            break;
        }
        // Shutdown: stop offering keep-alive. Without this the worker parks in
        // `read_head` for the whole keep-alive idle window - 75 s by default -
        // and the drain waits out that window on every open connection, so a
        // Ctrl+C appears to hang. The request in hand has already been
        // answered; closing now is the "as soon as practical" of a graceful
        // stop, and the client reconnects to the next instance.
        if shutdown.stopping() {
            break;
        }
        // Keep-alive: the next request is allowed to idle up to the keepalive
        // timeout before the worker gives up on the connection.
        header_deadline = timeouts.keepalive;
    }
    // `slot` releases the connection slot here, or on any unwind above.
    drop(slot);
}

/// Parse one request head, match and dispatch it, and report whether the
/// connection should stay open for another request.
fn handle_one(
    conn: &mut Conn,
    head_buf: &[u8],
    state: &Arc<AppState>,
    peer: std::net::SocketAddr,
) -> bool {
    let (mut head, off) = match parse_request_line(head_buf) {
        Ok(v) => v,
        Err(_) => {
            let _ = conn.send_error(400, "Bad Request", false);
            return false;
        }
    };
    // `parse_request_line` returns `off` pointing AT the request-line CRLF;
    // headers begin after it.
    if parse_headers(&mut head, &head_buf[off + 2..]).is_err() {
        let _ = conn.send_error(400, "Bad Request", false);
        return false;
    }

    // RFC 9110 s10.1.1: when Expect: 100-continue + content is pending, the
    // server MUST answer an interim 100 before reading the body (or a final
    // status without reading it). 1xx is a connection-level reply - write it
    // straight to the socket, OUTSIDE response_bytes (nginx likewise excludes
    // 1xx from body_bytes_sent).
    if !head.http10
        && head.expect_100_continue
        && !matches!(
            framing(head.content_length, head.transfer_encoding),
            clarus_core::http::body::Framing::Length(0)
        )
    {
        // The client may have gone away; failing to write is non-fatal.
        let _ = conn
            .stream_mut()
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }

    match match_route(head.method, head.path) {
        RouterOutcome::Match(route) => {
            // E-009: proxy identity headers are trusted only from peers inside
            // trusted_proxies; spoofed headers from outsiders are ignored.
            let peer_ip_str = peer.ip().to_string();
            let trusted = peer_is_trusted(&peer, &state.config.trusted_proxies);
            let xff = head.x_forwarded_for.map(first_xff);
            let client = clarus_core::http::auth::resolve_identity(
                peer_ip_str.as_bytes(),
                trusted,
                head.x_real_ip,
                xff,
                head.x_auth_user,
            );
            if let Err(code) =
                clarus_core::http::auth::enforce(&client, state.config.require_proxy_identity)
            {
                let _ = conn.send_error(code, "Bad Gateway", false);
                return false;
            }
            // Auth warnings (rate-limited per peer, 1/60s). Two ephemeral
            // cases that are allowed through but indicate a config problem or
            // a spoofing attempt:
            //  - trusted peer with proxy headers but no authenticated user
            //    (MissingIdentity) in NON-strict mode (would be 502 if strict);
            //  - an untrusted peer that SHIPPED identity headers (spoofing) -
            //    the headers are ignored by resolve_identity.
            // Auth warnings (rate-limited per peer, 1/60s). Two ephemeral
            // cases are allowed through but indicate a config problem or a
            // spoofing attempt:
            //  - trusted peer with proxy headers but no authenticated user
            //    (MissingIdentity) in NON-strict mode (502 if strict);
            //  - an untrusted peer that SHIPPED identity headers (spoofing),
            //    which resolve_identity ignores.
            let _ = rate_auth_warn(
                &client,
                head.x_real_ip.is_some()
                    || head.x_forwarded_for.is_some()
                    || head.x_auth_user.is_some(),
                peer_ip_str.as_bytes(),
            );
            // Router access policy. `LocalhostOnly` (kept for any route that
            // later needs a hard loopback pin) answers only loopback clients;
            // `Admin` adds the configured admin networks. Both are decided on
            // the RESOLVED client identity, never the raw socket peer.
            let denied = match route.access {
                clarus_core::http::router::Access::LocalhostOnly => {
                    !resolved_is_loopback(&client, &peer)
                }
                clarus_core::http::router::Access::Admin => {
                    !admin_allowed(&client, &peer, &state.config.admin_cidrs)
                }
                _ => false,
            };
            // The QA service root additionally requires the expose grant. It
            // is checked here, before any handler runs, so no route on that
            // root can be reached without it - and the same predicate also
            // forces the expose view in the handlers, so there is one
            // definition of "may see hidden instances", not two.
            let denied = denied
                || (route.qa
                    && !expose_rejected_allowed(
                        &client,
                        &peer,
                        &state.config.expose_rejected_cidrs,
                    ));
            if denied {
                // Journal the refusal: an operator debugging a 403 needs to
                // know which address the server actually saw.
                crate::clog_warn!(
                    route = route.id,
                    peer = %peer_ip_str,
                    resolved = %std::str::from_utf8(clarus_core::http::auth::effective_ip(&client))
                        .unwrap_or("<bad-utf8>"),
                    "access denied by route policy (403)"
                );
                let _ = conn.send_error(403, "Forbidden", false);
                return false;
            }
            let keep_alive = route.keep_alive == clarus_core::http::router::KeepAlive::On
                && !head.connection_close;
            // Remember what the head promised, so a handler that answers
            // without draining the body can be detected before the response
            // head is committed (see `Conn::body_pending`).
            conn.set_announced_body(head.content_length);
            let route_id = classify(head.method, head.path);
            let started = std::time::Instant::now();
            crate::metrics::record_request_start();
            let eff_ip = clarus_core::http::auth::effective_ip(&client);
            let client_ip: &[u8] = if eff_ip.is_empty() {
                peer_ip_str.as_bytes()
            } else {
                eff_ip
            };
            let mut user: &[u8] = clarus_core::http::auth::effective_user(&client, ANON_USER);
            // Direct loopback clients get an honest label: the embedded
            // viewer when it echoes its marker, `(loopback)` otherwise.
            if matches!(&client, clarus_core::http::auth::Identity::LocalViewer)
                && peer.ip().is_loopback()
            {
                user = local_user_label(head.x_clarus);
            }
            let via: &'static str = match &client {
                clarus_core::http::auth::Identity::Proxied { .. }
                | clarus_core::http::auth::Identity::MissingIdentity => "nginx",
                clarus_core::http::auth::Identity::LocalViewer => "loopback",
                clarus_core::http::auth::Identity::UntrustedPeer { .. } => "bridge",
            };
            let mut ctx = RequestContext {
                head: &head,
                uids: route.uids,
                n_uids: route.n_uids,
                client_ip,
                user,
                via,
                state,
                // The COMPUTED per-connection value (route flag && !client
                // close), so response heads say Connection: close exactly
                // when the connection really closes after this request.
                keep_alive: if keep_alive {
                    clarus_core::http::router::KeepAlive::On
                } else {
                    clarus_core::http::router::KeepAlive::Off
                },
                conn,
                wado_series_expected: None,
                stow_accepted: 0,
                proxy_trusted: trusted,
                route,
            };
            // HEAD is routed like GET; the connection then sends the head
            // with the GET Content-Length and suppresses every body write.
            ctx.conn
                .set_suppress_body(head.method == clarus_core::http::parse::Method::Head);
            let _ = dispatch(&route, &mut ctx);
            let status = status_from_head(&ctx.conn.header);
            crate::metrics::record_request_end_u16(route_id, status, started);
            // Access-audit journal (ATNA-lite, 2808): ONE line per protected
            // route event, identifiers only, never blocking. Health/metrics
            // probes are excluded (not user traffic).
            if route_id != RouteId::Health && route_id != RouteId::Metrics {
                crate::access_log::record_request(
                    route.id,
                    head.query,
                    status,
                    user,
                    client_ip,
                    via,
                    &ctx.uids,
                    ctx.n_uids,
                    ctx.stow_accepted,
                );
                // Auth-visibility (design 9a): rate-limited WARN when
                // anonymous requests hit protected endpoints.
                if user == ANON_USER {
                    crate::access_log::warn_anonymous_served(
                        state.config.audit.auth_warn_interval_min,
                    );
                }
            }
            // Access log (nginx-style). Telemetry probes /health and /metrics
            // are excluded: they are load-balanced liveness pings, not user
            // traffic, and would drown the file.
            if route_id != RouteId::Health && route_id != RouteId::Metrics {
                let bytes = ctx.conn.response_bytes();
                let us = started.elapsed().as_micros();
                // WADO instance slices aggregate into one line per series (a
                // slice is a transport unit, not a readable event). Both the
                // raw and the multipart branches resolved the series count
                // into `ctx.wado_series_expected` via `locate_object`.
                if route.id == "wado_instance" {
                    // 3 UIDs are guaranteed on this route by the router.
                    let lines = crate::access_agg::record(
                        user,
                        client_ip,
                        ctx.uid(0).as_bytes(),
                        ctx.uid(1).as_bytes(),
                        status,
                        bytes,
                        us as u64,
                        ctx.wado_series_expected,
                    );
                    for line in lines {
                        crate::clog_event!(crate::clog::Level::Info, "WADO_SERIES", "", "{line}");
                    }
                    // The aggregate answers "how is this series doing"; a
                    // request that HANGS produces no aggregate line at all
                    // until its bucket sweeps, which is exactly the blind
                    // spot that hid a 30 s stall in the field. The
                    // per-request ACCESS line is therefore emitted here too,
                    // IN ADDITION to the aggregate, so a slow or lost slice
                    // is visible while it happens. `rt=` is the bucket the
                    // metrics already classify this route under
                    // (`classify_route` maps `/dicomweb/studies/...` to
                    // `WadoStudy`); the `path=` field is what tells a
                    // slice apart from a study-level retrieve.
                    let method_s = method_name(head.method);
                    let path_s = std::str::from_utf8(head.path).unwrap_or("<bad-utf8>");
                    let qs_s = std::str::from_utf8(head.query).unwrap_or("<bad-utf8>");
                    let ip_s = std::str::from_utf8(client_ip).unwrap_or("<bad-utf8>");
                    let user_s = std::str::from_utf8(user).unwrap_or("<bad-utf8>");
                    let rt_s = route_id_str(route_id);
                    let peer_s = peer.to_string();
                    let line = access_line(
                        status, method_s, path_s, qs_s, rt_s, &peer_s, ip_s, user_s, bytes, us,
                    );
                    crate::clog_event!(crate::clog::Level::Info, "ACCESS", "", "{line}");
                    // A failed slice is a transport-level event worth a WARN,
                    // but rate-limited per (ip, sop) so one broken SOP does
                    // not drown the file with a repeated WARN every 60s.
                    if status >= 400 {
                        let mut rkey = Vec::with_capacity(client_ip.len() + ctx.uid(2).len() + 1);
                        rkey.extend_from_slice(client_ip);
                        rkey.push(b'/');
                        rkey.extend_from_slice(ctx.uid(2).as_bytes());
                        if crate::clog::rate_limited_allow(&rkey) {
                            crate::clog_warn!(
                                "WADO slice failed study={} series={} sop={} status={} user={} ip={}",
                                ctx.uid(0),
                                ctx.uid(1),
                                ctx.uid(2),
                                status,
                                std::str::from_utf8(user).unwrap_or("<bad-utf8>"),
                                std::str::from_utf8(client_ip).unwrap_or("<bad-utf8>"),
                            );
                        }
                    }
                } else {
                    let method_s = method_name(head.method);
                    let path_s = std::str::from_utf8(head.path).unwrap_or("<bad-utf8>");
                    let qs_s = std::str::from_utf8(head.query).unwrap_or("<bad-utf8>");
                    let ip_s = std::str::from_utf8(client_ip).unwrap_or("<bad-utf8>");
                    let user_s = std::str::from_utf8(user).unwrap_or("<bad-utf8>");
                    let rt_s = route_id_str(route_id);
                    let peer_s = peer.to_string();
                    let line = access_line(
                        status, method_s, path_s, qs_s, rt_s, &peer_s, ip_s, user_s, bytes, us,
                    );
                    crate::clog_event!(crate::clog::Level::Info, "ACCESS", "", "{line}");
                }
            }
            // A request that outlived its budget gets one WARN, emitted
            // AFTER the handler returned so the reason is known and the
            // line cannot interleave with the handler's own output. No
            // rate limit: on this server a ten-second handler is a defect
            // worth a line every single time it happens, and silence is
            // what let a 30 s stall pass unnoticed.
            //
            // Push streams are exempt: for them a ten-second lifetime is the
            // design, not a defect (see `is_push_stream`). Checking the route
            // before the clock keeps the exemption here, at the one place the
            // WARN is emitted, rather than weakening `is_slow` for every
            // caller - the WADO case still needs that comparison to hold.
            let elapsed = started.elapsed();
            if !is_push_stream(route.id) && is_slow(elapsed, SLOW_REQUEST) {
                crate::clog_warn!(
                    "{}",
                    slow_request_line(
                        route.id,
                        route_id_str(route_id),
                        status,
                        if route.n_uids >= 3 {
                            Some((ctx.uid(0), ctx.uid(1), ctx.uid(2)))
                        } else {
                            None
                        },
                        std::str::from_utf8(head.path).unwrap_or("<bad-utf8>"),
                        std::str::from_utf8(user).unwrap_or("<bad-utf8>"),
                        std::str::from_utf8(client_ip).unwrap_or("<bad-utf8>"),
                        elapsed,
                    )
                );
            }
            keep_alive
        }
        RouterOutcome::Options => {
            // CORS preflight (Fetch spec): the browser sends OPTIONS with
            // Access-Control-Request-* before any cross-origin request whose
            // Content-Type is not safelisted (STOW uploads). Answer 204 for
            // ANY path - the probe targets the server, not the resource.
            // `build_head` appends Access-Control-Allow-Origin itself.
            let ka = !head.connection_close;
            clarus_core::http::response::build_head(
                &mut conn.header,
                204,
                "No Content",
                None,
                Some(0),
                ka,
                &[
                    (
                        b"Access-Control-Allow-Methods",
                        b"GET, POST, PUT, DELETE, OPTIONS",
                    ),
                    (
                        b"Access-Control-Allow-Headers",
                        b"Content-Type, Accept, Authorization",
                    ),
                    (b"Access-Control-Max-Age", b"86400"),
                ],
            );
            let _ = conn.write_head();
            ka
        }
        RouterOutcome::NotFound => {
            let _ = conn.send_error(404, "Not Found", false);
            false
        }
        RouterOutcome::MethodNotAllowed { allow } => {
            let allow_bytes = allow.as_bytes();
            clarus_core::http::response::build_head(
                &mut conn.header,
                405,
                "Method Not Allowed",
                None,
                Some(0),
                false,
                &[(b"Allow", allow_bytes)],
            );
            let _ = conn.write_head();
            false
        }
        RouterOutcome::BadUid => {
            let _ = conn.send_error(400, "Bad Request", false);
            false
        }
    }
}

/// Classify a request into a metrics [`RouteId`] bucket from method + path.
fn classify(method: clarus_core::http::parse::Method, path: &[u8]) -> RouteId {
    let m = method_name(method);
    let p = std::str::from_utf8(path).unwrap_or("");
    crate::route_classify::classify_route(m, p)
}

fn method_name(m: clarus_core::http::parse::Method) -> &'static str {
    match m {
        clarus_core::http::parse::Method::Get => "GET",
        clarus_core::http::parse::Method::Post => "POST",
        clarus_core::http::parse::Method::Put => "PUT",
        clarus_core::http::parse::Method::Delete => "DELETE",
        clarus_core::http::parse::Method::Options => "OPTIONS",
        clarus_core::http::parse::Method::Head => "HEAD",
        _ => "OTHER",
    }
}

/// Short route bucket name for the access-log `rt=` field (matches the
/// [`RouteId`] taxonomy used by metrics).
fn route_id_str(route: RouteId) -> &'static str {
    match route {
        RouteId::Health => "health",
        RouteId::Metrics => "metrics",
        RouteId::QidoPatients => "qido-patients",
        RouteId::QidoStudies => "qido-studies",
        RouteId::WadoStudy => "wado-study",
        RouteId::StowStudies => "stow",
        RouteId::Admin => "admin",
        RouteId::Commitment => "commitment",
        RouteId::Ups => "ups",
        RouteId::Other => "other",
    }
}

/// Build the nginx-style access-log line (the `message` after the `[INFO]
/// ACCESS` prefix). `rt` is already a route bucket name.
///
/// Every string field passes through `sanitize_ctl`: `path`, `qs`, `peer`,
/// `ip` and `user` can all carry attacker-chosen bytes, and a control byte
/// that survives into the journal is a terminal-injection or log-forging
/// primitive for whoever reads the file next. The masked query string is
/// sanitized as well - masking replaces name VALUES but leaves the rest of
/// the query, including any control bytes, in place.
///
/// Ten fields by design (the line is the contract); the count is inherent to
/// the journal format, not an accident of how it is built.
#[allow(clippy::too_many_arguments)]
fn access_line(
    status: u16,
    method: &str,
    path: &str,
    qs: &str,
    rt: &str,
    peer: &str,
    ip: &str,
    user: &str,
    bytes: u64,
    us: u128,
) -> String {
    // PHI name values in the query string are masked by the shared journal
    // helper (docs/audit-logging.md s2: names out, identifiers visible).
    let qs = crate::access_log::sanitize_ctl(&crate::access_log::mask_query_phi(qs));
    let path = crate::access_log::sanitize_ctl(path);
    let peer = crate::access_log::sanitize_ctl(peer);
    let ip = crate::access_log::sanitize_ctl(ip);
    let user = crate::access_log::sanitize_ctl(user);
    format!(
        "status={status} method={method} path={path} qs={qs} rt={rt} \
         peer={peer} ip={ip} user={user} bytes={bytes} us={us}"
    )
}

/// Whether a completed request took long enough to warn about.
///
/// A separate function, taking the budget as an argument, so the rule is
/// testable without sleeping: the production caller passes
/// [`SLOW_REQUEST`], and a test passes a zero budget to exercise the same
/// comparison without spending ten seconds on it.
fn is_slow(elapsed: Duration, budget: Duration) -> bool {
    elapsed > budget
}

/// Whether a route is a push stream, which is long-lived BY DESIGN.
///
/// Three routes hold a connection open until an event arrives or the client
/// disconnects; none of them has a response to be slow about:
///
///   * `ups_events` - the UPS-RS event stream (SSE). It stays open until the
///     workitem reaches a terminal state and writes a `: keep-alive` comment
///     every 15 s of idle (dispatch_streams.rs, `h_ups_events`);
///   * `ups_notification_connect` - the UPS WebSocket notification connection
///     (PS3.18 8.10.4), open for the life of the subscription;
///   * `admin_logs_stream` - the admin log SSE, which follows the log until
///     the operator stops reading.
///
/// EXCLUDED FROM THE SLOW-REQUEST WARN, because `elapsed > 10 s` is the
/// NORMAL state of all three: the watchdog's premise ("every line is a real
/// candidate for investigation", on [`SLOW_REQUEST`]) holds for a
/// request/response handler, which has no business taking ten seconds, and is
/// simply false for a stream that is supposed to still be open. Warning on
/// them produces a false positive on every long-lived subscriber and teaches
/// the operator to ignore the line.
///
/// WADO IS DELIBERATELY NOT IN THIS LIST. `wado_series`,
/// `wado_series_metadata`, `wado_series_thumbnail`, `wado_frame` and
/// `wado_bulkdata` also stream, but they are FINITE: they write their frames
/// and finish. A WADO request past ten seconds is a real finding - a large
/// series over a slow link - and its WARN must stay, or the one case the
/// watchdog was written for would go unreported.
fn is_push_stream(route_id: &str) -> bool {
    matches!(
        route_id,
        "ups_events" | "ups_notification_connect" | "admin_logs_stream"
    )
}

/// The `slow request` WARN line for a request that outlived [`SLOW_REQUEST`].
///
/// A slice is named by its three UIDs and every other route by its path,
/// because those are the fields that identify the request in each family.
/// The path and the user are attacker-influenced and pass through
/// `sanitize_ctl` for the same reason the ACCESS line does: a control byte
/// that survives into the journal is a terminal-injection primitive for
/// whoever reads the file next.
///
/// The argument count is inherent to naming a request honestly (status,
/// route, identity, both route families' fields, the timing) - the same
/// reason [`access_line`] carries its own allowance.
#[allow(clippy::too_many_arguments)]
fn slow_request_line(
    route_id: &str,
    rt: &str,
    status: u16,
    uids: Option<(&str, &str, &str)>,
    path: &str,
    user: &str,
    ip: &str,
    elapsed: Duration,
) -> String {
    let user = crate::access_log::sanitize_ctl(user);
    let ip = crate::access_log::sanitize_ctl(ip);
    match uids {
        Some((study, series, sop)) if route_id == "wado_instance" => format!(
            "slow request rt={rt} status={status} study={study} series={series} sop={sop} \
             user={user} ip={ip} elapsed_ms={}",
            elapsed.as_millis()
        ),
        _ => format!(
            "slow request rt={rt} status={status} path={} user={user} ip={ip} elapsed_ms={}",
            crate::access_log::sanitize_ctl(path),
            elapsed.as_millis()
        ),
    }
}

/// Read the status code from the response head the handler just built
/// (`Conn.header` begins with `HTTP/1.1 {code}`). Falls back to 200.
fn status_from_head(header: &[u8]) -> u16 {
    // "HTTP/1.1 " is 9 bytes; the status is the following 3 digits.
    let rest = header.get(9..12).unwrap_or(&[]);
    std::str::from_utf8(rest)
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(200)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clarus_core::http::auth::Identity;
    use clarus_core::http::parse::Method;

    fn sock(ip: &str) -> std::net::SocketAddr {
        format!("{ip}:12345").parse().unwrap()
    }

    fn loopback_nets() -> Vec<crate::config::CidrNet> {
        vec![crate::config::parse_cidr("127.0.0.0/8").unwrap()]
    }

    #[test]
    fn trusted_loopback_peer_matches_net() {
        assert!(peer_is_trusted(&sock("127.0.0.1"), &loopback_nets()));
        assert!(!peer_is_trusted(&sock("10.0.0.5"), &loopback_nets()));
    }

    #[test]
    fn localhost_only_gate_uses_resolved_identity() {
        // Proxied client behind nginx on a remote IP: not loopback -> 403.
        let proxied = Identity::Proxied {
            ip: b"10.0.0.9".to_vec(),
            user: b"dr-smith".to_vec(),
        };
        assert!(!resolved_is_loopback(&proxied, &sock("127.0.0.1")));
        // Local viewer (trusted peer, no headers): peer decides -> allowed.
        assert!(resolved_is_loopback(
            &Identity::LocalViewer,
            &sock("127.0.0.1")
        ));
        // Untrusted remote peer: its own address decides -> denied.
        let untrusted = Identity::UntrustedPeer {
            ip: b"203.0.113.9".to_vec(),
        };
        assert!(!resolved_is_loopback(&untrusted, &sock("203.0.113.9")));
    }

    #[test]
    fn proxied_loopback_client_passes_gate() {
        let proxied = Identity::Proxied {
            ip: b"127.0.0.1".to_vec(),
            user: b"viewer".to_vec(),
        };
        assert!(resolved_is_loopback(&proxied, &sock("127.0.0.1")));
    }

    // -- Admin surface gate ------------------------------------------------

    #[test]
    fn admin_gate_default_is_local_machine_only() {
        // No admin_cidrs (the default): loopback yes, anything else no.
        let none: Vec<crate::config::CidrNet> = Vec::new();
        assert!(admin_allowed(
            &Identity::LocalViewer,
            &sock("127.0.0.1"),
            &none
        ));
        let direct_lan = Identity::UntrustedPeer {
            ip: b"192.168.1.50".to_vec(),
        };
        assert!(!admin_allowed(&direct_lan, &sock("192.168.1.50"), &none));
        // A proxied management client is not loopback either.
        let proxied = Identity::Proxied {
            ip: b"192.168.1.50".to_vec(),
            user: b"station".to_vec(),
        };
        assert!(!admin_allowed(&proxied, &sock("127.0.0.1"), &none));
    }

    #[test]
    fn admin_gate_allows_configured_network_direct_and_proxied() {
        let nets = vec![crate::config::parse_cidr("192.168.50.0/24").unwrap()];
        // Direct client: the socket peer is the resolved IP.
        let direct = Identity::UntrustedPeer {
            ip: b"192.168.50.7".to_vec(),
        };
        assert!(admin_allowed(&direct, &sock("192.168.50.7"), &nets));
        // Proxied client behind nginx: the socket peer is nginx on loopback and
        // must NOT decide - the resolved address in the identity does.
        let proxied = Identity::Proxied {
            ip: b"192.168.50.7".to_vec(),
            user: b"station".to_vec(),
        };
        assert!(admin_allowed(&proxied, &sock("127.0.0.1"), &nets));
        // Outside the configured network -> refused.
        let outside = Identity::Proxied {
            ip: b"192.168.51.7".to_vec(),
            user: b"station".to_vec(),
        };
        assert!(!admin_allowed(&outside, &sock("127.0.0.1"), &nets));
        // Loopback stays allowed regardless of the configured networks.
        assert!(admin_allowed(
            &Identity::LocalViewer,
            &sock("127.0.0.1"),
            &nets
        ));
    }

    #[test]
    fn admin_gate_denies_missing_identity_without_resolved_ip() {
        // `MissingIdentity` is a TRUSTED proxy that shipped identity headers
        // but no authenticated user, so there is no resolved address at all
        // (`effective_ip` is empty). An empty address is inside no network.
        let nets = vec![crate::config::parse_cidr("192.168.50.0/24").unwrap()];
        assert!(!resolved_ip_in_cidrs(&Identity::MissingIdentity, &nets));
        // Behind a proxying host that is NOT itself loopback, the request must
        // be refused: there is no evidence the client is on the admin network.
        assert!(!admin_allowed(
            &Identity::MissingIdentity,
            &sock("10.0.0.5"),
            &nets
        ));
        // The pre-existing rule is unchanged, though: `admin_cidrs` is an
        // ADDITION to localhost, so the local machine stays allowed and the
        // default configuration keeps behaving exactly as before.
        assert!(admin_allowed(
            &Identity::MissingIdentity,
            &sock("127.0.0.1"),
            &nets
        ));
    }

    #[test]
    fn admin_gate_covers_v6_networks() {
        let nets = vec![crate::config::parse_cidr("fd00:1234::/32").unwrap()];
        let v6 = Identity::UntrustedPeer {
            ip: b"fd00:1234::5".to_vec(),
        };
        assert!(admin_allowed(&v6, &sock("127.0.0.1"), &nets));
        // An IPv4 address never matches a v6 network.
        let v4 = Identity::UntrustedPeer {
            ip: b"10.0.0.5".to_vec(),
        };
        assert!(!admin_allowed(&v4, &sock("10.0.0.5"), &nets));
    }

    #[test]
    fn first_xff_takes_leftmost_hop() {
        assert_eq!(first_xff(b"10.0.0.9, 192.0.2.1"), b"10.0.0.9");
        assert_eq!(first_xff(b"10.0.0.9"), b"10.0.0.9");
    }

    #[test]
    fn local_user_label_distinguishes_viewer_from_loopback() {
        assert_eq!(local_user_label(Some(b"definitum semel")), b"(miniviewer)");
        assert_eq!(local_user_label(Some(b"something else")), b"(loopback)");
        assert_eq!(local_user_label(None), b"(loopback)");
    }

    #[test]
    fn classify_maps_methods() {
        assert_eq!(
            classify(Method::Get, b"/health"),
            classify(Method::Get, b"/health")
        );
        assert_eq!(method_name(Method::Post), "POST");
        assert_eq!(method_name(Method::Unknown), "OTHER");
    }

    #[test]
    fn access_line_renders_nginx_kv_fields() {
        let line = access_line(
            200,
            "GET",
            "/dicomweb/studies/1.2.3",
            "includefield=00100010",
            "wado-study",
            "127.0.0.1:51234",
            "10.0.0.9",
            "dr-smith",
            4096,
            1234,
        );
        assert_eq!(
            line,
            "status=200 method=GET path=/dicomweb/studies/1.2.3 qs=includefield=00100010 \
             rt=wado-study peer=127.0.0.1:51234 ip=10.0.0.9 user=dr-smith bytes=4096 us=1234"
        );
    }

    #[test]
    fn access_line_sanitizes_control_bytes_in_every_string_field() {
        // The request target is attacker-controlled and reaches the log
        // verbatim. Every string field is covered: ESC in the path (terminal
        // injection for whoever tails the log), NUL in the query, TAB and DEL
        // in the peer/ip/user fields.
        let line = access_line(
            200,
            "GET",
            "/dicomweb/\u{1b}[31mstudies",
            "limit=2\0x",
            "stow",
            "127.0.0.1:5\u{7f}1",
            "10.0.0.\t9",
            "dr\nsmith",
            1,
            2,
        );
        assert!(
            !line.contains('\u{1b}')
                && !line.contains('\0')
                && !line.contains('\t')
                && !line.contains('\u{7f}')
                && !line.contains('\n'),
            "no control byte may survive into the journal line: {line:?}"
        );
        assert_eq!(
            line,
            "status=200 method=GET path=/dicomweb/?[31mstudies qs=limit=2?x \
             rt=stow peer=127.0.0.1:5?1 ip=10.0.0.?9 user=dr?smith bytes=1 us=2"
        );
    }

    #[test]
    fn access_line_still_masks_phi_after_sanitizing() {
        // The two transformations compose: masking replaces a name VALUE with
        // `***`, sanitizing removes control bytes, and neither undoes the
        // other. A control byte in a name value must not be able to escape
        // the mask.
        let line = access_line(
            200,
            "GET",
            "/dicomweb/studies",
            "PatientName=Salgalov^\u{1b}[0m&limit=200",
            "qido-studies",
            "127.0.0.1:1",
            "127.0.0.1",
            "(loopback)",
            1,
            2,
        );
        assert!(
            line.contains("PatientName=***"),
            "name value must be masked"
        );
        assert!(
            !line.contains('\u{1b}'),
            "no control byte may survive: {line:?}"
        );
    }

    #[test]
    fn phi_name_values_in_qs_are_masked() {
        let m = crate::access_log::mask_query_phi;
        // Tag form + Cyrillic value: always the constant mask, key intact.
        assert_eq!(
            m("00100010=%D0%9C%D0%B0%D1%81%D1%82%D1%8F%D0%B5%D0%B2%D0%B0"),
            "00100010=***"
        );
        // Keyword form (the Weasis QIDO style) - was UNMASKED before 29.08.
        assert_eq!(
            m("PatientName=%D1%81%D0%B0%D0%BB%D0%B3%D0%B0&limit=200"),
            "PatientName=***&limit=200"
        );
        // Case-insensitive keyword + physician names.
        assert_eq!(m("patientname=Salgalov^D"), "patientname=***");
        assert_eq!(
            m("ReferringPhysicianName=Doc"),
            "ReferringPhysicianName=***"
        );
        // Identifiers stay VISIBLE (s3: the auditor sees whose data).
        assert_eq!(
            m("PatientID=7777&00080050=ACC123"),
            "PatientID=7777&00080050=ACC123"
        );
        // Any length, any script, incl. a one-char Korean name: constant ***.
        assert_eq!(m("PatientName="), "PatientName=***");
        assert_eq!(m("PatientName=5"), "PatientName=***");
        assert_eq!(m("PatientName=ab"), "PatientName=***");
        assert_eq!(m("PatientName=%EC%98%A4"), "PatientName=***");
    }

    #[test]
    fn non_phi_query_segments_pass_through() {
        // includefield mentions the tag but is not a PHI value - untouched.
        assert_eq!(
            crate::access_log::mask_query_phi("includefield=00100010&00080061=CT,DX&limit=25"),
            "includefield=00100010&00080061=CT,DX&limit=25"
        );
        assert_eq!(crate::access_log::mask_query_phi(""), "");
        // Segments without '=' are kept as-is.
        assert_eq!(crate::access_log::mask_query_phi("foo"), "foo");
        // Key matching is case-insensitive; value is the constant mask.
        assert_eq!(
            crate::access_log::mask_query_phi("00100010=Jo"),
            "00100010=***"
        );
        // PatientID stays visible; malformed escapes untouched.
        assert_eq!(
            crate::access_log::mask_query_phi("00100020=%ZZ777"),
            "00100020=%ZZ777"
        );
    }

    #[test]
    fn access_line_masks_phi_inline() {
        let line = access_line(
            200,
            "GET",
            "/dicomweb/studies",
            "00100010=%D0%9C%D0%B0%D1%81%D1%82%D1%8F%D0%B5%D0%B2%D0%B0&00080061=CT",
            "qido-studies",
            "127.0.0.1:51234",
            "10.0.0.9",
            "dr-smith",
            1024,
            5,
        );
        assert_eq!(
            line,
            "status=200 method=GET path=/dicomweb/studies \
             qs=00100010=***&00080061=CT rt=qido-studies \
             peer=127.0.0.1:51234 ip=10.0.0.9 user=dr-smith bytes=1024 us=5"
        );
    }

    #[test]
    fn route_id_str_covers_all_buckets() {
        assert_eq!(route_id_str(RouteId::QidoStudies), "qido-studies");
        assert_eq!(route_id_str(RouteId::StowStudies), "stow");
        assert_eq!(route_id_str(RouteId::Admin), "admin");
        assert_eq!(route_id_str(RouteId::Other), "other");
    }

    #[test]
    fn a_request_is_slow_only_past_the_budget() {
        // The comparison is strict: a request exactly at the budget is not
        // over it. The production budget (SLOW_REQUEST) is far above any
        // healthy handler, so this pins the rule, not the number.
        let budget = Duration::from_secs(10);
        assert!(!is_slow(Duration::from_secs(9), budget));
        assert!(!is_slow(budget, budget));
        assert!(is_slow(Duration::from_millis(10_001), budget));
    }

    #[test]
    fn only_the_three_push_routes_are_exempt() {
        assert!(is_push_stream("ups_events"));
        assert!(is_push_stream("ups_notification_connect"));
        assert!(is_push_stream("admin_logs_stream"));
        // Exactly three, not more: any other route keeps its WARN.
        for other in [
            "qido_studies",
            "stow_studies",
            "wado_instance",
            "admin",
            "other",
            "",
        ] {
            assert!(!is_push_stream(other), "{other} must not be exempt");
        }
    }

    #[test]
    fn a_slow_wado_route_is_not_exempt() {
        // WADO streams are FINITE (they write their frames and finish), so a
        // slow one is the case the watchdog exists for. Exempting them would
        // silence the one real finding.
        for route in [
            "wado_series",
            "wado_series_metadata",
            "wado_study_metadata",
            "wado_series_thumbnail",
            "wado_frame",
            "wado_bulkdata",
        ] {
            assert!(
                !is_push_stream(route),
                "{route} is finite and must keep its slow-request WARN"
            );
            // And the gate as a whole still fires for it.
            assert!(is_slow(Duration::from_millis(10_001), SLOW_REQUEST));
        }
    }

    #[test]
    fn the_gate_exempts_a_push_stream_but_still_warns_elsewhere() {
        // The exact expression at the call site, with a zero budget so no
        // test spends ten seconds proving a comparison.
        let budget = Duration::ZERO;
        let slow = Duration::from_millis(1);

        let warns = |route_id: &str| !is_push_stream(route_id) && is_slow(slow, budget);

        assert!(!warns("ups_events"), "a long-lived SSE must not warn");
        assert!(
            !warns("ups_notification_connect"),
            "a WebSocket must not warn"
        );
        assert!(!warns("admin_logs_stream"), "the log stream must not warn");
        assert!(warns("wado_series"), "a slow WADO series must still warn");
        assert!(warns("qido_studies"), "a slow query must still warn");
    }

    #[test]
    fn a_slow_slice_is_named_by_its_uids() {
        let line = slow_request_line(
            "wado_instance",
            "wado-study",
            200,
            Some(("1.2.3", "1.2.4", "1.2.5")),
            "/dicomweb/studies/1.2.3/series/1.2.4/instances/1.2.5",
            "clinfer",
            "127.0.0.1",
            Duration::from_millis(30_500),
        );
        assert!(line.contains("study=1.2.3"), "{line}");
        assert!(line.contains("series=1.2.4"), "{line}");
        assert!(line.contains("sop=1.2.5"), "{line}");
        assert!(line.contains("rt=wado-study"), "{line}");
        assert!(line.contains("elapsed_ms=30500"), "{line}");
        assert!(!line.contains("path="), "a slice is named by UID: {line}");
    }

    #[test]
    fn a_slow_request_on_another_route_is_named_by_its_path() {
        let line = slow_request_line(
            "qido_studies",
            "qido-studies",
            200,
            None,
            "/dicomweb/studies?PatientName=Smith",
            "anonymous",
            "10.0.0.5",
            Duration::from_millis(12_000),
        );
        assert!(line.contains("path=/dicomweb/studies"), "{line}");
        assert!(line.contains("rt=qido-studies"), "{line}");
        assert!(line.contains("elapsed_ms=12000"), "{line}");
        assert!(!line.contains("sop="), "{line}");
    }

    #[test]
    fn a_slow_request_line_cannot_forge_a_log_entry() {
        // A path or a user carrying a newline is the log-forging primitive
        // the ACCESS line already sanitizes; the WARN line must too.
        let line = slow_request_line(
            "qido_studies",
            "qido-studies",
            200,
            None,
            "/dicomweb/studies\n[ERROR] forged",
            "user\n[ERROR] forged",
            "127.0.0.1",
            Duration::from_secs(11),
        );
        assert!(!line.contains('\n'), "no raw newline survives: {line:?}");
    }
}
