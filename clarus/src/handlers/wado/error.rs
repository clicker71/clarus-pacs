// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Error types for WADO-RS handlers.
//!
//! Provides [`WadoError`] - a unified error enum used by the sync dispatch
//! stack; the byte-parity response rendering lives in `dispatch_streams`
//! (`wado_err_response`).

#![forbid(unsafe_code)]

/// Error type for WADO handlers.
#[derive(Debug)]
pub enum WadoError {
    /// Invalid UID format (Path Traversal attempt, null byte, etc.).
    InvalidUid(String),
    /// Instance not found in database.
    NotFound(String),
    /// File not found in CAS (inconsistent state).
    FileNotFound(String),
    /// Range not satisfiable.
    RangeNotSatisfiable,
    /// Parameter not acceptable (unsupported feature requested by client).
    NotAcceptable(String),
    /// Internal error (DB, I/O, etc.).
    Internal(String),
    /// A query parameter is ill-formed or asks for something the origin
    /// server does not support (PS3.18 8.3.5.1.3: an ill-formed or
    /// unsupported `viewport` on a Thumbnail is a 400, not a silent
    /// clamp). Carries the human-readable reason for the Status Report.
    BadRequest(String),
    /// Decode-time resource limit exceeded (e.g. thumbnail pixel cap).
    PayloadTooLarge(String),
    /// The same 413 for a refusal whose text is known at compile time.
    ///
    /// Exists so a budget refusal on an attacker-controlled request does not
    /// allocate a message for every oversized tile; the ap101 gate on the
    /// per-tile decode asserts zero bytes on exactly this path.
    PayloadTooLargeStatic(&'static str),
    /// The bounded transcode/render pool is saturated - honest fast 503, no
    /// unbounded queue. The client should retry after `Retry-After`.
    Busy,
}

impl std::fmt::Display for WadoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WadoError::InvalidUid(msg) => write!(f, "Invalid UID: {msg}"),
            WadoError::NotFound(msg) => write!(f, "Not found: {msg}"),
            WadoError::FileNotFound(sha) => write!(f, "File not found: {sha}"),
            WadoError::RangeNotSatisfiable => write!(f, "Range not satisfiable"),
            WadoError::NotAcceptable(msg) => write!(f, "Not acceptable: {msg}"),
            WadoError::Internal(msg) => write!(f, "Internal error: {msg}"),
            WadoError::BadRequest(msg) => write!(f, "Bad request: {msg}"),
            WadoError::PayloadTooLarge(msg) => write!(f, "Payload too large: {msg}"),
            WadoError::PayloadTooLargeStatic(msg) => write!(f, "Payload too large: {msg}"),
            WadoError::Busy => write!(f, "Transcode pool busy"),
        }
    }
}

impl std::error::Error for WadoError {}
