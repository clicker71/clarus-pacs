// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! HTTP/1.1 request router (E-005).
//!
//! Method-first fall-through matcher with manual `/`-segment comparison - no
//! trie, no regex. UID capture is a borrowed slice, validated as `[0-9.]*`
//! and `<= 64` bytes (PS3.5), so malformed UIDs become `400` not a miss.

use crate::http::parse::Method;

/// Route access policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Any client may call (health, metrics, capabilities).
    Any,
    /// Loopback-only. Enforced by the connection loop using the *resolved*
    /// client IP, not the raw socket peer.
    LocalhostOnly,
    /// Administrative surface: the loopback machine, or a client inside a
    /// configured `[server] admin_cidrs` network. Enforced by the connection
    /// loop on the **resolved** client IP, so a proxy-fronted management VLAN
    /// works. With no `admin_cidrs` configured this is exactly loopback-only.
    Admin,
    /// Everything else - gated by nginx in front.
    Default,
}

/// Response keep-alive policy for a matched route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepAlive {
    /// Keep the connection open (QIDO, STOW, UPS).
    On,
    /// Close after the response (WADO large streams).
    Off,
}

/// A matched route with its captured UID segments.
#[derive(Debug, Clone, Copy)]
pub struct RouteMatch<'a> {
    /// Stable identifier for metrics (route classification).
    pub id: &'static str,
    /// Access policy.
    pub access: Access,
    /// Keep-alive policy.
    pub keep_alive: KeepAlive,
    /// Captured UID segments (borrowed slices into the request path).
    pub uids: [&'a [u8]; 3],
    /// Number of captured UIDs valid in `[uids]`.
    pub n_uids: usize,
    /// Matched through the QA service root (`/dicomweb_qa`).
    ///
    /// The QA root serves the same resources but with sequestered Quality
    /// rejections visible (AIRA 57.2.1: a webservice endpoint configured to
    /// provide access to hidden instances). The serve loop requires the CIDR
    /// grant before honouring it, so this flag is never trusted on its own.
    pub qa: bool,
}

/// Result of matching a `(method, path)`.
#[derive(Debug, Clone, Copy)]
pub enum RouterOutcome<'a> {
    Match(RouteMatch<'a>),
    /// No route matched for this method + path.
    NotFound,
    /// Method not allowed on this path - reply `405 + Allow`.
    MethodNotAllowed {
        allow: &'static str,
    },
    /// Invalid UID (bad bytes or too long) -> 400.
    BadUid,
    /// CORS preflight - the serve loop answers `204` for any path.
    Options,
}

fn uid_ok(uid: &[u8]) -> bool {
    if uid.is_empty() || uid.len() > 64 {
        return false;
    }
    // Allowed bytes are digits and `.`. Additionally require at least one
    // digit: the byte check alone accepts `..` and `.`, which are not UIDs and
    // are exactly what a path-traversal probe sends (`/studies/../series`).
    // Without this, `..` reached the store layer as a "UID" and came back as
    // an empty result rather than a 400, which reads as "the study exists but
    // is empty". Two comparisons, zero allocation, so the hot path is intact.
    let mut has_digit = false;
    for &b in uid {
        if b.is_ascii_digit() {
            has_digit = true;
        } else if b != b'.' {
            return false;
        }
    }
    has_digit
}

fn frame_ok(frame: &[u8]) -> bool {
    !frame.is_empty() && frame.iter().all(|b| b.is_ascii_digit())
}

/// Max path segments we will ever inspect (deepest route has 9).
const MAX_SEGS: usize = 12;

/// SOP Class UID of the Worklist Subscription (PS3.18 Table 11.1.1-1).
/// It appears literally as a path segment in the subscription URI Template.
const WORKLIST_SUBSCRIPTION_UID: &str = "1.2.840.10008.5.1.4.34.5";

/// SOP Class UID of the Filtered Worklist Subscription (PS3.18 Table 11.1.1-1).
const FILTERED_WORKLIST_SUBSCRIPTION_UID: &str = "1.2.840.10008.5.1.4.34.5.1";

/// Split a path into `/`-separated segments into a fixed stack array
/// (0 allocation, hotpath-critical).
fn split_segments(path: &[u8]) -> ([&[u8]; MAX_SEGS], usize) {
    let mut segs = [&[][..]; MAX_SEGS];
    let mut n = 0usize;
    let mut start = 0usize;
    let bytes = path;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'/' {
            if i > start && n < MAX_SEGS {
                segs[n] = &bytes[start..i];
                n += 1;
            }
            start = i + 1;
        }
    }
    if start < bytes.len() && n < MAX_SEGS {
        segs[n] = &bytes[start..];
        n += 1;
    }
    (segs, n)
}

/// The QA service root prefix. PS3.18 8.2 leaves the Service Root Path
/// deployment-defined and allows one Base URI to support more than one
/// Service, so a second root is standard-conformant. Fixed in code by owner
/// decision: an operator who needs a different shape routes it in nginx.
const QA_PREFIX: &[u8] = b"/dicomweb_qa";

/// Split a QA-root path into the segments the ordinary table understands.
///
/// `/dicomweb_qa/studies/1.2.3` yields the same segment list a `/dicomweb/...`
/// request would: the `dicomweb_qa` first segment becomes `dicomweb`. Returns
/// `None` when the path is not under the QA root.
///
/// Zero allocation, and every returned slice borrows `path`, so the captured
/// UID segments stay valid for the request lifetime - a rewritten `Vec` would
/// dangle the moment this function returned.
fn qa_segments(path: &[u8]) -> Option<([&[u8]; MAX_SEGS], usize)> {
    let (segs, n, _rest) = qa_segments_with_rest(path)?;
    Some((segs, n))
}

/// As [`qa_segments`], also returning the path tail after the QA root.
///
/// The tail starts with `/` (or is empty) and borrows `path`, like every other
/// slice this module produces. Segments of a QA path, how many are valid, and
/// the tail: the shape is fixed by the borrow, so the router stays
/// allocation-free on the request path.
type QaSegments<'a> = ([&'a [u8]; MAX_SEGS], usize, &'a [u8]);

fn qa_segments_with_rest(path: &[u8]) -> Option<QaSegments<'_>> {
    if !path.starts_with(QA_PREFIX) {
        return None;
    }
    let rest = &path[QA_PREFIX.len()..];
    // `/dicomweb_qax` is a different path, not a QA resource: only a '/'
    // boundary continues the QA root.
    if !rest.is_empty() && rest[0] != b'/' {
        return None;
    }
    let (mut segs, n) = split_segments(path);
    // The first segment of a QA path is always `dicomweb_qa`; the table
    // matches `dicomweb`, and every downstream slice still points into
    // `path`. Swapping this ONE borrowed literal is enough.
    segs[0] = b"dicomweb";
    Some((segs, n, rest))
}

/// Match a request against the DICOMweb route table.
///
/// A path under the QA root (`/dicomweb_qa/...`) is matched against the same
/// resources with the match flagged `qa`. GET and HEAD only: the QA root is a
/// read surface, so STOW/PUT/DELETE there fall through and answer 404 rather
/// than silently writing through a root the caller was told is read-only.
pub fn match_route<'a>(method: Method, path: &'a [u8]) -> RouterOutcome<'a> {
    match method {
        Method::Get | Method::Head => {
            if let Some((segs, n, _rest)) = qa_segments_with_rest(path) {
                return match match_get_qa(&segs[..n]) {
                    RouterOutcome::Match(mut m) => {
                        m.qa = true;
                        RouterOutcome::Match(m)
                    }
                    other => other,
                };
            }
            // Non-QA path: split once into the stack array and borrow it.
            let (segs, n) = split_segments(path);
            match_get(path, &segs[..n])
        }
        Method::Post => {
            let (segs, n) = split_segments(path);
            match_post(&segs[..n])
        }
        Method::Put => {
            let (segs, n) = split_segments(path);
            match_put(&segs[..n])
        }
        Method::Delete => {
            let (segs, n) = split_segments(path);
            match_delete(&segs[..n])
        }
        Method::Options => {
            // 8.9.1: OPTIONS on the Base URI is the Capabilities Transaction;
            // every other OPTIONS stays a CORS preflight. The QA root answers
            // capabilities too: it is a service root of its own.
            if let Some((qs, qn)) = qa_segments(path)
                && qn <= 1
                && (qn == 0 || seg_eq(qs[0], "dicomweb"))
            {
                return m0("capabilities", Access::Any, KeepAlive::On);
            }
            let (segs, n) = split_segments(path);
            let segs = &segs[..n];
            if segs.is_empty() || (segs.len() == 1 && seg_eq(segs[0], "dicomweb")) {
                return m0("capabilities", Access::Any, KeepAlive::On);
            }
            RouterOutcome::Options
        }
        Method::Unknown => RouterOutcome::MethodNotAllowed {
            allow: "GET, HEAD, POST, PUT, DELETE",
        },
    }
}

