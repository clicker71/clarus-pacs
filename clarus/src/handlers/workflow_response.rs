// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Shared response types for the MWL and MPPS handlers.
//!
//! PS3.18 §14 and §15 render the same two media types (DICOM JSON default,
//! DICOM XML required), so the two services share one response shape and the
//! dispatch wrappers share one writer. Keeping the pair here - and not in each
//! handler module - is what lets `render_workflow` accept either service
//! without a second copy of the header layout.

#![forbid(unsafe_code)]

/// A fully-rendered MWL/MPPS response: status, content type, extra headers,
/// body. Mirrors `ups_pure::UpsResp` so the dispatch writer is the same shape.
pub struct WorkflowResp {
    pub status: u16,
    pub content_type: &'static str,
    /// Extra headers (e.g. the search truncation `Warning`).
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

/// A fully-rendered MWL/MPPS error: status + `{"error":..., "message":...}`.
pub struct WorkflowErr {
    pub status: u16,
    pub body: Vec<u8>,
}

/// A 400 naming a bad request, as a `WorkflowErr`.
pub(crate) fn bad_request(message: &str) -> WorkflowErr {
    WorkflowErr {
        status: 400,
        body: crate::handlers::json_common::err_msg_json("Bad Request", message),
    }
}

/// A 404 naming a missing record, as a `WorkflowErr`.
pub(crate) fn not_found(message: &str) -> WorkflowErr {
    WorkflowErr {
        status: 404,
        body: crate::handlers::json_common::err_msg_json("Not Found", message),
    }
}

/// A 409 with a DICOM Failure Status Code, per the standard's own vocabulary.
///
/// The MPPS N-CREATE duplicate case maps to a failure the standard names
/// (`0111H` "Duplicate SOP Instance", PS3.4 F.7.2.1). The code is carried in
/// the body so a conformant client can act on it rather than parse prose.
pub(crate) fn duplicate_instance(message: &str) -> WorkflowErr {
    WorkflowErr {
        status: 409,
        body: crate::handlers::json_common::err_msg_json(
            "Conflict",
            &format!("Duplicate SOP Instance (0111H): {message}"),
        ),
    }
}
