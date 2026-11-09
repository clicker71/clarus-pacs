// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! HTTP route classification for metrics and middleware.
//!
//! Extracted from the former `auth.rs` - auth is now handled by nginx.
//! This module provides route-type and route-ID enums for Prometheus
//! per-endpoint counters, without any authentication logic.
//!
//! # Route types
//! - `Public` - `/health`, `/metrics` (no auth, open to load balancers)
//! - `Dicomweb` - DICOMweb endpoints (auth by nginx)
//! - `Admin` - admin API endpoints (auth by nginx, IP-restricted)

#![forbid(unsafe_code)]

// ---------------------------------------------------------------------------
// Route ID classification (for per-endpoint Prometheus counters)
// ---------------------------------------------------------------------------

/// Route identifiers for per-endpoint Prometheus counters.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum RouteId {
    Health = 0,
    Metrics = 1,
    /// GET /dicomweb/patients - QIDO-RS patient search (PS3.18 Section 8.3).
    QidoPatients = 2,
    QidoStudies = 3,
    WadoStudy = 4,
    StowStudies = 5,
    /// Admin routes (/admin/v1/*)
    Admin = 6,
    /// Storage Commitment over DICOMweb: /dicomweb/commitment-requests/*
    /// (PS3.18 13.4/13.5). Its own route because the verdict distribution
    /// (2xx vs 4xx) is the signal a commitment deployment is watched on.
    Commitment = 7,
    /// UPS-RS workitems (/dicomweb/workitems*).
    Ups = 8,
    /// Catch-all for unknown/unmatched paths.
    Other = 9,
}

/// Number of distinct routes tracked in the metrics array.
pub const NUM_ROUTES: usize = 10;

/// Map an HTTP method + URI path to a [`RouteId`] without heap allocation.
pub fn classify_route(method: &str, path: &str) -> RouteId {
    if path.starts_with("/admin/v1/") {
        return RouteId::Admin;
    }
    match method {
        // HEAD carries no body but the same routing and the same probe
        // semantics as GET (PS3.18 8.1: HEAD is answered like GET without a
        // message body). Without this arm a HEAD on `/health` or `/metrics`
        // fell into `Other` and flooded the access log and audit trail the
        // RouteId-keyed exclusions in serve.rs rely on.
        "GET" | "HEAD" => match path {
            // `/health/status` shares the Health metrics bucket (and the
            // access-log exclusion) with `/health` - both are liveness/
            // telemetry probes, not user traffic.
            "/health" | "/health/status" => RouteId::Health,
            "/metrics" => RouteId::Metrics,
            "/dicomweb/patients" => RouteId::QidoPatients,
            "/dicomweb/studies" => RouteId::QidoStudies,
            p if p.starts_with("/dicomweb/studies/") => RouteId::WadoStudy,
            p if p.starts_with("/dicomweb/commitment-requests") => RouteId::Commitment,
            p if p.starts_with("/dicomweb/workitems") => RouteId::Ups,
            _ => RouteId::Other,
        },
        "POST" if path == "/dicomweb/studies" => RouteId::StowStudies,
        // Study-scoped STOW (`POST /dicomweb/studies/{study}`) is the second
        // Store Transaction target (PS3.18 10.5.1.1) and shares the metrics
        // bucket with the root form.
        "POST" if path.starts_with("/dicomweb/studies/") => RouteId::StowStudies,
        // Commitment POST is the request; workitems POST is Create/Search.
        "POST" if path.starts_with("/dicomweb/commitment-requests") => RouteId::Commitment,
        "POST" if path.starts_with("/dicomweb/workitems") => RouteId::Ups,
        "PUT" if path.starts_with("/dicomweb/workitems") => RouteId::Ups,
        _ => RouteId::Other,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_route() {
        assert_eq!(classify_route("GET", "/health"), RouteId::Health);
        assert_eq!(classify_route("GET", "/health/status"), RouteId::Health);
        assert_eq!(classify_route("GET", "/metrics"), RouteId::Metrics);
        assert_eq!(classify_route("GET", "/admin/v1/status"), RouteId::Admin);
        assert_eq!(classify_route("POST", "/admin/v1/db/stats"), RouteId::Admin);
        assert_eq!(
            classify_route("GET", "/dicomweb/patients"),
            RouteId::QidoPatients
        );
        assert_eq!(
            classify_route("GET", "/dicomweb/studies"),
            RouteId::QidoStudies
        );
        assert_eq!(
            classify_route("POST", "/dicomweb/studies"),
            RouteId::StowStudies
        );
        assert_eq!(
            classify_route("GET", "/dicomweb/commitment-requests/1.2.3"),
            RouteId::Commitment
        );
        assert_eq!(classify_route("POST", "/dicomweb/workitems"), RouteId::Ups);
    }

    #[test]
    fn test_head_classifies_like_get() {
        // PS3.18 8.1: HEAD is answered like GET without a body. The probe
        // routes matter most - they are excluded from the access log and the
        // audit trail by RouteId, so an `Other` bucket floods both.
        assert_eq!(classify_route("HEAD", "/health"), RouteId::Health);
        assert_eq!(classify_route("HEAD", "/health/status"), RouteId::Health);
        assert_eq!(classify_route("HEAD", "/metrics"), RouteId::Metrics);
        assert_eq!(
            classify_route("HEAD", "/dicomweb/studies/1.2.3"),
            RouteId::WadoStudy
        );
        // A method with no GET semantics still lands in Other.
        assert_eq!(classify_route("DELETE", "/health"), RouteId::Other);
    }

    #[test]
    fn test_classify_commitment_and_ups() {
        // Commitment (PS3.18 13.4/13.5): POST the request, GET the verdict.
        assert_eq!(
            classify_route("POST", "/dicomweb/commitment-requests/1.2.3"),
            RouteId::Commitment
        );
        assert_eq!(
            classify_route("GET", "/dicomweb/commitment-requests/1.2.3"),
            RouteId::Commitment
        );
        // UPS-RS workitems: create/search (POST), retrieve (GET), state (PUT).
        assert_eq!(classify_route("POST", "/dicomweb/workitems"), RouteId::Ups);
        assert_eq!(
            classify_route("GET", "/dicomweb/workitems/1.2.3"),
            RouteId::Ups
        );
        assert_eq!(
            classify_route("PUT", "/dicomweb/workitems/1.2.3/state"),
            RouteId::Ups
        );
        // They no longer land in Other.
        assert_ne!(
            classify_route("GET", "/dicomweb/commitment-requests/1.2.3"),
            RouteId::Other
        );
        assert_ne!(
            classify_route("POST", "/dicomweb/workitems"),
            RouteId::Other
        );
    }
}