/// Match a QA-root request.
///
/// Collection roots on the QA root carry the QA byte prefix, so they cannot be
/// compared against the canonical literals `match_get` uses. Segment shape is
/// unambiguous instead: the collection roots are the only routes with exactly
/// two segments. Anything deeper is study- or series-scoped and already matched
/// by segment content, so it delegates.
fn match_get_qa<'a>(segs: &[&'a [u8]]) -> RouterOutcome<'a> {
    if segs.len() == 2 {
        return match segs[1] {
            b"patients" => m0("qido_patients", Access::Admin, KeepAlive::On),
            b"studies" => m0("qido_studies", Access::Default, KeepAlive::On),
            b"series" => m0("qido_series_global", Access::Default, KeepAlive::On),
            b"instances" => m0("qido_instances_global", Access::Default, KeepAlive::On),
            b"workitems" => m0("ups_search", Access::Default, KeepAlive::On),
            b"modality-scheduled-procedure-steps" => {
                m0("mwl_search", Access::Default, KeepAlive::On)
            }
            b"wadouri" => m0("wado_uri", Access::Default, KeepAlive::On),
            _ => RouterOutcome::NotFound,
        };
    }
    if segs.len() == 1 {
        // The QA collection root itself: the same Capabilities answer the
        // canonical root gives, so a client can discover the service.
        return m0("capabilities", Access::Any, KeepAlive::On);
    }
    // Deeper resources match on segment content, which the rewritten first
    // segment already supplies. The path argument is only consulted for the
    // root literals already handled above, so an empty slice is correct here.
    match_get(b"", segs)
}

