// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Modality Worklist (MWL) and Modality Performed Procedure Step (MPPS) CBOR
//! store (`workflow/mwl/{uid}.cbor`, `workflow/mpps/{uid}.cbor`).
//!
//! These are PS3.18 §14 and §15 records: MWL is the C-FIND analog (a
//! scheduled step, read-only through DICOMweb) and MPPS is the N-CREATE/N-SET/
//! N-GET analog (a performed step, mutable until it reaches a final state).
//!
//! The storage tier is `data/` (the immutable CAS); the workflow records are
//! the one place a mutable short-lived row lives, so they get their own
//! directory and their own GC (retention sweep). The codec and the store
//! discipline are the ones `crate::ups` already uses - one numeric-key CBOR
//! map per file, tmp + rename write, linear scan read - because a scheduler
//! worklist and a UPS worklist are the same shape of problem (PS3.4 K.6 vs
//! PS3.4 CC.2.5).
//!
//! - [`record::SpsRecord`] / [`record::MppsRecord`] - the on-disk shapes.
//! - [`store::WorkflowStore`] - atomic write, per-UID read, linear scan.

pub mod record;
pub mod store;

pub use record::{MppsError, MppsRecord, MppsReference, SpsError, SpsRecord};
pub use store::{WorkflowStore, WorkflowStoreError};
