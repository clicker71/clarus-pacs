// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Build stamp for the server binary.
//!
//! `CARGO_PKG_VERSION` is the same for every build of a release line, so it
//! cannot tell two field builds apart. The git revision and the build date
//! can, and that is what an operator needs when deciding which binary is
//! running on which host.
//!
//! Both values are exported as compile-time environment variables, read by
//! `env!` in the crate. Git being unavailable (a source tarball, an air-gapped
//! build host without the repository) yields `"unknown"` - it must never fail
//! the build.

use std::process::Command;

fn main() {
    // Re-run when the commit changes so an incremental build cannot keep a
    // stale stamp.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs");

    println!("cargo:rustc-env=GIT_REV={}", git_rev());
    println!("cargo:rustc-env=COMMIT_DATE={}", commit_date());
}

/// Short git revision, or `"unknown"` when git cannot answer.
///
/// `git rev-parse --short HEAD` fails on a tarball, on a missing binary, and
/// inside a repository whose HEAD is unborn - all of which are legitimate
/// build environments, so a failure is a value, not an error.
fn git_rev() -> String {
    git(&["rev-parse", "--short", "HEAD"])
}

/// The COMMIT date (`YYYY-MM-DD`) of HEAD, or `"unknown"` when git cannot
/// answer.
///
/// Deliberately the commit date and not the build date: the stamp has to
/// describe the state of the CODE. A build date makes it non-deterministic -
/// the same revision rebuilt tomorrow would report a different stamp, and an
/// old revision rebuilt today would look fresh. `rev + commit date` is a
/// stable, unique description of one code state, which is what a field
/// operator asking "which build is this?" actually needs.
fn commit_date() -> String {
    git(&["log", "-1", "--format=%cs"])
}

/// Run a git subcommand and return its trimmed stdout, or `"unknown"`.
///
/// Never fails the build: a missing git binary, a source tarball and an unborn
/// HEAD are all legitimate, so a failure is a value rather than an error.
fn git(args: &[&str]) -> String {
    match Command::new("git").args(args).output() {
        Ok(o) if o.status.success() => {
            let value = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if value.is_empty() {
                "unknown".to_string()
            } else {
                value
            }
        }
        _ => "unknown".to_string(),
    }
}