fn match_get<'a>(path: &'a [u8], segs: &[&'a [u8]]) -> RouterOutcome<'a> {
    // Public + capabilities + viewer
    if path == b"/health" {
        return m0("health", Access::Any, KeepAlive::On);
    }
    // Structured viewer health/status endpoint. A
    // separate id from `/health` because the viewer polls it every 5s and it
    // returns JSON indicators, not the liveness 200/503 contract.
    if path == b"/health/status" {
        return m0("health_status", Access::Any, KeepAlive::On);
    }
    // Build identification: which binary is this? A constant render, public
    // like `/health` - an operator debugging a field box must not need
    // credentials to read a version string.
    if path == b"/version" {
        return m0("version", Access::Any, KeepAlive::On);
    }
    if path == b"/metrics" {
        return m0("metrics", Access::Any, KeepAlive::On);
    }
    if path == b"/" {
        return m0("viewer", Access::Default, KeepAlive::On);
    }
    if path == b"/dicomweb/" {
        return m0("capabilities", Access::Any, KeepAlive::On);
    }
    if path == b"/dicomweb" {
        return m0("capabilities_redirect", Access::Any, KeepAlive::On);
    }

    // QIDO
    //
    // The patient registry is the viewer's entry point (the embedded viewer
    // calls it first), so it is both the most sensitive read surface and the
    // one an operator cannot work without. It therefore shares the admin
    // policy: the local machine, or a configured management network.
    if path == b"/dicomweb/patients" {
        return m0("qido_patients", Access::Admin, KeepAlive::On);
    }
    if path == b"/dicomweb/studies" {
        return m0("qido_studies", Access::Default, KeepAlive::On);
    }
    // Top-level (global) series/instance QIDO - no study scope (gap 2).
    if path == b"/dicomweb/series" {
        return m0("qido_series_global", Access::Default, KeepAlive::On);
    }
    // Weasis-compat (06.09 field incident): Weasis appends "/instances" to
    // the QIDO series RetrieveURL and queries it verbatim. The global series
    // search emits the series-root form, so this path must resolve.
    if segs.len() == 4
        && seg_eq(segs[0], "dicomweb")
        && seg_eq(segs[1], "series")
        && seg_eq(segs[3], "instances")
    {
        let series = segs[2];
        if !uid_ok(series) {
            return RouterOutcome::BadUid;
        }
        return m1(
            "qido_series_root_instances",
            Access::Default,
            KeepAlive::On,
            series,
        );
    }
    if path == b"/dicomweb/instances" {
        return m0("qido_instances_global", Access::Default, KeepAlive::On);
    }
    if path == b"/dicomweb/workitems" {
        return m0("ups_search", Access::Default, KeepAlive::On);
    }
    // PS3.18 14.4: Search is the ONLY transaction of the Modality Scheduled
    // Procedure Step Service (14.3: no Create exists in DIMSE or DICOMweb), so
    // the collection resource carries one GET and nothing else.
    if path == b"/dicomweb/modality-scheduled-procedure-steps" {
        return m0("mwl_search", Access::Default, KeepAlive::On);
    }
    if path == b"/wadouri" || path == b"/dicomweb/wadouri" {
        return m0("wado_uri", Access::Default, KeepAlive::On);
    }

    // /dicomweb/studies/{study}...
    if segs.len() >= 3 && seg_eq(segs[0], "dicomweb") && seg_eq(segs[1], "studies") {
        let study = segs[2];
        if !uid_ok(study) {
            return RouterOutcome::BadUid;
        }
        if segs.len() == 3 {
            return m1("wado_study", Access::Default, KeepAlive::On, study);
        }
        if segs.len() == 4 && seg_eq(segs[3], "instances") {
            return m1(
                "qido_study_instances",
                Access::Default,
                KeepAlive::On,
                study,
            );
        }
        if segs.len() == 4 && seg_eq(segs[3], "series") {
            // PS3.18 SearchForSeries: GET /dicomweb/studies/{study}/series
            // (the miniviewer walks patients -> studies -> series -> instances).
            return m1("qido_series", Access::Default, KeepAlive::On, study);
        }
        // C2 (B3 all-or-none, PS3.18 10.4.1.1.4): study-level thumbnail.
        // `/rendered` is NOT an alias here: study-level rendering would need a
        // montage encoder, and 8.7.4-1 gives image/jpeg only for single-frame.
        // Serving the thumbnail bytes under the rendered name was a MANDATORY
        // deviation (10.4.3.3.3), so the name is left unmapped (404).
        if segs.len() == 4 && seg_eq(segs[3], "thumbnail") {
            return m1(
                "wado_study_thumbnail",
                Access::Default,
                KeepAlive::On,
                study,
            );
        }
        // PS3.18 10.4.1.1.2 Table 10.4.1-2: Study Metadata is a Metadata
        // Resource, so /studies/{study}/metadata is a Target Resource of the
        // Retrieve Transaction (dcm4chee answers 200 for all three levels).
        if segs.len() == 4 && seg_eq(segs[3], "metadata") {
            return m1("wado_study_metadata", Access::Default, KeepAlive::On, study);
        }
        if segs.len() >= 5 && seg_eq(segs[3], "series") {
            let series = segs[4];
            if !uid_ok(series) {
                return RouterOutcome::BadUid;
            }
            if segs.len() == 5 {
                return m2("wado_series", Access::Default, KeepAlive::On, study, series);
            }
            if segs.len() == 6 {
                if seg_eq(segs[5], "instances") {
                    return m2(
                        "qido_series_instances",
                        Access::Default,
                        KeepAlive::On,
                        study,
                        series,
                    );
                }
                if seg_eq(segs[5], "metadata") {
                    return m2(
                        "wado_series_metadata",
                        Access::Default,
                        KeepAlive::On,
                        study,
                        series,
                    );
                }
                // Series-level `/rendered` is unmapped for the same reason as
                // the study level above (8.7.4-1 has no multi-frame
                // image/jpeg); only `thumbnail` is served here.
                if seg_eq(segs[5], "thumbnail") {
                    return m2(
                        "wado_series_thumbnail",
                        Access::Default,
                        KeepAlive::On,
                        study,
                        series,
                    );
                }
            }
            if segs.len() >= 7 && seg_eq(segs[5], "instances") {
                let sop = segs[6];
                if !uid_ok(sop) {
                    return RouterOutcome::BadUid;
                }
                if segs.len() == 7 {
                    return m3(
                        "wado_instance",
                        Access::Default,
                        KeepAlive::On,
                        study,
                        series,
                        sop,
                    );
                }
                if segs.len() == 8 {
                    if seg_eq(segs[7], "metadata") {
                        return m3(
                            "wado_metadata",
                            Access::Default,
                            KeepAlive::On,
                            study,
                            series,
                            sop,
                        );
                    }
                    if seg_eq(segs[7], "thumbnail") {
                        return m3(
                            "wado_thumbnail",
                            Access::Default,
                            KeepAlive::On,
                            study,
                            series,
                            sop,
                        );
                    }
                    if seg_eq(segs[7], "rendered") {
                        // 0410: instance-level `/rendered` is the Rendered
                        // Resource (PS3.18 8.7.9) - a separate handler that can
                        // answer image/png, unlike the JPEG-only Thumbnail
                        // Resource at `.../thumbnail` (8.7.8).
                        return m3(
                            "wado_rendered",
                            Access::Default,
                            KeepAlive::On,
                            study,
                            series,
                            sop,
                        );
                    }
                    if seg_eq(segs[7], "frames") {
                        return RouterOutcome::NotFound; // frames/frame is len 9
                    }
                }
                if segs.len() == 9 {
                    if seg_eq(segs[7], "frames") {
                        let frame = segs[8];
                        if !frame_ok(frame) {
                            return RouterOutcome::BadUid;
                        }
                        return m3(
                            "wado_frame",
                            Access::Default,
                            KeepAlive::On,
                            study,
                            series,
                            sop,
                        );
                    }
                    if seg_eq(segs[7], "bulkdata") {
                        return m3(
                            "wado_bulkdata",
                            Access::Default,
                            KeepAlive::On,
                            study,
                            series,
                            sop,
                        );
                    }
                }
                // C2 (B3 all-or-none, PS3.18 10.4.1.1.4): frame-level thumbnail
                // `.../instances/{sop}/frames/{n}/thumbnail` (len 10). A frame
                // is not an instance, so `/rendered` does not name one here.
                if segs.len() == 10 && seg_eq(segs[7], "frames") {
                    let frame = segs[8];
                    if !frame_ok(frame) {
                        return RouterOutcome::BadUid;
                    }
                    if seg_eq(segs[9], "thumbnail") {
                        return m3(
                            "wado_frame_thumbnail",
                            Access::Default,
                            KeepAlive::On,
                            study,
                            series,
                            sop,
                        );
                    }
                }
            }
        }
    }

    // /dicomweb/workitems/{uid}
    if segs.len() >= 3 && seg_eq(segs[0], "dicomweb") && seg_eq(segs[1], "workitems") {
        let wi = segs[2];
        if !uid_ok(wi) {
            return RouterOutcome::BadUid;
        }
        // PS3.18 11.3: GET /dicomweb/workitems/{uid} - retrieve one workitem.
        if segs.len() == 3 {
            return m1("ups_get", Access::Default, KeepAlive::On, wi);
        }
        // /dicomweb/workitems/{uid}/events
        if segs.len() == 4 && seg_eq(segs[3], "events") {
            return m1("ups_events", Access::Default, KeepAlive::On, wi);
        }
    }

    // PS3.18 15.6: GET /modality-performed-procedure-steps/{mppsUID} -
    // Retrieve (N-GET). OPTIONAL for an origin server, and one of the three
    // transactions of the MPPS service; the resource has no other GET.
    if segs.len() == 3
        && seg_eq(segs[0], "dicomweb")
        && seg_eq(segs[1], "modality-performed-procedure-steps")
    {
        let mpps = segs[2];
        if !uid_ok(mpps) {
            return RouterOutcome::BadUid;
        }
        return m1("mpps_get", Access::Default, KeepAlive::On, mpps);
    }

    // PS3.18 8.10.4: GET /subscribers/{requester} - the Notification
    // Connection resource. `{requester}` is the AE Title of the user agent
    // opening the connection (Table 8.10.4-1a), NOT a workitem or a
    // subscription SOP Class UID, so it is matched literally at its own path
    // and not nested under `/workitems`.
    //
    // The route is registered for GET only. A request without the WebSocket
    // upgrade headers reaches the handler and is answered there; that keeps
    // the "is this an upgrade?" decision in one place instead of splitting it
    // between the router and the handler.
    if segs.len() == 3 && seg_eq(segs[0], "dicomweb") && seg_eq(segs[1], "subscribers") {
        let requester = segs[2];
        if requester.is_empty() {
            return RouterOutcome::BadUid;
        }
        return m1(
            "ups_notification_connect",
            Access::Default,
            KeepAlive::On,
            requester,
        );
    }

    // PS3.18 13.5: GET /dicomweb/commitment-requests/{TransactionUID} -
    // Check Commit Result. The Transaction UID is the PATH segment (13.5.1.1)
    // and there are no query parameters (13.5.1.2); the handler enforces the
    // latter, since the router cannot see the query string.
    if segs.len() == 3 && seg_eq(segs[0], "dicomweb") && seg_eq(segs[1], "commitment-requests") {
        let txn = segs[2];
        if !uid_ok(txn) {
            return RouterOutcome::BadUid;
        }
        return m1("commitment_check", Access::Default, KeepAlive::On, txn);
    }

    if let Some(id) = match_admin_get(segs) {
        return m0(id, Access::Admin, KeepAlive::On);
    }
    RouterOutcome::NotFound
}

fn match_post<'a>(segs: &[&'a [u8]]) -> RouterOutcome<'a> {
    // PS3.18 13.4: POST /dicomweb/commitment-requests/{TransactionUID} -
    // Commit. Same resource as Check, distinguished by method (Table 13.3-1).
    if segs.len() == 3 && seg_eq(segs[0], "dicomweb") && seg_eq(segs[1], "commitment-requests") {
        let txn = segs[2];
        if !uid_ok(txn) {
            return RouterOutcome::BadUid;
        }
        return m1("commitment_commit", Access::Default, KeepAlive::On, txn);
    }
    // PS3.18 15.4 and 15.5: Create and Update share ONE method and ONE URI
    // shape (POST .../modality-performed-procedure-steps/{mppsUID}) and are
    // told apart by the `?update` query parameter (15.5: "the update
    // parameter ... distinguishes an Update Transaction from a Create
    // Transaction"). The router cannot see the query string, so - exactly as
    // UPS Create and Search do on `/workitems` - ONE route id covers both and
    // the dispatcher chooses.
    if segs.len() == 3
        && seg_eq(segs[0], "dicomweb")
        && seg_eq(segs[1], "modality-performed-procedure-steps")
    {
        let mpps = segs[2];
        if !uid_ok(mpps) {
            return RouterOutcome::BadUid;
        }
        return m1(
            "mpps_create_or_update",
            Access::Default,
            KeepAlive::On,
            mpps,
        );
    }
    if segs.len() == 2 && seg_eq(segs[0], "dicomweb") && seg_eq(segs[1], "studies") {
        return m0("stow_studies", Access::Default, KeepAlive::On);
    }
    // POST /dicomweb/studies/{study} - the second Store Transaction target
    // (PS3.18 10.5.1.1 Table 10.5.1-1 names `/studies/{study}` alongside the
    // root `/studies`). The study UID is a path segment, so it is captured as
    // the single route UID and the STOW core compares every part's Study
    // Instance UID against it.
    if segs.len() == 3 && seg_eq(segs[0], "dicomweb") && seg_eq(segs[1], "studies") {
        let study = segs[2];
        if !uid_ok(study) {
            return RouterOutcome::BadUid;
        }
        return m1("stow_study", Access::Default, KeepAlive::On, study);
    }
    // POST /dicomweb/workitems - UPS Create or UPS Search with a JSON body
    // (PS3.18 11.4 vs 11.9).
    //
    // Create (11.4.1.1 Table 11.4.1-1) targets `/workitems?{workitem}`: the
    // Workitem UID is a QUERY parameter and there is no path segment after
    // `workitems`. Search-with-body targets `/workitems?search`. The router
    // sees no query string, so ONE route id covers both and the dispatcher
    // tells them apart by which parameter is present - which is exactly the
    // distinction the standard draws, and the reason Create is not the
    // three-segment route it used to be.
    if segs.len() == 2 && seg_eq(segs[0], "dicomweb") && seg_eq(segs[1], "workitems") {
        return m0("ups_create", Access::Default, KeepAlive::On);
    }
    // POST /dicomweb/workitems/{workitem} - Update Workitem (PS3.18 11.6.1).
    // The Workitem UID is a PATH segment and the Transaction UID is the query
    // parameter, so this route is unambiguous against Create: a path segment
    // after `workitems` means the workitem resource, and a write to it is an
    // Update; no path segment means the worklist, and a write to it with a
    // `workitem` query parameter is a Create.
    //
    // PS3.18 11.8: POST /dicomweb/workitems/{workitem}/cancelrequest - Request
    // Cancellation, a four-segment route whose trailing segment names the
    // operation rather than a further identifier, so the Workitem UID stays at
    // index 2. The `?requester=` query parameter is optional and the router
    // does not see the query string; the handler reads it.
    if segs.len() == 4
        && seg_eq(segs[0], "dicomweb")
        && seg_eq(segs[1], "workitems")
        && seg_eq(segs[3], "cancelrequest")
    {
        let wi = segs[2];
        if !uid_ok(wi) {
            return RouterOutcome::BadUid;
        }
        return m1("ups_cancelrequest", Access::Default, KeepAlive::On, wi);
    }
    if segs.len() == 3 && seg_eq(segs[0], "dicomweb") && seg_eq(segs[1], "workitems") {
        let wi = segs[2];
        if !uid_ok(wi) {
            return RouterOutcome::BadUid;
        }
        return m1("ups_update_attributes", Access::Default, KeepAlive::On, wi);
    }
    // PS3.18 11.10/11.11: POST .../workitems/{subscription-sop-class-uid}/subscribers/{subscriber}
    // registers a subscriber. The segment after `workitems` IS the
    // subscription SOP Class UID (Table 11.1.1-1), so it is matched literally -
    // it names the subscription resource, not a workitem to validate. The
    // filtered variant differs only by the SOP Class UID (`.34.5.1`).
    if segs.len() == 5
        && seg_eq(segs[0], "dicomweb")
        && seg_eq(segs[1], "workitems")
        && seg_eq(segs[3], "subscribers")
    {
        let subscriber = segs[4];
        if subscriber.is_empty() {
            return RouterOutcome::BadUid;
        }
        if seg_eq(segs[2], WORKLIST_SUBSCRIPTION_UID) {
            return m1("ups_subscribe", Access::Default, KeepAlive::On, subscriber);
        }
        if seg_eq(segs[2], FILTERED_WORKLIST_SUBSCRIPTION_UID) {
            return m1(
                "ups_subscribe_filtered",
                Access::Default,
                KeepAlive::On,
                subscriber,
            );
        }
        // PS3.18 11.10.1.1 Table 11.10.1-1 requires the Workitem Subscription
        // template as well: the segment after `workitems` is then a real
        // workitem SOP Instance UID rather than a subscription SOP Class UID,
        // so the two are told apart by the literal match above and by the UID
        // check here. The subscriber still arrives as the last segment and the
        // workitem is captured first, which is the order the handler expects.
        if uid_ok(segs[2]) {
            return m2(
                "ups_subscribe_workitem",
                Access::Default,
                KeepAlive::On,
                segs[2],
                subscriber,
            );
        }
    }
    if let Some(id) = match_admin_post(segs) {
        return m0(id, Access::Admin, KeepAlive::On);
    }
    RouterOutcome::NotFound
}

