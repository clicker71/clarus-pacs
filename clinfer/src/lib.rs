// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! clinfer - DICOMweb inference sidecar.
//!
//! The sidecar is a plain DICOMweb client: it claims UPS workitems from an
//! origin server, runs a model, and stores the result back as an ordinary
//! DICOM object.
//!
//! Layers, bottom up, with no dependency pointing back down: `json` and
//! `http` are wire formats; `dicomweb` names the operations in the
//! standard's vocabulary; `ws` and then `notify` are the notification
//! channel and its loop; `worker` is the state machine and the thread
//! that runs it; `receiver` is the inbound listener. `conf` and `matcher`
//! are what a rule decides.

#![deny(unsafe_code)]

pub mod abi;
pub mod conf;
pub mod derived;
pub mod dicomweb;
pub mod http;
pub mod inspect;
pub mod json;
pub mod matcher;
pub mod measurements_writer;
pub mod notify;
pub mod pidfile;
pub mod pixels;
pub mod pr;
pub mod procstat;
pub mod receiver;
pub mod reload;
pub mod rtstruct;
pub mod seg;
pub mod sr;
pub mod stl;
pub mod ups;
pub mod value;
pub mod worker;
pub mod ws;
pub mod yaml;

/// Build identity of the binary, for `/version` and log headers.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
