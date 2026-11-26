// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! HTTP/1.1 implementation - nginx-way, std only, thread-per-connection.
//!
//! Replaces axum/hyper/tower/tower-http/multer. Blocking sockets and a hard
//! limit on header size / count for slowloris and request-smuggling
//! protection.
//!
//! # Allocation discipline
//!
//! The ap101 guarantee covers REQUEST PARSING and ROUTE MATCHING: `parse`
//! writes into the caller's byte slice, and `router` matches borrowed path
//! segments, so neither allocates for any input inside the header limits.
//! Headers are stored as borrowed slices in `RequestHead`, not in a
//! `[ParsedHeader; 16]` array, and the count limit lives in `MAX_HEADERS`
//! independently of that storage.
//!
//! It does NOT cover the request path end to end. Identity resolution
//! (`auth::resolve_identity`) allocates owned `Vec<u8>` for the peer address
//! and the user name, and the handlers allocate for their own responses.
//! Reading the sentence above as a whole-path guarantee would be wrong.
//!
//! # Modules
//!
//! - [`parse`] - request-line + `RequestHead` (borrowed header slices).
//! - [`response`] - per-connection header buffer + writev/sendfile writer.
//! - [`router`] - method-first, manual segment matching, UID slices.
//! - [`multipart`] - streaming multipart/related boundary state machine.
//! - [`body`] - Content-Length + chunked (TE) body reader.
//! - [`conn`] - connection loop, keep-alive, per-request timeouts.
//! - [`auth`] - trusted-proxy client identity (X-Real-IP / X-Authenticated-User).

pub mod auth;
pub mod body;
pub mod conn;
pub mod multipart;
pub mod parse;
pub mod response;
pub mod router;
pub mod status;