fn match_put<'a>(segs: &[&'a [u8]]) -> RouterOutcome<'a> {
    if segs.len() == 3 && seg_eq(segs[0], "dicomweb") && seg_eq(segs[1], "workitems") {
        let wi = segs[2];
        if !uid_ok(wi) {
            return RouterOutcome::BadUid;
        }
        return m1("ups_update", Access::Default, KeepAlive::On, wi);
    }
    // PS3.18 11.2 names the state-change resource explicitly:
    // PUT /dicomweb/workitems/{uid}/state. It is the same operation as the
    // bare PUT on the workitem and shares its handler; the suffix only makes
    // the target of the change unambiguous. The workitem UID stays at index
    // 2, exactly as on the bare route, because the trailing segment names
    // the resource being changed rather than a further identifier.
    if segs.len() == 4
        && seg_eq(segs[0], "dicomweb")
        && seg_eq(segs[1], "workitems")
        && seg_eq(segs[3], "state")
    {
        let wi = segs[2];
        if !uid_ok(wi) {
            return RouterOutcome::BadUid;
        }
        return m1("ups_update", Access::Default, KeepAlive::On, wi);
    }
    RouterOutcome::NotFound
}

fn match_delete<'a>(segs: &[&'a [u8]]) -> RouterOutcome<'a> {
    if segs.len() == 3 && seg_eq(segs[0], "dicomweb") && seg_eq(segs[1], "workitems") {
        let wi = segs[2];
        if !uid_ok(wi) {
            return RouterOutcome::BadUid;
        }
        return m1("ups_delete", Access::Default, KeepAlive::On, wi);
    }
    // PS3.18 11.10/11.11: DELETE .../subscribers/{subscriber} unsubscribes.
    if segs.len() == 5
        && seg_eq(segs[0], "dicomweb")
        && seg_eq(segs[1], "workitems")
        && seg_eq(segs[3], "subscribers")
    {
        let subscriber = segs[4];
        if subscriber.is_empty() {
            return RouterOutcome::BadUid;
        }
        if seg_eq(segs[2], WORKLIST_SUBSCRIPTION_UID)
            || seg_eq(segs[2], FILTERED_WORKLIST_SUBSCRIPTION_UID)
        {
            return m1(
                "ups_unsubscribe",
                Access::Default,
                KeepAlive::On,
                subscriber,
            );
        }
        // The Workitem Subscription template (PS3.18 11.10.1.1) unsubscribes
        // under the same path shape, with a workitem UID in place of the
        // subscription SOP Class UID.
        if uid_ok(segs[2]) {
            return m2(
                "ups_unsubscribe_workitem",
                Access::Default,
                KeepAlive::On,
                segs[2],
                subscriber,
            );
        }
    }
    RouterOutcome::NotFound
}

fn seg_eq(seg: &[u8], lit: &str) -> bool {
    seg == lit.as_bytes()
}

fn m0<'a>(id: &'static str, access: Access, ka: KeepAlive) -> RouterOutcome<'a> {
    RouterOutcome::Match(RouteMatch {
        id,
        access,
        keep_alive: ka,
        uids: [&[], &[], &[]],
        n_uids: 0,
        qa: false,
    })
}

fn m1<'a>(id: &'static str, access: Access, ka: KeepAlive, a: &'a [u8]) -> RouterOutcome<'a> {
    RouterOutcome::Match(RouteMatch {
        id,
        access,
        keep_alive: ka,
        uids: [a, &[], &[]],
        n_uids: 1,
        qa: false,
    })
}

fn m2<'a>(
    id: &'static str,
    access: Access,
    ka: KeepAlive,
    a: &'a [u8],
    b: &'a [u8],
) -> RouterOutcome<'a> {
    RouterOutcome::Match(RouteMatch {
        id,
        access,
        keep_alive: ka,
        uids: [a, b, &[]],
        n_uids: 2,
        qa: false,
    })
}

fn m3<'a>(
    id: &'static str,
    access: Access,
    ka: KeepAlive,
    a: &'a [u8],
    b: &'a [u8],
    c: &'a [u8],
) -> RouterOutcome<'a> {
    RouterOutcome::Match(RouteMatch {
        id,
        access,
        keep_alive: ka,
        uids: [a, b, c],
        n_uids: 3,
        qa: false,
    })
}

