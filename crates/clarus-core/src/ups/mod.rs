// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! UPS workitem CBOR file store (C-014).
//!
//! The UPS-RS worklist moves from SQLite to a per-workitem CBOR file at
//! `workitems/{uid}.cbor`, modelled on the study manifest and using the same
//! zero-allocation custom CBOR codec (`crate::cbor`) - no new crates.
//!
//! - [`record::WorkItemRecord`] - the on-disk record shape + codec.
//! - [`store::WorkItemStore`] - atomic read/write, Transaction-UID
//!   compare-and-swap, and the linear-search filter/sort contract mirroring
//!   the SQLite oracle.
//!
//! The server boundary (`crates/clarus-server`) converts between this core
//! record and `clarus_ports::models::WorkItem`.

pub mod record;
pub mod store;

pub use record::{OutputReference, WorkItemCode, WorkItemError, WorkItemRecord};
pub use store::{StoreError, StoreFilter, WorkItemStore};
