// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! UPS-RS handlers - Unified Procedure Step (DICOMweb PS3.18 Section 11).
//!
//! # Endpoints
//!
//! The two Workitem transactions differ by RESOURCE SHAPE, not by HTTP method -
//! both are POST (PS3.18 11.4.1.1 / 11.6.1, `standard/ps3.18/ups-rs-transactions.json`):
//!
//! - `GET /dicomweb/workitems` - Search for workitems (G-U1)
//! - `POST /dicomweb/workitems?workitem={uid}` - Create workitem (G-U2). The
//!   SOP Instance UID is a QUERY parameter and there is no path segment after
//!   `workitems`. The parameter is optional only because the SAME fact may
//!   travel in the Workitem Data Set: 11.4.1.2 gives the request body the Type 1
//!   SOP Instance UID, so a request that omits the parameter MUST name the UID
//!   in the data set. The standard does NOT let the origin server invent one -
//!   the workitem identity is the requester's.
//! - `POST /dicomweb/workitems/{uid}` - Update workitem (G-U3). The UID is a
//!   PATH segment and the Transaction UID travels as the query parameter (or in
//!   the N-SET data set); this is a different operation from Change State.
//! - `DELETE /dicomweb/workitems/{uid}` - NON-STANDARD ADMINISTRATIVE
//!   EXTENSION, not a UPS-RS transaction.
//!
//! PS3.18 11.4-11.13 defines DELETE for a SUBSCRIPTION resource only
//! (`/workitems/{resource}/subscribers/{subscriber}`, `standard/ps3.18/ups-rs-transactions.json`);
//! there is no Delete Workitem transaction, so a conformant client never calls
//! the route above. It exists as a non-standard administrative extension for
//! cleaning up test data, and this module doc must not present it as part of
//! the standard surface.
//!
//! # Workflow State Machine (PS3.4 CC.1.1, `ups-cc-1.1-state-machine.json`)
//!
//! Exactly FOUR states. There is no SUSPENDED state in the UPS IOD - it was
//! removed from the standard, and this server stores only the four below, so a
//! request naming SUSPENDED is refused rather than accepted as a fifth value.
//!
//! ```text
//! SCHEDULED -> IN PROGRESS -> COMPLETED / CANCELED
//! ```
//!
//! A SCHEDULED workitem may not reach a Final State directly: CC.2.2.3 requires
//! IN PROGRESS first, which is why a cancel of a queued workitem performs two
//! transitions.

#![forbid(unsafe_code)]

pub mod event_bus;
pub mod json;
pub mod state_machine;
pub mod types;
pub mod xml;

pub use types::UpsQuery;