/// /admin/v1/... GET routes. Returns the route id on a match.
fn match_admin_get(segs: &[&[u8]]) -> Option<&'static str> {
    if segs.len() < 2 || !seg_eq(segs[0], "admin") || !seg_eq(segs[1], "v1") {
        return None;
    }
    let n = segs.len();
    if n == 3 {
        let a = segs[2];
        return if seg_eq(a, "status") {
            Some("admin_status")
        } else if seg_eq(a, "health") {
            Some("admin_health")
        } else if seg_eq(a, "version") {
            Some("admin_version")
        } else if seg_eq(a, "dashboard") {
            Some("admin_dashboard")
        } else if seg_eq(a, "audit-log") {
            Some("admin_audit_log")
        } else if seg_eq(a, "access-log") {
            Some("admin_access_log")
        } else if seg_eq(a, "storage") {
            Some("admin_storage")
        } else {
            None
        };
    }
    if n == 4 {
        let a = segs[2];
        let b = segs[3];
        if seg_eq(a, "logs") && seg_eq(b, "stream") {
            return Some("admin_logs_stream");
        }
        if seg_eq(a, "metrics") && seg_eq(b, "history") {
            return Some("admin_metrics_history");
        }
        if seg_eq(a, "instances") && seg_eq(b, "search") {
            return Some("admin_instances_search");
        }
        // IOCM recovery surface (Clarus extension, admin-CIDR-gated):
        // list what a rejection note sequestered, and read its retained
        // bytes. The UID travels as a query parameter rather than a path
        // segment because the admin GET matcher has no dynamic segment.
        if seg_eq(a, "instances") && seg_eq(b, "hidden") {
            return Some("admin_instances_hidden");
        }
        if seg_eq(a, "instances") && seg_eq(b, "raw") {
            return Some("admin_instances_raw");
        }
        if seg_eq(a, "storage") && seg_eq(b, "s3-validation") {
            return Some("admin_storage_s3");
        }
        if seg_eq(a, "storage") && seg_eq(b, "audit-copies") {
            return Some("admin_audit_copies");
        }
        return None;
    }
    if n == 5 {
        let a = segs[2];
        let b = segs[3];
        let c = segs[4];
        if seg_eq(a, "storage") && seg_eq(b, "cas-audit") && seg_eq(c, "status") {
            return Some("admin_cas_audit_status");
        }
        // GET /admin/v1/recovery/rebuild/status.
        if seg_eq(a, "recovery") && seg_eq(b, "rebuild") && seg_eq(c, "status") {
            return Some("admin_recovery_rebuild_status");
        }
        return None;
    }
    None
}

