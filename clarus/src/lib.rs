// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Clarus PACS server library.
//!
//! Re-exports the public API for use by integration tests and external consumers.

// unsafe lives ONLY in `crate::ffi` (audited OS boundary, see
// docs/unsafe-audit.md). New `unsafe` outside ffi.rs is a compile error.
// NOTE: `deny`, not `forbid`, at the root: `forbid` cannot be overridden by
// the `allow` in ffi.rs (Rust E0453), `deny` can. The only
// `allow(unsafe_code)` override in the crate is ffi.rs (grep-gated).
#![deny(unsafe_code)]

pub mod access_agg;
pub mod access_log;
pub mod audit_copies;
pub mod audit_log;
pub mod cas_audit_worker;
pub mod clog;
pub mod commitment_retention;
pub mod config;
pub mod disk_health;
pub mod disk_utils;
pub mod dispatch;
pub mod dispatch_streams;
pub mod erase_worker;
pub mod ffi;
pub mod handlers;
pub mod idx_fixer;
pub mod log_writer;
pub mod metrics;
pub mod migration_worker;
pub mod push;
pub mod rebuild_worker;
pub mod route_classify;
pub mod serve;
pub mod shadow;
pub mod sse_fanout;
pub mod state;
pub mod storage_tiers;
pub mod stow_agg;
pub mod ups_retention;
pub mod validation;
pub mod version;
pub mod workflow_retention;
pub mod ws_channels;
