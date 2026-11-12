// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Clarus Transcode - DICOM anonymization and transcoding pipeline.
//!
//! This crate provides the infrastructure for receiving DICOM studies,
//! anonymizing them, transcoding pixel data, and forwarding the results
//! to downstream consumers via DIMSE or DICOMweb.
//!
//! # Architecture
//!
//! ```text
//! InputAdapter ---> Pipeline ---> OutputAdapter
//!                   |
//!                   +-- AnonymizeStage
//!                   +-- TranscodeStage
//!                   +-- OutboxStage (retry)
//! ```

#![forbid(unsafe_code)]

pub mod adapters;
pub mod auth;
pub mod config;
pub mod dicom_utils;
pub mod health;
pub mod metrics;
pub mod outbox;
pub mod outbox_worker;
pub mod pipeline;
pub mod pipeline_worker;
pub mod spool;
pub mod stages;

// Re-export key types for convenience.
pub use adapters::{AdapterError, InputAdapter, OutputAdapter};
pub use config::{Config, ConfigError, TransferSyntaxMap, transfer_syntax_names};
pub use pipeline::{NoopStage, Pipeline, PipelineError, PipelineStage};

/// Crate version, matching workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