/// /admin/v1/... POST routes (all take `segs` already split).
fn match_admin_post(segs: &[&[u8]]) -> Option<&'static str> {
    if segs.len() < 2 || !seg_eq(segs[0], "admin") || !seg_eq(segs[1], "v1") {
        return None;
    }
    let n = segs.len();
    if n == 3 && seg_eq(segs[2], "maintenance") {
        return Some("admin_maintenance");
    }
    if n == 4 {
        let a = segs[2];
        let b = segs[3];
        if seg_eq(a, "storage") && seg_eq(b, "migrate") {
            return Some("admin_storage_migrate");
        }
        if seg_eq(a, "storage") && seg_eq(b, "cas-audit") {
            return Some("admin_cas_audit");
        }
        if seg_eq(a, "instances") && seg_eq(b, "delete") {
            return Some("admin_instances_delete");
        }
        // Restore clears a sequester that no standard surface can undo; it is
        // a Clarus extension, not a DICOM/IHE transaction.
        if seg_eq(a, "instances") && seg_eq(b, "restore") {
            return Some("admin_instances_restore");
        }
        // POST /admin/v1/recovery/rebuild (cas->manifests rebuild gap).
        if seg_eq(a, "recovery") && seg_eq(b, "rebuild") {
            return Some("admin_recovery_rebuild");
        }
        return None;
    }
    if n == 5
        && seg_eq(segs[2], "storage")
        && seg_eq(segs[3], "cas-audit")
        && seg_eq(segs[4], "start")
    {
        return Some("admin_cas_audit");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_routes_like_get() {
        match match_route(Method::Head, b"/dicomweb/studies") {
            RouterOutcome::Match(m) => assert_eq!(m.id, "qido_studies"),
            other => panic!("expected qido_studies, got {other:?}"),
        }
        // HEAD on a path with no GET route mirrors GET: a plain 404.
        assert!(matches!(
            match_route(Method::Head, b"/no/such/route"),
            RouterOutcome::NotFound
        ));
    }

    #[test]
    fn patients_uses_admin_policy() {
        match match_route(Method::Get, b"/dicomweb/patients") {
            RouterOutcome::Match(m) => {
                assert_eq!(m.access, Access::Admin);
                assert_eq!(m.keep_alive, KeepAlive::On);
                assert_eq!(m.id, "qido_patients");
            }
            other => panic!("expected match, got {other:?}"),
        }
    }

    #[test]
    fn admin_routes_use_admin_policy() {
        for (method, path) in [
            (Method::Get, &b"/admin/v1/status"[..]),
            (Method::Get, b"/admin/v1/logs/stream"),
            (Method::Post, b"/admin/v1/maintenance"),
            (Method::Post, b"/admin/v1/recovery/rebuild"),
        ] {
            match match_route(method, path) {
                RouterOutcome::Match(m) => assert_eq!(
                    m.access,
                    Access::Admin,
                    "{path:?} must use the admin policy"
                ),
                other => panic!("expected match for {path:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn wado_instance_matches_with_three_uids() {
        match match_route(
            Method::Get,
            b"/dicomweb/studies/1.2.3/series/4.5/instances/6.7",
        ) {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "wado_instance");
                assert_eq!(m.keep_alive, KeepAlive::On);
                assert_eq!(m.n_uids, 3);
                assert_eq!(m.uids[0], b"1.2.3");
                assert_eq!(m.uids[1], b"4.5");
                assert_eq!(m.uids[2], b"6.7");
            }
            other => panic!("expected match, got {other:?}"),
        }
    }

    #[test]
    fn bad_uid_rejected() {
        assert!(matches!(
            match_route(Method::Get, b"/dicomweb/studies/bad!uid/x"),
            RouterOutcome::BadUid
        ));
    }

    #[test]
    fn dots_only_uid_is_rejected() {
        // A UID segment of only `.` characters is not a UID. It is what a
        // traversal probe sends, and before this gate `..` was accepted as a
        // valid segment and answered as an empty result set instead of 400.
        for path in [
            &b"/dicomweb/studies/../series"[..],
            b"/dicomweb/studies/./series",
            b"/dicomweb/studies/.../series",
            b"/dicomweb/studies/../series/../instances",
            b"/dicomweb/studies/1.2.3/series/../instances",
            b"/dicomweb/studies/../metadata",
        ] {
            assert!(
                matches!(match_route(Method::Get, path), RouterOutcome::BadUid),
                "{path:?} must be rejected as a bad uid"
            );
        }
        // The unit itself, including the boundary shapes.
        assert!(!uid_ok(b"."));
        assert!(!uid_ok(b".."));
        assert!(!uid_ok(b"..."));
        // A single dot inside a real UID stays legal, as does a trailing dot
        // in a digits-bearing segment (the digits rule is what matters).
        assert!(uid_ok(b"1.2.3"));
        assert!(uid_ok(b"1.2.3."));
        assert!(uid_ok(b".1.2.3"));
        assert!(!uid_ok(b""));
    }

    #[test]
    fn empty_uid_segment_still_rejected() {
        // `/studies//series` - the empty segment between the slashes must not
        // become a wildcard.
        assert!(matches!(
            match_route(Method::Get, b"/dicomweb/studies//series"),
            RouterOutcome::BadUid
        ));
    }

    #[test]
    fn series_search_matches_with_study_uid() {
        match match_route(Method::Get, b"/dicomweb/studies/1.2.3/series") {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "qido_series");
                assert_eq!(m.keep_alive, KeepAlive::On);
                assert_eq!(m.access, Access::Default);
                assert_eq!(m.n_uids, 1);
                assert_eq!(m.uids[0], b"1.2.3");
            }
            other => panic!("expected qido_series match, got {other:?}"),
        }
    }

    #[test]
    fn unknown_method_gives_405_allow() {
        assert!(matches!(
            match_route(Method::Unknown, b"/dicomweb/studies"),
            RouterOutcome::MethodNotAllowed { .. }
        ));
    }

    #[test]
    fn options_gives_cors_preflight_outcome() {
        // Any path, even one that does not exist: preflight is answered
        // before routing (browser probes the server, not the resource).
        assert!(matches!(
            match_route(Method::Options, b"/dicomweb/studies"),
            RouterOutcome::Options
        ));
        assert!(matches!(
            match_route(Method::Options, b"/no/such/path"),
            RouterOutcome::Options
        ));
    }

    #[test]
    fn options_on_the_base_uri_is_the_capabilities_transaction() {
        // PS3.18 8.9.1: OPTIONS on the Base URI is the Retrieve Capabilities
        // Transaction, not a CORS preflight. Both spellings route.
        for path in [&b"/dicomweb"[..], &b"/dicomweb/"[..]] {
            match match_route(Method::Options, path) {
                RouterOutcome::Match(m) => {
                    assert_eq!(m.id, "capabilities");
                    assert_eq!(m.access, Access::Any);
                }
                other => panic!("expected capabilities match for {path:?}, got {other:?}"),
            }
        }
    }

    /// PS3.18 8.7.8/8.7.9: `/rendered` is implemented at the INSTANCE level
    /// only. At the study/series/frame level the name used to resolve to the
    /// thumbnail handler, which served the thumbnail bytes under the rendered
    /// name - a MANDATORY deviation (10.4.3.3.3 wants a rendering of all
    /// instances, which 8.7.4-1 gives no image/jpeg for). Those names are now
    /// honest 404s; the instance-level resource is unchanged.
    #[test]
    fn rendered_is_an_instance_level_resource_only() {
        match match_route(
            Method::Get,
            b"/dicomweb/studies/1.2.840.1.2/series/3.4.5.6/instances/7.8.9/rendered",
        ) {
            RouterOutcome::Match(m) => assert_eq!(m.id, "wado_rendered"),
            other => panic!("expected wado_rendered, got {other:?}"),
        }
        for path in [
            &b"/dicomweb/studies/1.2.840.1.2/rendered"[..],
            b"/dicomweb/studies/1.2.840.1.2/series/3.4.5.6/rendered",
            b"/dicomweb/studies/1.2.840.1.2/series/3.4.5.6/instances/7.8.9/frames/2/rendered",
        ] {
            assert!(
                matches!(match_route(Method::Get, path), RouterOutcome::NotFound),
                "{path:?} must not resolve to the thumbnail handler"
            );
        }
        // The thumbnail names stay exactly as they were at all three levels.
        match match_route(Method::Get, b"/dicomweb/studies/1.2.840.1.2/thumbnail") {
            RouterOutcome::Match(m) => assert_eq!(m.id, "wado_study_thumbnail"),
            other => panic!("expected wado_study_thumbnail, got {other:?}"),
        }
        match match_route(
            Method::Get,
            b"/dicomweb/studies/1.2.840.1.2/series/3.4.5.6/thumbnail",
        ) {
            RouterOutcome::Match(m) => assert_eq!(m.id, "wado_series_thumbnail"),
            other => panic!("expected wado_series_thumbnail, got {other:?}"),
        }
        match match_route(
            Method::Get,
            b"/dicomweb/studies/1.2.840.1.2/series/3.4.5.6/instances/7.8.9/frames/2/thumbnail",
        ) {
            RouterOutcome::Match(m) => assert_eq!(m.id, "wado_frame_thumbnail"),
            other => panic!("expected wado_frame_thumbnail, got {other:?}"),
        }
    }

    #[test]
    fn global_series_matches_top_level() {
        match match_route(Method::Get, b"/dicomweb/series") {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "qido_series_global");
                assert_eq!(m.keep_alive, KeepAlive::On);
                assert_eq!(m.access, Access::Default);
                assert_eq!(m.n_uids, 0);
            }
            other => panic!("expected qido_series_global match, got {other:?}"),
        }
    }

    #[test]
    fn series_root_instances_matches_weasis_compat() {
        // 06.09 field incident: Weasis appends "/instances" to the QIDO
        // series RetrieveURL (series-root form from the global search).
        match match_route(Method::Get, b"/dicomweb/series/1.2.840.1.2/instances") {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "qido_series_root_instances");
                assert_eq!(m.keep_alive, KeepAlive::On);
                assert_eq!(m.access, Access::Default);
                assert_eq!(m.n_uids, 1);
                assert_eq!(m.uids[0], b"1.2.840.1.2");
            }
            other => panic!("expected qido_series_root_instances match, got {other:?}"),
        }
        assert!(matches!(
            match_route(Method::Get, b"/dicomweb/series/bad!uid/instances"),
            RouterOutcome::BadUid
        ));
    }

    /// PS3.18 10.4.1.1.2 Table 10.4.1-2: Study Metadata is a Metadata Resource,
    /// so `/studies/{study}/metadata` is a four-segment Target Resource (the
    /// study is the only captured UID). dcm4chee answers 200 for it.
    #[test]
    fn study_metadata_matches_the_study_resource() {
        match match_route(Method::Get, b"/dicomweb/studies/1.2.840.1.2/metadata") {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "wado_study_metadata");
                assert_eq!(m.keep_alive, KeepAlive::On);
                assert_eq!(m.access, Access::Default);
                assert_eq!(m.n_uids, 1);
                assert_eq!(m.uids[0], b"1.2.840.1.2");
            }
            other => panic!("expected wado_study_metadata match, got {other:?}"),
        }
        assert!(matches!(
            match_route(Method::Get, b"/dicomweb/studies/bad!uid/metadata"),
            RouterOutcome::BadUid
        ));
    }

    #[test]
    fn global_instances_matches_top_level() {
        match match_route(Method::Get, b"/dicomweb/instances") {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "qido_instances_global");
                assert_eq!(m.keep_alive, KeepAlive::On);
                assert_eq!(m.access, Access::Default);
                assert_eq!(m.n_uids, 0);
            }
            other => panic!("expected qido_instances_global match, got {other:?}"),
        }
    }

    #[test]
    fn health_and_metrics_are_public() {
        match match_route(Method::Get, b"/health") {
            RouterOutcome::Match(m) => {
                assert_eq!(m.access, Access::Any);
                assert_eq!(m.id, "health");
            }
            other => panic!("expected health match, got {other:?}"),
        }
    }

    #[test]
    fn recovery_rebuild_post_matches() {
        // POST /admin/v1/recovery/rebuild.
        match match_route(Method::Post, b"/admin/v1/recovery/rebuild") {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "admin_recovery_rebuild");
            }
            other => panic!("expected admin_recovery_rebuild match, got {other:?}"),
        }
    }

    #[test]
    fn recovery_rebuild_status_get_matches() {
        // GET /admin/v1/recovery/rebuild/status.
        match match_route(Method::Get, b"/admin/v1/recovery/rebuild/status") {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "admin_recovery_rebuild_status");
            }
            other => panic!("expected admin_recovery_rebuild_status match, got {other:?}"),
        }
    }

    /// PS3.18 Table 11.1.1-1: the subscription SOP Class UID is the URI segment
    /// right after `workitems`, so POST registers either flavour.
    #[test]
    fn worklist_subscription_post_matches() {
        match match_route(
            Method::Post,
            b"/dicomweb/workitems/1.2.840.10008.5.1.4.34.5/subscribers/CLARUS",
        ) {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "ups_subscribe");
                assert_eq!(m.uids[0], b"CLARUS");
            }
            other => panic!("expected ups_subscribe match, got {other:?}"),
        }
    }

    #[test]
    fn filtered_worklist_subscription_post_matches() {
        match match_route(
            Method::Post,
            b"/dicomweb/workitems/1.2.840.10008.5.1.4.34.5.1/subscribers/CLARUS",
        ) {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "ups_subscribe_filtered");
                assert_eq!(m.uids[0], b"CLARUS");
            }
            other => panic!("expected ups_subscribe_filtered match, got {other:?}"),
        }
    }

    /// DELETE is the unsubscribe verb for both subscription flavours.
    #[test]
    fn worklist_subscription_delete_matches() {
        match match_route(
            Method::Delete,
            b"/dicomweb/workitems/1.2.840.10008.5.1.4.34.5/subscribers/CLARUS",
        ) {
            RouterOutcome::Match(m) => assert_eq!(m.id, "ups_unsubscribe"),
            other => panic!("expected ups_unsubscribe match, got {other:?}"),
        }
        match match_route(
            Method::Delete,
            b"/dicomweb/workitems/1.2.840.10008.5.1.4.34.5.1/subscribers/CLARUS",
        ) {
            RouterOutcome::Match(m) => assert_eq!(m.id, "ups_unsubscribe"),
            other => panic!("expected ups_unsubscribe match, got {other:?}"),
        }
    }

    /// A segment in the subscription position that is NOT a UID is not a
    /// subscription: the route must not match.
    ///
    /// An UNKNOWN but well-formed UID does match now - PS3.18 11.10.1.1's
    /// Workitem Subscription template puts a real workitem UID there, so
    /// whether it names a workitem is the handler's question, not the
    /// router's (see `workitem_subscription_post_matches`).
    #[test]
    fn non_uid_segment_in_the_subscription_position_does_not_match() {
        assert!(matches!(
            match_route(
                Method::Post,
                b"/dicomweb/workitems/not-a-uid/subscribers/CLARUS"
            ),
            RouterOutcome::NotFound
        ));
    }

    /// PS3.18 11.10.1.1 Table 11.10.1-1: the Workitem Subscription template
    /// puts a real workitem SOP Instance UID after `workitems`. The router
    /// captures the workitem FIRST and the subscriber second - the opposite
    /// order from the Worklist forms, which capture only the subscriber.
    #[test]
    fn workitem_subscription_post_matches() {
        match match_route(
            Method::Post,
            b"/dicomweb/workitems/1.2.3.4.5/subscribers/CLARUS",
        ) {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "ups_subscribe_workitem");
                assert_eq!(m.n_uids, 2);
                assert_eq!(m.uids[0], b"1.2.3.4.5", "the workitem comes first");
                assert_eq!(m.uids[1], b"CLARUS", "the subscriber second");
            }
            other => panic!("expected ups_subscribe_workitem match, got {other:?}"),
        }
    }

    /// The same path with DELETE unsubscribes the workitem-scoped form.
    #[test]
    fn workitem_subscription_delete_matches() {
        match match_route(
            Method::Delete,
            b"/dicomweb/workitems/1.2.3.4.5/subscribers/CLARUS",
        ) {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "ups_unsubscribe_workitem");
                assert_eq!(m.n_uids, 2);
                assert_eq!(m.uids[0], b"1.2.3.4.5");
                assert_eq!(m.uids[1], b"CLARUS");
            }
            other => panic!("expected ups_unsubscribe_workitem match, got {other:?}"),
        }
    }

    /// The two subscription SOP Class UIDs must keep winning over the generic
    /// workitem form: they are themselves well-formed UIDs, so a matcher that
    /// checked `uid_ok` first would swallow them both.
    #[test]
    fn subscription_sop_class_uids_beat_the_workitem_form() {
        for (path, expected) in [
            (
                &b"/dicomweb/workitems/1.2.840.10008.5.1.4.34.5/subscribers/CLARUS"[..],
                "ups_subscribe",
            ),
            (
                &b"/dicomweb/workitems/1.2.840.10008.5.1.4.34.5.1/subscribers/CLARUS"[..],
                "ups_subscribe_filtered",
            ),
        ] {
            match match_route(Method::Post, path) {
                RouterOutcome::Match(m) => {
                    assert_eq!(m.id, expected);
                    assert_eq!(m.n_uids, 1, "the Worklist forms capture one UID");
                }
                other => panic!("expected {expected}, got {other:?}"),
            }
        }
    }

    /// A subscription path with no `{subscriber}` segment must never resolve to
    /// a subscription route. The segment splitter drops empty segments, so the
    /// trailing slash shortens the path and it falls through to the ordinary
    /// workitem matcher instead. Only the absence of a subscription match is
    /// guaranteed here.
    #[test]
    fn missing_subscriber_segment_is_not_a_subscription_route() {
        let outcome = match_route(
            Method::Post,
            b"/dicomweb/workitems/1.2.840.10008.5.1.4.34.5/subscribers/",
        );
        assert!(
            matches!(outcome, RouterOutcome::NotFound)
                || matches!(outcome, RouterOutcome::Match(m)
                    if m.id != "ups_subscribe" && m.id != "ups_subscribe_filtered"),
            "unexpected outcome: {outcome:?}"
        );
    }

    /// The state-change resource is named two ways and both must reach the
    /// same handler: the bare workitem PUT, and the `/state` suffix PS3.18
    /// 11.2 defines. The workitem UID is index 2 on both, because the
    /// trailing segment names the resource rather than another identifier.
    #[test]
    fn workitem_state_change_matches_on_both_forms() {
        for path in [
            &b"/dicomweb/workitems/1.2.3"[..],
            b"/dicomweb/workitems/1.2.3/state",
        ] {
            match match_route(Method::Put, path) {
                RouterOutcome::Match(m) => {
                    assert_eq!(m.id, "ups_update", "{path:?}");
                    assert_eq!(m.access, Access::Default, "{path:?}");
                    assert_eq!(m.keep_alive, KeepAlive::On, "{path:?}");
                    assert_eq!(m.n_uids, 1, "{path:?}");
                    assert_eq!(m.uids[0], b"1.2.3", "{path:?}");
                }
                other => panic!("expected a match for {path:?}, got {other:?}"),
            }
        }
    }

    /// The `/state` suffix is only meaningful on a workitem path. A suffix
    /// under a different resource, or on an otherwise unmatched shape, must
    /// not be answered as a state change.
    #[test]
    fn state_suffix_under_another_resource_does_not_match() {
        for path in [
            &b"/dicomweb/studies/1.2.3/state"[..],
            b"/dicomweb/workitems/1.2.3/state/extra",
            b"/dicomweb/workitems/1.2.3/states",
        ] {
            assert!(
                matches!(match_route(Method::Put, path), RouterOutcome::NotFound),
                "{path:?} must not resolve to a state change"
            );
        }
    }

    /// The UID gate applies to the `/state` form as it does to the bare one.
    #[test]
    fn state_suffix_with_a_bad_uid_is_rejected() {
        assert!(matches!(
            match_route(Method::Put, b"/dicomweb/workitems/../state"),
            RouterOutcome::BadUid
        ));
    }

    /// PS3.18 11.4.1.1 vs 11.6.1: Create and Update share the POST method on
    /// the workitem family and are told apart by WHERE the Workitem UID is.
    ///
    /// Create targets `/workitems?workitem={uid}` (the UID is a query
    /// parameter, no path segment), Update targets `/workitems/{uid}` (the
    /// UID is a path segment). The router sees no query string, so it maps
    /// the two shapes to two different ids and the dispatcher picks between
    /// them - that split is what this pins.
    #[test]
    fn create_and_update_are_told_apart_by_the_uid_placement() {
        // No path segment: the worklist resource, which carries Create.
        match match_route(Method::Post, b"/dicomweb/workitems") {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "ups_create", "the worklist resource is Create");
                assert_eq!(m.n_uids, 0, "no path UID on the worklist resource");
            }
            other => panic!("expected a match for the worklist resource, got {other:?}"),
        }

        // A path segment: the workitem resource, which carries Update.
        match match_route(Method::Post, b"/dicomweb/workitems/1.2.3") {
            RouterOutcome::Match(m) => {
                assert_eq!(
                    m.id, "ups_update_attributes",
                    "the workitem resource is Update"
                );
                assert_eq!(m.access, Access::Default);
                assert_eq!(m.keep_alive, KeepAlive::On);
                assert_eq!(m.n_uids, 1);
                assert_eq!(m.uids[0], b"1.2.3");
            }
            other => panic!("expected a match for the workitem resource, got {other:?}"),
        }
    }

    /// The Update route validates the workitem UID like every other
    /// workitem-scoped route.
    #[test]
    fn update_with_a_bad_uid_is_rejected() {
        assert!(matches!(
            match_route(Method::Post, b"/dicomweb/workitems/.."),
            RouterOutcome::BadUid
        ));
    }

    // -- Storage Commitment (PS3.18 Section 13) --------------------------

    /// Both transactions share one resource path and are told apart by method
    /// (Table 13.3-1): POST is Commit, GET is Check Commit Result.
    #[test]
    fn commitment_requests_match_by_method() {
        let path = &b"/dicomweb/commitment-requests/1.2.3"[..];

        match match_route(Method::Post, path) {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "commitment_commit");
                assert_eq!(m.access, Access::Default);
                assert_eq!(m.n_uids, 1);
                assert_eq!(m.uids[0], b"1.2.3");
            }
            other => panic!("expected a Commit match, got {other:?}"),
        }

        match match_route(Method::Get, path) {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "commitment_check");
                assert_eq!(m.access, Access::Default);
                assert_eq!(m.uids[0], b"1.2.3");
            }
            other => panic!("expected a Check match, got {other:?}"),
        }
    }

    /// The collection path names no transaction, so it is not a resource: the
    /// standard puts both transactions on `{TransactionUID}` (13.3).
    #[test]
    fn the_commitment_collection_path_is_not_a_transaction() {
        let path = &b"/dicomweb/commitment-requests"[..];
        assert!(matches!(
            match_route(Method::Get, path),
            RouterOutcome::NotFound
        ));
        assert!(matches!(
            match_route(Method::Post, path),
            RouterOutcome::NotFound
        ));
    }

    // -- Modality Scheduled Procedure Step (PS3.18 Section 14) ------------

    /// PS3.18 14.4: Search is a GET on the collection resource. The service
    /// is search-only (14.3), so this is its one and only route.
    #[test]
    fn mwl_search_matches_the_collection() {
        match match_route(Method::Get, b"/dicomweb/modality-scheduled-procedure-steps") {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "mwl_search");
                assert_eq!(m.access, Access::Default);
                assert_eq!(m.keep_alive, KeepAlive::On);
                assert_eq!(m.n_uids, 0, "the collection carries no UID segment");
            }
            other => panic!("expected mwl_search match, got {other:?}"),
        }
    }

    /// 14.3: "it is not possible to create Modality Scheduled Procedure Steps
    /// using DICOM, neither with DIMSE, nor with DICOMweb", so a write to the
    /// collection must not resolve to anything.
    #[test]
    fn mwl_has_no_create_route() {
        for method in [Method::Post, Method::Put, Method::Delete] {
            assert!(
                matches!(
                    match_route(method, b"/dicomweb/modality-scheduled-procedure-steps"),
                    RouterOutcome::NotFound
                ),
                "{method:?} must not create an MWL step"
            );
        }
    }

    // -- Modality Performed Procedure Step (PS3.18 Section 15) -----------

    /// 15.4/15.5: Create and Update are the SAME method and path shape, told
    /// apart only by `?update`, which the router cannot see - so both must map
    /// to the one id the dispatcher then splits.
    #[test]
    fn mpps_create_and_update_share_one_route_id() {
        match match_route(
            Method::Post,
            b"/dicomweb/modality-performed-procedure-steps/1.2.3",
        ) {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "mpps_create_or_update");
                assert_eq!(m.access, Access::Default);
                assert_eq!(m.n_uids, 1);
                assert_eq!(m.uids[0], b"1.2.3");
            }
            other => panic!("expected mpps_create_or_update match, got {other:?}"),
        }
    }

    /// 15.6: Retrieve is a GET on `{mppsUID}` (N-GET). OPTIONAL for the origin
    /// server, and implemented here.
    #[test]
    fn mpps_retrieve_matches_the_instance() {
        match match_route(
            Method::Get,
            b"/dicomweb/modality-performed-procedure-steps/1.2.3",
        ) {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "mpps_get");
                assert_eq!(m.access, Access::Default);
                assert_eq!(m.n_uids, 1);
                assert_eq!(m.uids[0], b"1.2.3");
            }
            other => panic!("expected mpps_get match, got {other:?}"),
        }
    }

    /// 15.3: "it is not possible to list all Modality Performed Procedure
    /// Steps using DICOM, neither with DIMSE, nor with DICOMweb" - the
    /// collection path names no transaction.
    #[test]
    fn the_mpps_collection_path_is_not_a_resource() {
        let path = &b"/dicomweb/modality-performed-procedure-steps"[..];
        assert!(matches!(
            match_route(Method::Get, path),
            RouterOutcome::NotFound
        ));
        assert!(matches!(
            match_route(Method::Post, path),
            RouterOutcome::NotFound
        ));
    }

    /// The MPPS UID goes through the same UID gate as every other resource:
    /// a traversal probe is a 400, not a miss.
    #[test]
    fn mpps_bad_uid_is_rejected() {
        assert!(matches!(
            match_route(
                Method::Post,
                b"/dicomweb/modality-performed-procedure-steps/.."
            ),
            RouterOutcome::BadUid
        ));
        assert!(matches!(
            match_route(
                Method::Get,
                b"/dicomweb/modality-performed-procedure-steps/not-a-uid"
            ),
            RouterOutcome::BadUid
        ));
    }

    /// PS3.18 10.5.1.1 Table 10.5.1-1: `/studies/{study}` is the second
    /// target of the Store Transaction, alongside the root `/studies`. The
    /// study UID is captured as the route's single UID so the STOW core can
    /// compare every part against it.
    #[test]
    fn study_scoped_stow_matches_and_captures_the_study() {
        match match_route(Method::Post, b"/dicomweb/studies/1.2.3") {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "stow_study");
                assert_eq!(m.access, Access::Default);
                assert_eq!(m.n_uids, 1);
                assert_eq!(m.uids[0], b"1.2.3");
            }
            other => panic!("expected stow_study match, got {other:?}"),
        }
    }

    /// The root Store Transaction target still resolves to `stow_studies`
    /// with no captured UID - the new three-segment branch must not shadow it.
    #[test]
    fn root_stow_still_matches_without_a_uid() {
        match match_route(Method::Post, b"/dicomweb/studies") {
            RouterOutcome::Match(m) => {
                assert_eq!(m.id, "stow_studies");
                assert_eq!(m.n_uids, 0);
            }
            other => panic!("expected stow_studies match, got {other:?}"),
        }
    }

    /// The study segment goes through the same UID gate as every other
    /// resource: a traversal probe is a 400, not a miss.
    #[test]
    fn study_scoped_stow_bad_uid_is_rejected() {
        assert!(matches!(
            match_route(Method::Post, b"/dicomweb/studies/.."),
            RouterOutcome::BadUid
        ));
        assert!(matches!(
            match_route(Method::Post, b"/dicomweb/studies/not-a-uid"),
            RouterOutcome::BadUid
        ));
    }

    /// A malformed Transaction UID is refused by the router's UID gate.
    ///
    /// The gate is the same one every other DICOMweb resource uses
    /// (`uid_ok`): digits and dots, at least one digit, at most 64 bytes.
    #[test]
    fn a_bad_commitment_transaction_uid_is_rejected() {
        for path in [
            &b"/dicomweb/commitment-requests/abc"[..],
            b"/dicomweb/commitment-requests/1.2.3x",
            b"/dicomweb/commitment-requests/..",
        ] {
            assert!(
                matches!(match_route(Method::Get, path), RouterOutcome::BadUid),
                "{path:?} must be a bad UID"
            );
        }
    }

    /// There is no deeper resource under a commitment request.
    #[test]
    fn a_suffix_under_a_commitment_request_does_not_match() {
        for path in [
            &b"/dicomweb/commitment-requests/1.2.3/state"[..],
            b"/dicomweb/commitment-requests/1.2.3/extra",
        ] {
            assert!(
                matches!(match_route(Method::Get, path), RouterOutcome::NotFound),
                "{path:?} must not resolve"
            );
        }
    }
}
