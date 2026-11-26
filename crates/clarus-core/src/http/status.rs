// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! HTTP status codes as PS3.18 8.5 defines them for DICOMweb.
//!
//! The facts live in `standard/ps3.18/http-status-codes-8.5.json` (source:
//! PS3.18 8.5), which splits every code by the obligation word the standard
//! uses. This module is the code-side spelling of the same set, so a handler
//! names a code instead of typing a number: a bare `400` in a match arm is a
//! protocol fact with no provenance, while `status::BAD_REQUEST` points at the
//! clause.
//!
//! Only the codes DICOMweb actually uses are listed. A status the server emits
//! but PS3.18 does not discuss for DICOMweb (e.g. 502 from an upstream proxy)
//! belongs where it is raised, not here.

/// 200 OK - the transaction succeeded.
pub const OK: u16 = 200;

/// 202 Accepted - STOW-RS accepted the request with warnings (PS3.18 10.5.2).
pub const ACCEPTED: u16 = 202;

/// 204 No Content - the instance was stored with no response payload.
pub const NO_CONTENT: u16 = 204;

/// 400 Bad Request - the request is malformed or a parameter is invalid.
pub const BAD_REQUEST: u16 = 400;

/// 401 Unauthorized - authentication is required or failed.
pub const UNAUTHORIZED: u16 = 401;

/// 403 Forbidden - authenticated, but not permitted.
pub const FORBIDDEN: u16 = 403;

/// 404 Not Found - the Target Resource does not exist.
pub const NOT_FOUND: u16 = 404;

/// 405 Method Not Allowed - the resource does not support the method; the
/// response carries `Allow` (PS3.18 8.5).
pub const METHOD_NOT_ALLOWED: u16 = 405;

/// 406 Not Acceptable - the `Accept` header names no representation the server
/// can produce (RFC 9110 15.5.7, PS3.18 10.4.3.1).
pub const NOT_ACCEPTABLE: u16 = 406;

/// 409 Conflict - the request conflicts with the current state of the target.
pub const CONFLICT: u16 = 409;

/// 410 Gone - the Target Resource existed and has been removed.
pub const GONE: u16 = 410;

/// 411 Length Required - the request carries no usable framing.
pub const LENGTH_REQUIRED: u16 = 411;

/// 413 Payload Too Large - the request body exceeds a server limit.
pub const PAYLOAD_TOO_LARGE: u16 = 413;

/// 414 URI Too Long - the request target is longer than the server accepts.
pub const URI_TOO_LONG: u16 = 414;

/// 415 Unsupported Media Type - the request body's media type is not usable.
pub const UNSUPPORTED_MEDIA_TYPE: u16 = 415;

/// 416 Range Not Satisfiable - the byte-Range header cannot be satisfied by
/// the body. This is a statement about the RANGE HEADER only (RFC 9110 14.4);
/// a frame number or UID that names no resource is a 404.
pub const RANGE_NOT_SATISFIABLE: u16 = 416;

/// 500 Internal Server Error - the server failed on a request it understood.
pub const INTERNAL_SERVER_ERROR: u16 = 500;

/// 503 Service Unavailable - the server cannot serve the request right now, so
/// the client should retry later.
pub const SERVICE_UNAVAILABLE: u16 = 503;

/// 507 Insufficient Storage - the store cannot accept the payload for lack of
/// space.
pub const INSUFFICIENT_STORAGE: u16 = 507;

#[cfg(test)]
mod tests {
    use super::*;

    /// The numbers are the standard's, and they agree with the fact file the
    /// module is derived from. Pinned so an edit here cannot silently move a
    /// code without the source of truth moving too.
    #[test]
    fn the_codes_match_the_standard_set() {
        assert_eq!(OK, 200);
        assert_eq!(ACCEPTED, 202);
        assert_eq!(NO_CONTENT, 204);
        assert_eq!(BAD_REQUEST, 400);
        assert_eq!(UNAUTHORIZED, 401);
        assert_eq!(FORBIDDEN, 403);
        assert_eq!(NOT_FOUND, 404);
        assert_eq!(METHOD_NOT_ALLOWED, 405);
        assert_eq!(NOT_ACCEPTABLE, 406);
        assert_eq!(CONFLICT, 409);
        assert_eq!(GONE, 410);
        assert_eq!(LENGTH_REQUIRED, 411);
        assert_eq!(PAYLOAD_TOO_LARGE, 413);
        assert_eq!(URI_TOO_LONG, 414);
        assert_eq!(UNSUPPORTED_MEDIA_TYPE, 415);
        assert_eq!(RANGE_NOT_SATISFIABLE, 416);
        assert_eq!(INTERNAL_SERVER_ERROR, 500);
        assert_eq!(SERVICE_UNAVAILABLE, 503);
        assert_eq!(INSUFFICIENT_STORAGE, 507);
    }

    /// PS3.18 8.5's classification rule: 4xx names the client, 5xx the server.
    /// The sets below are the ones the file calls out, so the split is a fact
    /// about the standard rather than a convention of this module.
    #[test]
    fn the_client_and_server_classes_are_split_at_500() {
        let client = [
            BAD_REQUEST,
            UNAUTHORIZED,
            FORBIDDEN,
            NOT_FOUND,
            METHOD_NOT_ALLOWED,
            NOT_ACCEPTABLE,
            CONFLICT,
            GONE,
            LENGTH_REQUIRED,
            PAYLOAD_TOO_LARGE,
            URI_TOO_LONG,
            UNSUPPORTED_MEDIA_TYPE,
            RANGE_NOT_SATISFIABLE,
        ];
        for code in client {
            assert!((400..500).contains(&code), "{code} must be a 4xx");
        }
        let server = [
            INTERNAL_SERVER_ERROR,
            SERVICE_UNAVAILABLE,
            INSUFFICIENT_STORAGE,
        ];
        for code in server {
            assert!((500..600).contains(&code), "{code} must be a 5xx");
        }
    }
}
