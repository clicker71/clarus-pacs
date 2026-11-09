// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Embedded mini-viewer HTML.
//!
//! `viewer_html()` is the pure, sync core of the `/` route -
//! it resolves the `{VERSION}` placeholder once and serves the string forever.
//! It is axum-free (`&'static str` in, `&'static str` out) and is called by
//! both the thin `serve_viewer` wrapper (in `routes.rs`) and, later, the stage
//! 8 dispatcher.

#![forbid(unsafe_code)]

use std::sync::OnceLock;

/// Return the embedded viewer HTML with the `{VERSION}` placeholder replaced
/// by the full build identification the `/version` route reports, so the
/// viewer banner names the same binary the operator probes.
///
/// The substitution runs exactly once (lazily, on first call) into a
/// [`OnceLock`]; subsequent calls return the cached `&'static str`. This
/// produces the same bytes as the previous per-request `String::replace`
/// while allocating zero on the hot path.
pub fn viewer_html() -> &'static str {
    static HTML: OnceLock<String> = OnceLock::new();
    HTML.get_or_init(|| {
        include_str!("../../assets/clarus-viewer.html")
            .replace("{VERSION}", &crate::version::version_line())
    })
}
