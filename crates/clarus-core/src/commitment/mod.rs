// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Storage Commitment CBOR file store (PS3.18 Section 13).
//!
//! One transaction per `commitments/{TransactionUID}.cbor`, modelled on the
//! UPS workitem store and using the same zero-allocation custom CBOR codec
//! (`crate::cbor`) - no new crates.
//!
//! - [`record::CommitmentRecord`] - the on-disk record shape + codec.
//! - [`store::CommitmentStore`] - atomic read/write and the retention sweep.
//!
//! The server boundary (`crates/clarus-server`) converts between this core
//! record and the DICOMweb payloads of PS3.18 Annex J.

pub mod record;
pub mod store;

pub use record::{CommitmentOutcome, CommitmentRecord, InstanceResult, RecordError};
pub use store::{CommitmentStore, StoreError};
