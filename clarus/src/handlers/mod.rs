// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! HTTP handler modules for DICOMweb endpoints.

pub mod accept;
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
//
// The media types are the ones PS3.18 Table 8.7-1 names; the facts behind
// them, including why only the JSON one carries a charset, are recorded in
// `standard/ps3.18/media-types-8.7.json`.

/// DICOM JSON content type per PS3.18 Table 8.7-1.
///
/// MUST include `charset=utf-8` - otherwise DICOMweb clients (Weasis, OHIF)
/// may default to Latin-1 and misinterpret Cyrillic/UTF-8 characters. The
/// parameter is permitted by RFC 8259 8.1 and does not change the base Media
/// Type, so a client matching on `application/dicom+json` still accepts it.
pub const DICOM_JSON_CONTENT_TYPE: &str = "application/dicom+json; charset=utf-8";

/// DICOM XML content type per PS3.18 Table 8.7-1.
///
/// No charset parameter: the document declares its encoding in the XML
/// declaration, which is where XML defines it.
pub const DICOM_XML_CONTENT_TYPE: &str = "application/dicom+xml";

// -- DICOM Failure Status Codes (PS3.7 Annex C, `standard/ps3.7/status-codes.json`)

/// `0111H` "Duplication SOP Instance" - the SOP Instance already exists.
///
/// PS3.7 status codes carry the code as a 16-bit hex value; DICOMweb reports it
/// in the response body, and PS3.18 Appendix F encodes a numeric attribute as a
/// JSON number, so the DECIMAL form is what a client parses.
pub const STATUS_DUPLICATE_SOP_INSTANCE: u16 = 0x0111;

/// `0112H` "No Such SOP Instance" - the referenced instance is not there.
pub const STATUS_NO_SUCH_SOP_INSTANCE: u16 = 0x0112;

/// `0110H` "Processing Failure" - the operation could not be performed.
pub const STATUS_PROCESSING_FAILURE: u16 = 0x0110;

/// The WADL media type a capabilities document is served as (PS3.18 8.9).
pub const WADL_CONTENT_TYPE: &str = "application/vnd.sun.wadl+xml";
