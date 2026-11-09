// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! HTTP/1.1 implementation - nginx-way, std only, thread-per-connection.
//!
//! Replaces axum/hyper/tower/tower-http/multer. Blocking sockets, zero
//! allocations in the hotpath (`ap101`-guarded), and a hard limit on header
//! size / count for slowloris and request-smuggling protection.
//!
//! # Modules
//!
//! - [`parse`] - request-line + fixed-`[ParsedHeader; 16]` header parser.
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
