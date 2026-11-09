// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! UPS-RS handlers - Unified Procedure Step (DICOMweb PS3.18 Section 11).
//!
//! # Endpoints
//! - `GET /dicomweb/workitems` - Search for workitems (G-U1)
//! - `POST /dicomweb/workitems/{uid}` - Create workitem (G-U2)
//! - `PUT /dicomweb/workitems/{uid}` - Update workitem (G-U3)
//! - `DELETE /dicomweb/workitems/{uid}` - Delete workitem (R-03)
//!
//! # Workflow State Machine (PS3.18 Section 11.3)
//! ```text
//! SCHEDULED -> IN PROGRESS -> COMPLETED / CANCELED
//!                 |
//!                 v
//!             SUSPENDED -> IN PROGRESS (resume)
//! ```

#![forbid(unsafe_code)]

pub mod event_bus;
pub mod json;
pub mod state_machine;
pub mod types;
pub mod xml;

pub use types::UpsQuery;
