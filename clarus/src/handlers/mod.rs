// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! HTTP handler modules for DICOMweb endpoints.

pub mod admin;
pub mod cad_workitem;
pub mod capabilities;
pub mod commitment_pure;
pub mod instance_xml;
pub mod json_common;
pub mod metrics_render;
pub mod mpps_pure;
pub mod mwl_pure;
pub mod qido_pure;
pub mod readiness;
pub mod stow_pure;
pub mod stow_response;
pub mod subscribe_pure;
pub mod ups;
pub mod ups_pure;
pub mod viewer;
pub mod wado;
pub mod wado_pure;
pub mod wado_range;
pub mod websocket;
pub mod workflow_response;

// -- DICOMweb constants (moved from dicomweb_types.rs) -------------------

/// DICOMweb JSON content type per PS3.18 Section 8.7.
/// MUST include charset=utf-8 - otherwise DICOMweb clients (Weasis, OHIF)
/// may default to Latin-1 and misinterpret Cyrillic/UTF-8 characters.
pub const DICOM_JSON_CONTENT_TYPE: &str = "application/dicom+json; charset=utf-8";

/// DICOM XML content type per PS3.18 Section 8.7.
pub const DICOM_XML_CONTENT_TYPE: &str = "application/dicom+xml";
