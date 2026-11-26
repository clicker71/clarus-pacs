// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Shared response types for the MWL and MPPS handlers.
//!
//! PS3.18 §14 and §15 render the same two media types (DICOM JSON default,
//! DICOM XML required), so the two services share one response shape and the
//! dispatch wrappers share one writer. Keeping the pair here - and not in each
//! handler module - is what lets `render_workflow` accept either service
//! without a second copy of the header layout.

#![forbid(unsafe_code)]

/// HTTP 200 OK (RFC 9110 15.3.1, listed for DICOMweb in PS3.18 8.5,
/// `standard/ps3.18/http-status-codes-8.5.json`).
pub const HTTP_OK: u16 = 200;

/// HTTP 201 Created (RFC 9110 15.3.2): a successful N-CREATE (PS3.18 15.4.3.3).
pub const HTTP_CREATED: u16 = 201;

/// HTTP 400 Bad Request (PS3.18 8.5): the client sent something malformed.
pub const HTTP_BAD_REQUEST: u16 = 400;

/// HTTP 404 Not Found (PS3.18 8.5): no representation for the target resource.
pub const HTTP_NOT_FOUND: u16 = 404;

/// HTTP 409 Conflict (PS3.18 8.5): the request conflicts with the resource's
/// current state - the DICOMweb spelling of a DIMSE failure status.
pub const HTTP_CONFLICT: u16 = 409;

/// A fully-rendered MWL/MPPS response: status, content type, extra headers,
/// body. Mirrors `ups_pure::UpsResp` so the dispatch writer is the same shape.
pub struct WorkflowResp {
    pub status: u16,
    pub content_type: &'static str,
    /// Extra headers (e.g. the search truncation `Warning`).
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl WorkflowResp {
    /// A status with NO payload.
    ///
    /// PS3.18 15.4.3.3 gives a successful Create no response body, and 15.5.3.3
    /// recommends the same for Update: the created or updated resource is
    /// retrieved with GET. Rendering the dataset on those replies made a 201
    /// indistinguishable from a retrieve and gave a client a body the standard
    /// says it will not get.
    #[must_use]
    pub fn empty(status: u16) -> Self {
        Self {
            status,
            content_type: "",
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
}

/// A fully-rendered MWL/MPPS error: status + `{"error":..., "message":...}`.
pub struct WorkflowErr {
    pub status: u16,
    pub body: Vec<u8>,
}

/// A 400 naming a bad request, as a `WorkflowErr`.
pub(crate) fn bad_request(message: &str) -> WorkflowErr {
    WorkflowErr {
        status: HTTP_BAD_REQUEST,
        body: crate::handlers::json_common::err_msg_json("Bad Request", message),
    }
}

/// A 404 naming a missing record, as a `WorkflowErr`.
pub(crate) fn not_found(message: &str) -> WorkflowErr {
    WorkflowErr {
        status: HTTP_NOT_FOUND,
        body: crate::handlers::json_common::err_msg_json("Not Found", message),
    }
}

/// A 409 with a DICOM Failure Status Code, per the standard's own vocabulary.
///
/// The MPPS N-CREATE duplicate case maps to a failure the standard names
/// (`0111H` "Duplication SOP Instance", PS3.7 Annex C, `standard/ps3.7/
/// status-codes.json`). The code is carried in the body so a conformant client
/// can act on it rather than parse prose.
pub(crate) fn duplicate_instance(message: &str) -> WorkflowErr {
    WorkflowErr {
        status: HTTP_CONFLICT,
        body: crate::handlers::json_common::err_msg_json(
            "Conflict",
            &format!(
                "Duplicate SOP Instance ({:04X}H): {message}",
                super::STATUS_DUPLICATE_SOP_INSTANCE
            ),
        ),
    }
}
