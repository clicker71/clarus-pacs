// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DICOM streaming parser adapter.
//!
//! Implements `clarus_ports::dicom::DicomParser` via a manual,
//! zero-dependency, element-by-element DICOM Part 10 parser.
//!
//! # Why manual instead of `dicom-rs`
//! The `dicom` meta-crate pulls `dicom-transfer-syntax-registry` ->
//! `jpeg2k` -> `openjp2`, which fails to link on Windows MSVC
//! (`posix_memalign` not available). A pure-Rust manual parser
//! avoids this while providing true streaming reads.

#![forbid(unsafe_code)]

pub mod frame;
mod parser;
pub mod tags;

pub use parser::{DicomRsParser, PnGroups, split_pn_groups};
