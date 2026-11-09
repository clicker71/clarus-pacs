// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Clarus core - dependency-free foundation for the no-SQL storage tier.

// unsafe lives ONLY in `crate::ffi` (audited OS boundary,
// `#![allow(unsafe_code)]` inside; see docs/unsafe-audit.md). The rest of
// the crate is pure safe Rust - new `unsafe` outside ffi.rs is a compile
// error. NOTE: `deny`, not `forbid`, at the root: `forbid` cannot be
// overridden by the `allow` in ffi.rs (Rust E0453), `deny` can. The only
// `allow(unsafe_code)` override in the crate is ffi.rs (grep-gated).
#![deny(unsafe_code)]

//!
//! This crate holds the primitives that every other layer builds on:
//! a zero-allocation CBOR codec and the CBOR-backed study manifest format.
//!
//! # Contents
//!
//! - [`cbor`] - streaming CBOR token parser (0 alloc) and fixed-buffer encoder (0 alloc).
//! - [`manifest`] - the per-study CBOR manifest: one file per study holding the
//!   full DICOM hierarchy (system header, study QIDO keys, series, instances,
//!   and CAS frame chunking), written atomically and read via mmap.
//!
//! # Design invariants
//!
//! - **No DICOM pixel data in CBOR.** Manifests store only textual/numeric
//!   metadata and blob hashes. Pixel bytes, audio, and embedded PDFs stay in
//!   the content-addressed `.dcm` files (see `clarus-adapters::storage`).
//! - **Numerical CBOR keys.** System keys use the 1-6 range; DICOM tags are the
//!   full 32-bit tag values. Keys are written in ascending order for binary
//!   search on read.
//! - **0 alloc in the codec hotpath.** The parser borrows from the input slice;
//!   the encoder writes into a caller-provided buffer. Neither allocates.

// unsafe lives ONLY in `crate::ffi` (audited OS boundary; see
// docs/unsafe-audit.md): localtime, setsockopt, sendfile/TransmitFile and
// the read-only mmap helper all live there behind safe facades.
pub mod auth;
pub mod cbor;
pub mod commitment;
pub mod ffi;
pub mod hash;
pub mod http;
pub mod http_status;
pub mod idx;
pub mod journal;
pub mod manifest;
pub mod manifest_cache;
pub mod query;
pub mod recovery;
pub mod shutdown;
pub mod stow;
pub mod time;
pub mod ups;
pub mod workflow;

pub use cbor::{CborError, CborReader, CborToken, CborWriter};
pub use commitment::{CommitmentRecord, CommitmentStore};
pub use http_status::status_class;
pub use manifest::{
    FrameRecord, InstanceRecord, Manifest, ManifestError, SeriesRecord, StudyQido, SysHeader,
};
pub use time::{LocalTime, civil_from_days, local_now};
pub use workflow::{MppsRecord, SpsRecord, WorkflowStore, WorkflowStoreError};
