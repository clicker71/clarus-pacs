// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DICOM streaming parser adapter.
//!
//! Implements `clarus_ports::dicom::DicomParser` via a manual,
//! element-by-element DICOM Part 10 parser that does not use the `dicom`
//! meta-crate. (The crate is not dependency-free: the parser itself uses
//! `log`, `zmij` for float formatting, and feature-gated `encoding_rs` for
//! the CJK character sets - "manual" here means the Part 10 element walk and
//! the VR table are ours, not that nothing else is linked.)
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
