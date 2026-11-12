// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Global `reqwest::Client` pool for S3 HTTP requests.
//!
//! One `Client` per process - connection reuse eliminates TCP handshake
//! overhead and acts as a DDoS self-protection mechanism via
//! `pool_max_idle_per_host`. nginx-way: KISS, no abstractions.

use reqwest::blocking::Client;
use std::sync::OnceLock;
use std::time::Duration;

static S3_HTTP: OnceLock<Client> = OnceLock::new();

/// Returns a reference to the process-global S3 HTTP client.
///
/// Configured with:
/// - `pool_max_idle_per_host(20)` - DDoS self-protection
/// - `pool_idle_timeout(90s)` - release idle connections
/// - `tcp_keepalive(60s)` - detect dead connections
/// - `connect_timeout(10s)` - fail fast on an unreachable endpoint
///
/// NO total `timeout` here: reqwest's `timeout` covers the whole request
/// including the body, so a fixed 30 s killed every large object on a slow
/// link and the caller saw a false "Connection error" for what was actually a
/// healthy transfer. The timeout is applied per request instead, wide enough
/// for a full object at a slow-but-live link speed (`S3_XFER_TIMEOUT`).
pub(crate) fn s3_client() -> &'static Client {
    S3_HTTP.get_or_init(|| {
        Client::builder()
            .pool_max_idle_per_host(20)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("S3 HTTP client init")
    })
}

/// Per-request ceiling for a whole object transfer.
///
/// The ceiling is set by the largest object the CAS accepts, not by a typical
/// one: `max_instance_size_mb` defaults to 500, and at 10 Mbit/s that object
/// needs about 400 s, so 30 minutes leaves a factor of four for a slower link
/// before the transfer is cut. A 1 Mbit/s link moves about 225 MB in the same
/// window - below the default ceiling, which is why the ceiling and the
/// instance cap have to be reasoned about together rather than separately. A
/// connection that stops making progress at all is caught by `tcp_keepalive`,
/// not by this total.
pub(crate) const S3_XFER_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Per-request ceiling for metadata operations (HEAD / LIST / DELETE).
///
/// These never transfer object bytes, so a healthy round-trip is bounded by
/// `connect_timeout` (10 s) plus server latency. 30 s leaves a factor of three
/// over that bound before a hung metadata call fails - a stuck HEAD/LIST/
/// DELETE must not hold a scrub/audit walk open for the full 30-minute
/// transfer ceiling while transferring nothing.
pub(crate) const S3_META_TIMEOUT: Duration = Duration::from_secs(30);
