// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! WADO-RS handlers - retrieve DICOM instances.
//!
//! WADO = Web Access to DICOM Objects (DICOMweb PS3.18 Section 10).
//!
//! # Endpoints
//! - `GET /dicomweb/studies/{study}` -> multipart/related
//! - `GET /dicomweb/studies/{study}/series/{series}` -> multipart/related
//! - `GET /dicomweb/studies/{study}/series/{series}/instances/{uid}` -> binary DICOM
//! - `GET .../metadata` -> DICOMweb JSON
//! - `GET .../bulkdata/{tag}` -> raw binary tag value
//! - `GET /wadouri?requestType=WADO&...` -> legacy WADO-URI

#![forbid(unsafe_code)]

pub mod error;
pub mod helpers;
pub mod types;
mod validation;

pub use error::WadoError;
pub use types::{WadoQueryParams, WadoUriQuery};
pub use validation::{validate_dicom_uid, validate_wado_params};
