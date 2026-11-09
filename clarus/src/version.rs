// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Public build identification (`GET /version`).
//!
//! One question this answers, and only this one: which binary is running?
//! `CARGO_PKG_VERSION` alone cannot - every build of a release line shares it -
//! so the git revision and the build date are stamped in at compile time by
//! `build.rs` and reported here.
//!
//! The renderer is a constant string join, the same weight as `/health`: it is
//! called by operators and probes, never on a DICOMweb hot path.

#![forbid(unsafe_code)]

/// Package version (`CARGO_PKG_VERSION`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short git revision of the built commit, or `"unknown"` when the build host
/// had no git (tarball, air-gapped builder).
pub const GIT_REV: &str = env!("GIT_REV");

/// Commit date `YYYY-MM-DD` of the built revision.
///
/// The COMMIT date, not the build date: `rev + commit date` describes one
/// state of the code, so rebuilding the same revision tomorrow yields the same
/// stamp, and an old revision rebuilt today does not look fresh.
pub const COMMIT_DATE: &str = env!("COMMIT_DATE");

/// Render the `GET /version` body: `clarus <version> (<git_rev>, <commit_date>)`.
pub fn version_line() -> String {
    format!("clarus {VERSION} ({GIT_REV}, {COMMIT_DATE})")
}

/// The `(version, build)` pair reported inside `/health/status`.
///
/// `build` is the same `rev, date` pair the version line carries, so a viewer
/// badge and an operator's curl tell the same story.
pub fn build_pair() -> (&'static str, String) {
    (VERSION, format!("{GIT_REV}, {COMMIT_DATE}"))
}
