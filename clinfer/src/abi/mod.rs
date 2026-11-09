// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The `clinfer-abi` batch contract: one study in, one `result.json` out.
//!
//! clinfer is a DICOMweb processing runtime. A *processor* is an ordinary
//! program the site configures; clinfer hands it a directory containing the
//! study's raw pixel frames and a manifest describing them, then reads the
//! single `result.json` it writes back. DICOM correctness stays here - the
//! processor never sees a DICOM object, so it cannot corrupt one.
//!
//! The contract has three parts, and all three live in this module tree:
//!
//! - [`manifest`] - what clinfer WRITES: the workdir layout and its
//!   `manifest.json`.
//! - [`workdir`] - where it writes them: one directory per run, built so
//!   the same run can be repeated by hand.
//! - [`schema`] - what clinfer READS: the strict shape of `result.json`.
//! - [`external`] - how the program is RUN: the one spawn seam, with its
//!   supervision, timeout and process-tree kill.
//!
//! `abi/README.md` in the crate root is the prose form of the same contract,
//! written for the person who writes a processor. It is the reference; this
//! module is the implementation, and the contract test compares them.
//!
//! # Why a directory and not a stream
//!
//! Studies are large (a tomo series is hundreds of slices), the data has to
//! be seekable and re-runnable, and an operator has to be able to run a
//! processor by hand during forensics. Files give all three; a stream would
//! need a framing protocol for pixel bytes and would throw away the ability
//! to re-run the exact same input. The resident mode contemplated for later
//! adds a CONTROL channel over stdio and leaves the data in the workdir, so
//! the data form does not change.

pub mod external;
pub mod manifest;
pub mod schema;
pub mod workdir;

pub use external::{ExternalError, Outcome, Processor, RunReport};
pub use manifest::{FrameEntry, Manifest, StudyContext};
pub use schema::{Derived, PrBox, PrResult, Producer, ResultFile, SrResult};
pub use workdir::{Frame, Workdir, WorkdirError};
