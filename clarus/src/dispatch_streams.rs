// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Synchronous streaming transport handlers.
//!
//! These handlers ARE the production transport: they turn a matched route
//! into bytes on the wire using pure core functions. The axum/framework server
//! is not part of this layer - the ROUTES table and
//! the serve loop in serve.rs own the wire end to end.
//!
//! # Synchronous-only invariant (same as dispatch.rs)
//! No framework types, no runtime handles, no concurrency primitives, no
//! suspension point in any handler. All blocking I/O is `std::io` /
//! `std::thread`. The S3 tier deliberately degrades to the local CAS chain
//! (hot -> cold) - a known gap vs the production resolver, which downloads
//! from S3 when built with the `s3` feature. Sync S3 download is planned after
//! local minio test matrix), so no external blocking client is required
//! in the sync transport yet.

#![forbid(unsafe_code)]

use std::io::{self, Read, Seek, Write};
use std::time::Duration;

use clarus_core::http::body::{ChunkedReader, Framing, PrefixedReader, framing};
use clarus_core::http::conn::Conn;
use clarus_core::http::multipart::{MultipartEvent, MultipartParser};
use clarus_core::http::parse::RequestHead;
use clarus_core::http::router::KeepAlive;
use clarus_ports::StorageTier;

use crate::dispatch::RequestContext;
use crate::handlers::json_common::push_json_str;
use crate::handlers::json_common::push_json_str_loose;
use crate::handlers::stow_pure::IngestOutcome;
use crate::handlers::wado::error::WadoError;

/// JSON error content type (matches dispatch.rs).
const JSON_CT: &[u8] = b"application/json; charset=utf-8";
/// Stable DICOM content type used by WADO single-object and STOW.
const DICOM_CT: &[u8] = b"application/dicom";

/// The media type of a DICOM body that is Explicit VR Little Endian.
///
/// PS3.18 8.7.3.1 gives `application/dicom` an optional `transfer-syntax`
/// parameter, and 8.7.3 forbids serving Implicit VR LE over Web Services at
/// all. A body this server produced by transcoding therefore says which syntax
/// it is, so the client does not have to read the file meta to find out.
///
/// This is a `&'static str` because [`WadoResp::content_type`] is one, and
/// `concat!` needs literals, so the UID appears here once. The unit test below
/// pins it against `ts_uid::EXPLICIT_VR_LE` - the constant stays the single
/// source of truth, and a drifted copy fails the build's tests instead of
/// shipping a media type that names a different syntax than the bytes.
/// The Explicit VR Little Endian media type, kept as a test fixture only.
///
/// The single-instance body no longer answers with a fixed syntax - it is
/// whatever `maybe_transcode` returned - so nothing in the server uses this
/// value. What it still does is pin the SPELLING of the media type against
/// `ts_uid::EXPLICIT_VR_LE`: [`dicom_media_type`] builds that string at run
/// time from the manifest's UID, and the test below keeps the format honest.
#[cfg(test)]
const DICOM_EVRLE_CT: &str = "application/dicom; transfer-syntax=1.2.840.10008.1.2.1";
/// SSE content type.
const SSE_CT: &[u8] = b"text/event-stream";

/// Counts how many times `h_wado_rendered` reaches its WHOLE-blob read (STEP
/// 5). The 413 pixel-budget gate runs BEFORE that read, so an over-budget
/// instance must never move this counter - the wire test asserts exactly that.
/// One relaxed add per `/rendered` request; no other effect.
pub static RENDERED_FULL_READS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// -------------------------------------------------------------------------
// Shared transport helpers
// -------------------------------------------------------------------------

/// Reason phrase lookup (kept local; mirrors the phrase table in dispatch.rs).
fn reason_phrase(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        400 => "Bad Request",
        404 => "Not Found",
        406 => "Not Acceptable",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        507 => "Insufficient Storage",
        _ => "OK",
    }
}

/// Build and write a bare response with a body + Content-Length.
fn write_resp(conn: &mut Conn, status: u16, ctype: &[u8], body: &[u8], ka: bool) -> io::Result<()> {
    clarus_core::http::response::build_head(
        &mut conn.header,
        status,
        reason_phrase(status),
        Some(ctype),
        Some(body.len() as u64),
        ka,
        &[],
    );
    conn.write_head_configured()?;
    conn.write_all_configured(body)
}

/// Map a mid-stream STOW temp-file `write_all` failure to the wire response.
///
/// `write_all` is `write(2)` under the hood: an in-flight `ENOSPC` is the
/// honest `507 Insufficient Storage` (the preflight only runs BEFORE the body,
/// so a disk that fills during a large upload lands HERE). The body reuses the
/// preflight shape via [`crate::handlers::stow_pure::insufficient_storage_error`]
/// and carries `Retry-After: 300`. Any other write error stays `500` exactly as
/// today.
fn write_stow_write_error(ctx: &mut RequestContext<'_>, ka: bool, e: io::Error) -> io::Result<()> {
    if e.kind() == io::ErrorKind::StorageFull {
        let free_mb = crate::disk_utils::available_disk_mb(ctx.state.cas.root_dir());
        let stow_err = crate::handlers::stow_pure::insufficient_storage_error(free_mb);
        crate::clog_warn!("STOW 507: {}", String::from_utf8_lossy(&stow_err.body));
        let retry_buf = stow_err.retry_after.unwrap_or(0).to_string().into_bytes();
        let extras: Vec<(&[u8], &[u8])> = vec![(b"Retry-After", &retry_buf)];
        clarus_core::http::response::build_head(
            &mut ctx.conn.header,
            stow_err.status,
            reason_phrase(stow_err.status),
            Some(JSON_CT),
            Some(stow_err.body.len() as u64),
            ka,
            &extras,
        );
        ctx.conn.write_head_configured()?;
        return ctx.conn.write_all_configured(&stow_err.body);
    }
    crate::clog_warn!(error = %e, "STOW 500: temp write");
    let body = crate::handlers::json_common::err_status_json(&format!("temp write: {e}"), 500);
    write_resp(ctx.conn, 500, JSON_CT, &body, ka)
}

/// The PURE part of [`write_stow_write_error`]: pick the STOW error for a
/// mid-stream write failure. `ENOSPC` -> 507 (+ the shared preflight-shaped
/// body); anything else -> 500. Split out so the mapping is unit-testable
/// without a full-disk fixture or a live connection.
pub fn stow_write_error_for(
    kind: io::ErrorKind,
    free_mb: f64,
) -> crate::handlers::stow_pure::StowError {
    if kind == io::ErrorKind::StorageFull {
        crate::handlers::stow_pure::insufficient_storage_error(free_mb)
    } else {
        crate::handlers::stow_pure::StowError::plain(
            500,
            crate::handlers::json_common::err_status_json("temp write failed", 500),
        )
    }
}

/// Write a STOW response, adding the RFC 7234 `Warning` header (partial
/// success / failures) exactly as the axum transport did.
fn write_stow_response(
    conn: &mut Conn,
    status: u16,
    ct: &str,
    body: &[u8],
    warning: Option<(String, String)>,
    ka: bool,
) -> io::Result<()> {
    let extra: Vec<(Vec<u8>, Vec<u8>)> = warning
        .map(|(k, v)| (k.into_bytes(), v.into_bytes()))
        .into_iter()
        .collect();
    let extra_refs: Vec<(&[u8], &[u8])> = extra
        .iter()
        .map(|(k, v)| (k.as_slice(), v.as_slice()))
        .collect();
    clarus_core::http::response::build_head(
        &mut conn.header,
        status,
        reason_phrase(status),
        Some(ct.as_bytes()),
        Some(body.len() as u64),
        ka,
        &extra_refs,
    );
    conn.write_head_configured()?;
    conn.write_all_configured(body)
}

/// Map a `WadoError` onto the wire as a status plus a (mostly JSON) body.
///
/// Reproduced synchronously from the axum `IntoResponse` (error.rs) so the
/// dispatch transport emits the same status/body as prod. `FileNotFound`
/// deliberately yields an *empty* body - DICOM viewers expect the resource to
/// be absent, not a JSON error, on the sendfile path.
fn wado_err_response(ctx: &mut RequestContext<'_>, err: WadoError) -> io::Result<()> {
    let ka = ctx.keep_alive == KeepAlive::On;
    match err {
        WadoError::InvalidUid(uid) => {
            let body = error_body("Invalid DICOM UID: ", &uid);
            write_resp(ctx.conn, 400, JSON_CT, &body, ka)
        }
        // PS3.18 8.3.5.1.3: an ill-formed or unsupported rendering query
        // parameter is a 400 with a Status Report payload, never a silent
        // clamp. The message names the parameter and what was wrong with it.
        WadoError::BadRequest(msg) => {
            let body = error_body("", &msg);
            write_resp(ctx.conn, 400, JSON_CT, &body, ka)
        }
        WadoError::NotFound(_) => {
            let body = b"{\"error\":\"Not Found\",\"message\":\"The requested resource was not found.\",\"status\":404}".to_vec();
            write_resp(ctx.conn, 404, JSON_CT, &body, ka)
        }
        WadoError::FileNotFound(_) => write_resp(ctx.conn, 404, JSON_CT, &[], ka),
        WadoError::RangeNotSatisfiable => {
            let body = b"{\"error\":\"Range not satisfiable\"}".to_vec();
            write_resp(ctx.conn, 416, JSON_CT, &body, ka)
        }
        WadoError::NotAcceptable(msg) => {
            let body = error_body("", &msg);
            write_resp(ctx.conn, 406, JSON_CT, &body, ka)
        }
        WadoError::PayloadTooLarge(msg) => {
            let body = error_body("", &msg);
            write_resp(ctx.conn, 413, JSON_CT, &body, ka)
        }
        // Same 413, message known at compile time. `error_body` still builds
        // the JSON envelope here; only the MESSAGE avoids an allocation, which
        // is what the ap101 budget gate requires from the refusal itself.
        WadoError::PayloadTooLargeStatic(msg) => {
            let body = error_body("", msg);
            write_resp(ctx.conn, 413, JSON_CT, &body, ka)
        }
        // 2608: honest fast 503 when the global transcode/render pool is
        // saturated. No unbounded queue - the client retries after 5s.
        WadoError::Busy => {
            let body =
                b"{\"error\":\"Transcode busy\",\"message\":\"Transcode pool saturated - retry later\",\"status\":503}"
                    .to_vec();
            write_resp_with_header(
                ctx.conn,
                503,
                JSON_CT,
                &body,
                ka,
                (&b"Retry-After"[..], &b"5"[..]),
            )
        }
        WadoError::Internal(_) => {
            let body = b"{\"error\":\"Internal server error\"}".to_vec();
            write_resp(ctx.conn, 500, JSON_CT, &body, ka)
        }
    }
}

/// Build a `{"error":"..."}` body with the message JSON-escaped.
///
/// The message carries client-controlled text - a rejected UID, an
/// unsupported `transfer-syntax=` or `Accept` value - so splicing it into a
/// quoted string let a single `"` break the body or inject sibling keys. The
/// escaper is the same one every other JSON surface uses.
fn error_body(prefix: &str, msg: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(prefix.len() + msg.len() + 16);
    out.extend_from_slice(b"{\"error\":");
    push_json_str(&mut out, &format!("{prefix}{msg}"));
    out.push(b'}');
    out
}

/// Like [`write_resp`] but with one extra response header (used for the 503
/// `Retry-After`). The header is emitted before Content-Length by wrapping
/// through `build_head`'s `extra` slice.
fn write_resp_with_header(
    conn: &mut Conn,
    status: u16,
    ctype: &[u8],
    body: &[u8],
    ka: bool,
    header: (&[u8], &[u8]),
) -> io::Result<()> {
    clarus_core::http::response::build_head(
        &mut conn.header,
        status,
        reason_phrase(status),
        Some(ctype),
        Some(body.len() as u64),
        ka,
        &[header],
    );
    conn.write_head_configured()?;
    conn.write_all_configured(body)
}

/// Open a stored CAS file synchronously for `blake3` (CAS key).
///
/// The declared tier is tried first, then the opposite local CAS - exactly the
/// chain the production `resolve_dicom_stream` uses, so on the default/transcode
/// builds (no S3 feature) an object resolves wherever it physically lives. The
/// canonical two-level BLAKE3 layout is used. The core `sendfile_response`
/// handles the EINVAL/ENOSYS fallback to `copy_loop`, so this wrapper never
/// inspects errno.
fn resolve_file(
    state: &crate::state::AppState,
    blake3: &str,
    tier: StorageTier,
) -> io::Result<std::fs::File> {
    // Probe order per tier; `StorageTier::S3` deliberately degrades to the
    // local CAS chain (hot -> cold). Known gap: production downloads from S3
    // on `s3` builds; the sync S3 download lands once the minio matrix
    // is wired.
    let mut cas_order: Vec<&clarus_adapters::storage::ContentAddressedStorage> = Vec::new();
    match tier {
        StorageTier::Hot | StorageTier::S3 => {
            cas_order.push(&state.cas);
            if let Some(ref cold) = state.cold_cas {
                cas_order.push(cold);
            }
        }
        StorageTier::Cold => {
            if let Some(ref cold) = state.cold_cas {
                cas_order.push(cold);
            }
            cas_order.push(&state.cas);
        }
    }

    for cas in cas_order {
        // A8: the canonical two-level layout rendered into a stack buffer -
        // no `format!` per open (the PathBuf join stays, one allocation).
        let mut rel_buf = [0u8; 128];
        let Some(rel) = clarus_adapters::storage::cas_rel_path_fill_buf(blake3, &mut rel_buf)
        else {
            continue;
        };
        if let Ok(f) = std::fs::File::open(cas.root_dir().join(rel)) {
            // 0210: storage read throughput. `metadata()` here is one stat on
            // the fd we already opened - the alternative (counting the bytes
            // from the transfer loop) would mean touching the sendfile/copy
            // path the ap101 gates pin, for a rate() that only needs a
            // counter. The length of the file handed to the writer IS the
            // bytes read.
            if let Ok(md) = f.metadata() {
                crate::metrics::STORAGE_READ_BYTES
                    .fetch_add(md.len(), std::sync::atomic::Ordering::Relaxed);
            }
            return Ok(f);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("CAS object not found: {blake3}"),
    ))
}

/// Resolve a stored object to its CAS PATH (not an open handle).
///
/// Same tier probe order as [`resolve_file`]; used by the bulkdata
/// seek-based single-tag read, which needs the path to re-open + seek rather
/// than a buffered handle.
fn resolve_path(
    state: &crate::state::AppState,
    blake3: &str,
    tier: StorageTier,
) -> io::Result<std::path::PathBuf> {
    let mut cas_order: Vec<&clarus_adapters::storage::ContentAddressedStorage> = Vec::new();
    match tier {
        StorageTier::Hot | StorageTier::S3 => {
            cas_order.push(&state.cas);
            if let Some(ref cold) = state.cold_cas {
                cas_order.push(cold);
            }
        }
        StorageTier::Cold => {
            if let Some(ref cold) = state.cold_cas {
                cas_order.push(cold);
            }
            cas_order.push(&state.cas);
        }
    }
    for cas in cas_order {
        let mut rel_buf = [0u8; 128];
        let Some(rel) = clarus_adapters::storage::cas_rel_path_fill_buf(blake3, &mut rel_buf)
        else {
            continue;
        };
        let path = cas.root_dir().join(rel);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("CAS object not found: {blake3}"),
    ))
}

/// Read a stored object fully into memory (buffered paths: thumbnail, frame
/// extraction, metadata blob, bulkdata).
pub(crate) fn read_cas_bytes(
    state: &crate::state::AppState,
    blake3: &str,
    tier: StorageTier,
) -> io::Result<Vec<u8>> {
    let mut file = resolve_file(state, blake3, tier)?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Read at most `max` bytes from the START of a stored object (0310 F3).
///
/// The bounded-header primitive for the metadata / bulkdata routes: those
/// renderers stop at PixelData, so they need only the small head of the blob,
/// never the multi-GB pixel stream. Allocating `max` bytes for a 13 GB
/// instance replaces a 13 GB `read_to_end` (the pre-F3 whole-blob read).
///
/// `max` is a trusted config bound (`max_part_header`), not an instance size.
/// A file shorter than `max` returns the whole file; a longer one returns
/// exactly `max` bytes.
pub fn read_cas_head(
    state: &crate::state::AppState,
    blake3: &str,
    tier: StorageTier,
    max: u64,
) -> io::Result<Vec<u8>> {
    let mut file = resolve_file(state, blake3, tier)?;
    let file_len = file.metadata()?.len();
    let read_len = max.min(file_len) as usize;
    let mut buf = vec![0u8; read_len];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// Read exactly `len` bytes at `offset` from a stored object.
///
/// This is the frame-chunk serve path: a bounded `seek` + `read` that
/// allocates only the frame, never the instance. It exists because the
/// manifest now carries a per-frame `(offset, compressed_len)` table, so
/// serving one tile from a multi-GB WSI no longer costs a multi-GB `Vec`.
///
/// A short read is an error rather than a truncated frame: the table is
/// derived from the blob, so a short read means the blob and the manifest
/// disagree (a replaced CAS object), and serving a partial tile would hide
/// that. The caller falls back to the whole-blob path.
///
/// # Allocation bound (0310 F2)
/// `len` comes from `FrameRecord.compressed_len`, which is the `item_len`
/// read from the untrusted instance at STOW. A crafted `item_len` of
/// `0xFFFFFFFF` therefore stores a 4 GB length, and a single GET would
/// otherwise request exactly that many bytes. `config.max_frame_bytes` is
/// the trusted bound: it is checked BEFORE the `vec!`, so an over-cap length
/// costs ZERO allocations. It is compared as `u64` (no `usize` truncation)
/// so the guard cannot be bypassed by a platform with a narrow `usize`.
///
/// Public for the ap101 gate (`tests/ap101_wado_range.rs`), which proves the
/// zero-allocation refusal - the same reason `blake3_of_file` is public.
pub fn read_cas_range(
    state: &crate::state::AppState,
    blake3: &str,
    tier: StorageTier,
    offset: u64,
    len: u64,
) -> io::Result<Vec<u8>> {
    let max_frame_bytes = state.config.max_frame_bytes;
    if len > max_frame_bytes {
        // `io::Error::from` on a bare `ErrorKind` is allocation-free, while
        // `io::Error::new(kind, msg)` boxes a custom payload - the ap101 gate
        // asserts ZERO bytes on this exact refusal, so the cheap form is the
        // one that keeps the 4 GB request from touching the allocator at all.
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let mut file = resolve_file(state, blake3, tier)?;
    let cap = usize::try_from(len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame length does not fit in memory",
        )
    })?;
    let mut buf = vec![0u8; cap];
    if cap == 0 {
        return Ok(buf); // a sparse tile is a legitimately empty frame
    }
    file.seek(io::SeekFrom::Start(offset))?;
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// Write a full buffered body produced by a pure core function (`WadoResp`).
///
/// Emits the renderer's extra headers (`resp.headers`), forwarding them to
/// `build_head`'s `extra` slice, but DROPS any `Content-Length` entry from it:
/// `build_head` computes `Content-Length` itself from `body.len()`, so a
/// duplicate would be illegal. Rendering headers such as `Cache-Control` and
/// `ETag` therefore reach the wire exactly as the renderer set them.
fn write_wado_resp(
    ctx: &mut RequestContext<'_>,
    resp: &crate::handlers::wado_pure::WadoResp,
) -> io::Result<()> {
    let ka = ctx.keep_alive == KeepAlive::On;
    // resp.headers pairs are (&'static str, String); only a temporary Vec can
    // host the borrowed &[u8] slices build_head wants.
    let mut extra: Vec<(&[u8], &[u8])> = Vec::with_capacity(resp.headers.len());
    for (k, v) in &resp.headers {
        if k.eq_ignore_ascii_case("content-length") {
            continue; // build_head derives it from body.len() - no duplicate
        }
        extra.push((k.as_bytes(), v.as_bytes()));
    }
    clarus_core::http::response::build_head(
        &mut ctx.conn.header,
        resp.status,
        reason_phrase(resp.status),
        Some(resp.content_type.as_bytes()),
        Some(resp.body.len() as u64),
        ka,
        &extra,
    );
    ctx.conn.write_head_configured()?;
    ctx.conn.write_all_configured(&resp.body)
}

// -------------------------------------------------------------------------
// 1. WADO single-object -> sendfile
// -------------------------------------------------------------------------

/// Serve one DICOM object unchanged from CAS (WADO-RS instance / WADO-URI).
///
/// `locate_object` (stage 4) validates UIDs and resolves the CAS key+tier;
/// `resolve_file` opens it synchronously; the head (with Content-Length) is
/// written first, then `sendfile_response` streams the bytes in one system
/// call. The core handles the non-Linux fallback, so the wrapper stays uniform.
fn serve_single_object(
    ctx: &mut RequestContext<'_>,
    study_uid: &str,
    series_uid: Option<&str>,
    sop_uid: &str,
) -> io::Result<()> {
    let (oref, expected) = match crate::handlers::wado_pure::locate_object(
        ctx.state,
        study_uid,
        series_uid,
        sop_uid,
        ctx.route.qa,
    ) {
        Ok(v) => v,
        Err(e) => return wado_err_response(ctx, e),
    };
    // Record the series slice count for the WADO access aggregator. A failed
    // resolve returns Err here and leaves the recorded count untouched (the
    // aggregator keeps its one-time expected from the last success).
    ctx.wado_series_expected = expected.or(ctx.wado_series_expected);

    // PS3.18 8.7.3: an Implicit VR Little Endian instance shall not be served
    // over Web Services, so this path cannot stream the stored bytes as they
    // are. The dataset is re-encoded to Explicit VR (headers only; the values,
    // including a Surface Segmentation's mesh data, are untouched), which is a
    // buffered answer - the cost is inherent to the correctness of the reply,
    // and `sendfile_response` below stays the path for every other syntax.
    if oref.transfer_syntax == clarus_adapters::codec::ts_uid::IMPLICIT_VR_LE {
        let bytes = match read_cas_bytes(ctx.state, &oref.sha256, oref.storage_tier) {
            Ok(b) => b,
            Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(oref.sha256)),
        };
        let (out, out_ts) = match crate::handlers::wado_pure::maybe_transcode(
            bytes,
            &oref.transfer_syntax,
            Some(clarus_adapters::codec::ts_uid::EXPLICIT_VR_LE),
            &ctx.state.codec_registry,
            &ctx.state.transcode_pool,
            ctx.state.config.transcode.max_megapixels,
        ) {
            Ok(v) => v,
            // A conversion that produced nothing is not an answer: an empty
            // 200 would tell the client it retrieved the instance. The error
            // that got here already names the reason.
            Err(e) => return wado_err_response(ctx, e),
        };
        if out.is_empty() {
            return wado_err_response(
                ctx,
                WadoError::NotAcceptable(
                    "Re-encoding the Implicit VR instance produced no bytes".to_string(),
                ),
            );
        }
        let range_header = ctx.head.range.and_then(|v| std::str::from_utf8(v).ok());
        let total = out.len() as u64;
        return serve_instance_buffer(ctx, out, &out_ts, range_header, total);
    }

    let file = match resolve_file(ctx.state, &oref.sha256, oref.storage_tier) {
        Ok(f) => f,
        Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(oref.sha256)),
    };
    // N2: the length comes from the file actually opened, not from the
    // manifest. `oref.file_size` is a manifest record, and this file itself
    // documents it as stale after a canonicalization rewrite - so a Range, a
    // Content-Length or a sendfile span computed from it could describe bytes
    // that are no longer there. One stat on the handle already in hand is the
    // only authoritative answer.
    let real_size = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(oref.sha256)),
    };
    // The stored bytes are served unchanged, so the media type names the
    // syntax they are actually in (PS3.18 8.7.3.1). Answering with a bare
    // `application/dicom` left a client to decode the file meta itself, and a
    // viewer that maps the media type on its own (cornerstone reads the part
    // Content-Type first) fell back to Implicit VR and decoded compressed
    // pixels as raw bytes.
    let media_type = dicom_media_type(&oref.transfer_syntax);
    // RFC 9110 14.2: a Range on a streamed object is answered with 206 and
    // only the requested span. The whole-file head declared 200 and the whole
    // length, so a client asking for a slice received the full object under a
    // status that promised exactly the object.
    let range_header = ctx.head.range.and_then(|v| std::str::from_utf8(v).ok());
    let (status, reason, span) =
        match crate::handlers::wado_range::parse_range_header_str(range_header, real_size) {
            crate::handlers::wado_range::RangeResult::Partial(range) => {
                (206u16, "Partial Content", range)
            }
            crate::handlers::wado_range::RangeResult::NotSatisfiable => {
                return wado_err_response(ctx, WadoError::RangeNotSatisfiable);
            }
            // `InvalidIgnored` is the RFC 9110 14.2 "unsatisfiable syntax is
            // ignored" case: the full representation is the legal answer.
            _ => (
                200u16,
                "OK",
                crate::handlers::wado_range::ByteRange {
                    start: 0,
                    end: real_size.saturating_sub(1),
                },
            ),
        };
    let content_len = if real_size == 0 { 0 } else { span.len() };
    let mut extra: Vec<(&[u8], Vec<u8>)> = Vec::with_capacity(2);
    extra.push((b"Accept-Ranges", b"bytes".to_vec()));
    if status == 206 {
        extra.push((
            b"Content-Range",
            crate::handlers::wado_range::content_range_header(span, real_size).into_bytes(),
        ));
    }
    let extra_refs: Vec<(&[u8], &[u8])> = extra.iter().map(|(k, v)| (*k, v.as_slice())).collect();
    // A7: build the head into the reusable per-connection header buffer and
    // take it for the sendfile call (the conn.rs `send_file` take/re-assign
    // dance). No fresh Vec per request; the wire bytes are identical.
    clarus_core::http::response::build_head(
        &mut ctx.conn.header,
        status,
        reason,
        Some(media_type.as_bytes()),
        Some(content_len),
        ctx.keep_alive == KeepAlive::On,
        &extra_refs,
    );
    let head = std::mem::take(&mut ctx.conn.header);
    let res = if ctx.conn.suppress_body() {
        // HEAD: the response head (with the GET Content-Length) goes out,
        // the file bytes do not.
        ctx.conn.stream().write_all(&head)
    } else {
        ctx.conn.write_all_configured(&head).and_then(|_| {
            clarus_core::http::response::sendfile_response_at(
                ctx.conn.stream(),
                &[],
                &file,
                span.start,
                content_len,
            )?;
            // sendfile_response writes straight to the raw stream and bypasses
            // Conn::response_bytes, so a serve_single_* instance download was
            // invisible to the access log - the same undercount the multipart
            // part loop fixed for streamed parts. A whole-instance GET is the
            // largest body this server sends; leaving it out made `bytes=` the
            // header size for exactly the requests that matter most.
            ctx.conn.add_response_bytes(content_len);
            Ok(())
        })
    };
    ctx.conn.header = head;
    res
}

/// The WADO-RS media type for a stored instance, with its transfer syntax.
///
/// PS3.18 8.7.3.1 lists the `transfer-syntax` parameter as optional for
/// `application/dicom`; adding it is what lets a client pick its decoder
/// without parsing the file meta, and a compressed instance is otherwise
/// indistinguishable from an uncompressed one at the header layer.
///
/// The value comes from the manifest, not from a constant, because it must
/// describe the bytes being sent.
fn dicom_media_type(transfer_syntax_uid: &str) -> String {
    format!("application/dicom; transfer-syntax={transfer_syntax_uid}")
}

/// Serve an already-transcoded single-instance buffer, applying an RFC 7233
/// Range if present.
///
/// Frozen pre-extraction contract (Scenario 3 / Scenario 4 in wado_smoke):
/// - no valid Range -> 200, full buffer, `application/dicom`;
/// - `bytes=start-end` -> 206, `Content-Range: bytes start-end/total`, body =
///   `buffer[start..end]` with a RUST-EXCLUSIVE end (so `bytes=0-9` yields the
///   `[0..9]` slice, byte-width 9);
/// - Range beyond size -> 416 via `RangeNotSatisfiable`.
fn serve_instance_buffer(
    ctx: &mut RequestContext<'_>,
    out: Vec<u8>,
    out_ts: &str,
    range_header: Option<&str>,
    total: u64,
) -> io::Result<()> {
    use crate::handlers::wado_range::{RangeResult, content_range_header, parse_range_header_str};
    // The body is the OUTPUT of `maybe_transcode`, which may be the stored
    // bytes (any syntax), a re-encoded Explicit VR dataset, a transcoded one,
    // or the ORIGINAL bytes after a transient decoder failure. The parameter
    // has to state which, so it is passed in rather than assumed: a hard-coded
    // Explicit VR LE label was wrong on three of those four paths, and a client
    // that picks its decoder from the header decoded the wrong thing.
    let content_type = dicom_media_type(out_ts);
    match parse_range_header_str(range_header, total) {
        RangeResult::FullFile | RangeResult::InvalidIgnored => {
            // Written directly rather than through `WadoResp`: that struct
            // carries `content_type: &'static str`, and the media type here is
            // built at run time from the syntax the body is actually in.
            let mut headers: Vec<(&str, String)> = vec![("Accept-Ranges", "bytes".to_string())];
            write_buffer_response(ctx, 200, &content_type, &mut headers, out)
        }
        RangeResult::Partial(range) => {
            // RFC 9110 14.1.2: the Content-Range end is INCLUSIVE, a Rust
            // slice end is EXCLUSIVE. Adding one converts between them; a body
            // of `bytes=0-9` is ten bytes, and the header and the body then
            // describe the same span. Without it every 206 was one byte short
            // of what its own header promised, which a client assembling
            // ranges reads as a missing byte.
            let end = (range.end as usize).saturating_add(1).min(out.len());
            let start = (range.start as usize).min(end);
            let body = out[start..end].to_vec();
            let mut headers = vec![("Accept-Ranges", "bytes".to_string())];
            headers.push(("Content-Range", content_range_header(range, total)));
            write_buffer_response(ctx, 206, &content_type, &mut headers, body)
        }
        RangeResult::NotSatisfiable => wado_err_response(ctx, WadoError::RangeNotSatisfiable),
    }
}

/// Write a buffered WADO-RS response whose media type is decided at run time.
///
/// The counterpart of [`write_wado_resp`] for the one case that cannot go
/// through [`crate::handlers::wado_pure::WadoResp`]: its `content_type` is
/// `&'static str`, and the single-instance body may come back in any syntax
/// the request asked for.
fn write_buffer_response(
    ctx: &mut RequestContext<'_>,
    status: u16,
    content_type: &str,
    headers: &mut Vec<(&'static str, String)>,
    body: Vec<u8>,
) -> io::Result<()> {
    let ka = ctx.keep_alive == KeepAlive::On;
    let mut extra: Vec<(&[u8], &[u8])> = Vec::with_capacity(headers.len());
    for (k, v) in headers.iter() {
        extra.push((k.as_bytes(), v.as_bytes()));
    }
    clarus_core::http::response::build_head(
        &mut ctx.conn.header,
        status,
        reason_phrase(status),
        Some(content_type.as_bytes()),
        Some(body.len() as u64),
        ka,
        &extra,
    );
    ctx.conn.write_all_configured(&ctx.conn.header.clone())?;
    ctx.conn.header.clear();
    ctx.conn.write_all_configured(&body)?;
    ctx.conn.add_response_bytes(body.len() as u64);
    Ok(())
}

/// `GET /dicomweb/studies/{s}/series/{se}/instances/{i}` - single instance.
///
/// Same overload as the study and series paths: a JSON Accept serves the
/// QIDO-RS instance resource (8.7), otherwise WADO-RS (10.4) as before. The
/// `image/jpeg` renderer below keeps priority over both, because a client
/// asking for a rendered image is asking for neither transaction.
pub(crate) fn h_wado_instance(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    // Full-size rendered: `Accept: image/jpeg` on the INSTANCE endpoint returns
    // a full-resolution JPEG (v1, 26.08). `application/dicom*` is untouched.
    // The accept match is case-insensitive and tolerates a `;q=` media-range
    // parameter, so both `image/jpeg` and `image/jpeg;q=0.9` trigger it.
    let wants_rendered = ctx
        .accept()
        .map(|a| content_type_is(a, "image/jpeg"))
        .unwrap_or(false);
    if !wants_rendered {
        match classify_retrieve_accept(ctx.accept()) {
            RetrieveWanted::Json => return serve_retrieve_json_instance(ctx),
            RetrieveWanted::Unacceptable => {
                return wado_err_response(ctx, not_acceptable(ctx.accept()));
            }
            RetrieveWanted::Wado => {}
        }
    }
    if wants_rendered {
        let (oref, _expected) = match crate::handlers::wado_pure::locate_object(
            ctx.state,
            ctx.uid(0),
            Some(ctx.uid(1)),
            ctx.uid(2),
            ctx.route.qa,
        ) {
            Ok(v) => v,
            Err(e) => return wado_err_response(ctx, e),
        };
        let bytes = match read_cas_bytes(ctx.state, &oref.sha256, oref.storage_tier) {
            Ok(b) => b,
            Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(oref.sha256)),
        };
        let resp = match crate::handlers::wado_pure::render_full_from_bytes(
            &bytes,
            ctx.uid(2),
            &ctx.state.codec_registry,
            &ctx.state.transcode_pool,
            ctx.state.config.transcode.max_megapixels,
        ) {
            Ok(r) => r,
            Err(e) => return wado_err_response(ctx, e), // Busy -> 503; PayloadTooLarge -> 413
        };
        return write_wado_resp(ctx, &resp);
    }

    // An explicit target transfer syntax (`?transferSyntax=` or a matching
    // `Accept` TS parameter) routes the single instance through the buffered
    // transcode path, BEFORE the multipart/single split. A `transcode` build
    // transcodes the instance; an unsupported TS yields the honest 406
    // ("Available:" list). The stored == requested no-op passes through
    // unchanged, and a Range header is then applied to the (possibly
    // transcoded) buffer per the frozen pre-extraction contract: 206 with
    // `Content-Range: bytes start-end/total` and a body sliced with a
    // Rust-exclusive end.
    let target_ts = crate::handlers::wado_pure::resolve_target_ts(&wado_query(ctx), ctx.accept());
    if let Some(ts) = target_ts {
        let (oref, expected) = match crate::handlers::wado_pure::locate_object(
            ctx.state,
            ctx.uid(0),
            Some(ctx.uid(1)),
            ctx.uid(2),
            ctx.route.qa,
        ) {
            Ok(v) => v,
            Err(e) => return wado_err_response(ctx, e),
        };
        ctx.wado_series_expected = expected.or(ctx.wado_series_expected);
        let bytes = match read_cas_bytes(ctx.state, &oref.sha256, oref.storage_tier) {
            Ok(b) => b,
            Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(oref.sha256)),
        };
        let (out, out_ts) = match crate::handlers::wado_pure::maybe_transcode(
            bytes,
            &oref.transfer_syntax,
            Some(&ts),
            &ctx.state.codec_registry,
            &ctx.state.transcode_pool,
            ctx.state.config.transcode.max_megapixels,
        ) {
            // NotAcceptable -> 406; Busy -> 503; PayloadTooLarge -> 413.
            Ok(v) => v,
            Err(e) => return wado_err_response(ctx, e),
        };
        let range_header = ctx.head.range.and_then(|v| std::str::from_utf8(v).ok());
        let total = out.len() as u64;
        return serve_instance_buffer(ctx, out, &out_ts, range_header, total);
    }

    // DICOMweb viewers (Weasis, OHIF) request `multipart/related` even for a
    // single instance; wrap it then, otherwise send raw `application/dicom`.
    let wants_multipart = ctx
        .accept()
        .map(|a| a.to_ascii_lowercase().contains("multipart/related"))
        .unwrap_or(false);
    if !wants_multipart {
        return serve_single_object(ctx, ctx.uid(0), Some(ctx.uid(1)), ctx.uid(2));
    }
    // Single-instance multipart/related, byte-parity with the axum path.
    let (oref, expected) = match crate::handlers::wado_pure::locate_object(
        ctx.state,
        ctx.uid(0),
        Some(ctx.uid(1)),
        ctx.uid(2),
        ctx.route.qa,
    ) {
        Ok(v) => v,
        Err(e) => return wado_err_response(ctx, e),
    };
    ctx.wado_series_expected = expected.or(ctx.wado_series_expected);
    // PS3.18 8.7.3 forbids serving Implicit VR LE over Web Services, and the
    // part header below names the syntax its bytes are in - so an Implicit VR
    // instance cannot go down the streaming path: `sendfile_response` would
    // put the stored bytes behind a header that must not describe them. The
    // re-encoded dataset is assembled as one buffer and served through the
    // same framing the buffered path used before F3.
    if oref.transfer_syntax == clarus_adapters::codec::ts_uid::IMPLICIT_VR_LE {
        let bytes = match read_cas_bytes(ctx.state, &oref.sha256, oref.storage_tier) {
            Ok(b) => b,
            Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(oref.sha256)),
        };
        let (out, out_ts) = match crate::handlers::wado_pure::maybe_transcode(
            bytes,
            &oref.transfer_syntax,
            Some(clarus_adapters::codec::ts_uid::EXPLICIT_VR_LE),
            &ctx.state.codec_registry,
            &ctx.state.transcode_pool,
            ctx.state.config.transcode.max_megapixels,
        ) {
            Ok(v) => v,
            Err(e) => return wado_err_response(ctx, e),
        };
        if out.is_empty() {
            return wado_err_response(
                ctx,
                WadoError::NotAcceptable(
                    "Re-encoding the Implicit VR instance produced no bytes".to_string(),
                ),
            );
        }
        let boundary = single_part_boundary();
        // The head goes out first, then the re-encoded instance, then the
        // closing boundary: the same framing the streaming path writes, with
        // the converted bytes in the middle where the file used to be.
        let ct = format!("multipart/related; type=\"application/dicom\"; boundary=\"{boundary}\"");
        let part_ct = dicom_media_type(&out_ts);
        let mut head = format!("\r\n--{boundary}\r\nContent-Type: {part_ct}\r\n\r\n").into_bytes();
        let body_len = head.len() as u64 + out.len() as u64 + close_len(boundary);
        clarus_core::http::response::build_head(
            &mut ctx.conn.header,
            200,
            reason_phrase(200),
            Some(ct.as_bytes()),
            Some(body_len),
            ctx.keep_alive == KeepAlive::On,
            &[],
        );
        let mut wire = std::mem::take(&mut ctx.conn.header);
        wire.append(&mut head);
        let res = if ctx.conn.suppress_body() {
            ctx.conn.stream().write_all(&wire)
        } else {
            ctx.conn.write_all_configured(&wire).and_then(|_| {
                ctx.conn.write_all_configured(&out).and_then(|_| {
                    ctx.conn
                        .write_all_configured(format!("\r\n--{boundary}--\r\n").as_bytes())
                })
            })
        };
        ctx.conn.header = wire;
        return res;
    }
    // 0310 F3 (D2): STREAM the single instance instead of buffering it.
    // The pre-F3 code built `Vec::with_capacity(bytes.len() + 256)` - the
    // WHOLE instance in RAM plus the part framing. Here the instance body is
    // streamed from the CAS file with `sendfile_response` (the :349 mechanism),
    // so a multi-GB instance costs O(1) server memory. Content-Length is
    // computed from the REAL file size (N2: the manifest record can be stale
    // after a canonicalization rewrite) + the fixed framing.
    let file = match resolve_file(ctx.state, &oref.sha256, oref.storage_tier) {
        Ok(f) => f,
        Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(oref.sha256)),
    };
    let real_size = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(oref.sha256)),
    };
    let boundary = single_part_boundary();
    // Framing: "\r\n--B\r\nContent-Type: ...\r\n\r\n" + <instance> +
    // "\r\n--B--\r\n". Byte-identical to the pre-F3 buffered body.
    let open = format!(
        "\r\n--{boundary}\r\nContent-Type: application/dicom; transfer-syntax={}\r\n\r\n",
        oref.transfer_syntax
    );
    let close = format!("\r\n--{boundary}--\r\n");
    let body_len = open.len() as u64 + real_size + close.len() as u64;
    let ct = format!("multipart/related; type=\"application/dicom\"; boundary=\"{boundary}\"");
    let ka = ctx.keep_alive == KeepAlive::On;
    // The response head is built into the reusable per-connection buffer; the
    // opening part header is prepended so `sendfile_response` emits head+open,
    // then the file, and the closing boundary is written after.
    clarus_core::http::response::build_head(
        &mut ctx.conn.header,
        200,
        reason_phrase(200),
        Some(ct.as_bytes()),
        Some(body_len),
        ka,
        &[],
    );
    let mut head = std::mem::take(&mut ctx.conn.header);
    head.extend_from_slice(open.as_bytes());
    let res = if ctx.conn.suppress_body() {
        // HEAD: the response head (with the GET Content-Length) goes out, the
        // file bytes and the closing boundary do not.
        ctx.conn.stream().write_all(&head)
    } else {
        clarus_core::http::response::sendfile_response(ctx.conn.stream(), &head, &file, real_size)
            .and_then(|_| ctx.conn.write_all_configured(close.as_bytes()))
    };
    ctx.conn.header = head;
    res
}

/// The byte length of the closing boundary line that follows a part body.
///
/// `sendfile_response` cannot be used for a re-encoded instance, so the
/// `Content-Length` of that response is assembled here instead of from the
/// manifest's file size. Kept as a function so the prefix and the close are
/// spelled once and cannot drift into a body whose header lies about it.
fn close_len(boundary: &str) -> u64 {
    format!("\r\n--{boundary}--\r\n").len() as u64
}

/// D7: the single-instance multipart boundary is a per-process constant
/// (pid + counter) - build it once, not per request.
fn single_part_boundary() -> &'static str {
    static BOUNDARY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BOUNDARY.get_or_init(|| format!("clarus-boundary-{}-{}", std::process::id(), 0u64))
}

/// `GET /wadouri` / `/dicomweb/wadouri` - object selected by query params.
pub(crate) fn h_wado_uri(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    use crate::handlers::wado::types::WadoUriQuery;
    let Some(qs) = ctx.query_str() else {
        return wado_err_response(ctx, WadoError::InvalidUid("missing query".to_string()));
    };
    let query = match WadoUriQuery::parse(qs) {
        Some(q) => q,
        None => return wado_err_response(ctx, WadoError::InvalidUid("bad query".to_string())),
    };
    if query.request_type != "WADO" {
        return wado_err_response(ctx, WadoError::InvalidUid("bad requestType".to_string()));
    }
    if let Some(ct) = query.content_type.as_deref()
        && !ct.eq_ignore_ascii_case("application/dicom")
    {
        return wado_err_response(
            ctx,
            WadoError::NotAcceptable("unsupported contentType".to_string()),
        );
    }
    // WADO-URI is not the `wado_instance` route: the series aggregator does
    // not see it, so no series_uid is threaded (expected stays `None`).
    serve_single_object(ctx, &query.study_uid, None, &query.object_uid)
}

// -------------------------------------------------------------------------
// 2. WADO study/series multipart -> chunked
// -------------------------------------------------------------------------

/// Build a chunked multipart/related response for a study or series.
///
/// Strictly sequential, no `BufWriter` - every write goes straight to the
/// socket. The plan (stage 5) is resolved first; for each part we open the CAS
/// file *before* writing any part bytes, so a missing object is skipped without
/// emitting a phantom part. Wire layout (from stage 5, unchanged):
///
/// ```text
/// --BOUNDARY\r\n  (first part) | \r\n--BOUNDARY\r\n (later parts)
/// Content-Type: application/dicom; transfer-syntax=<actual part TS>\r\n
/// Content-Length: N\r\n
/// \r\n
/// ...N bytes...\r\n
///
/// --BOUNDARY--\r\n
/// 0\r\n\r\n                      (chunked terminator)
/// ```
///
/// The `transfer-syntax=` parameter (Step 0 of the 3008 stored-first
/// passthrough) always names the ACTUAL syntax of that part's bytes:
/// `source_ts` for StreamAsIs parts, the transcode target for TranscodeTo
/// parts. The bridge reads it byte-level per part and never trusts a parser.
///
/// Each MIME part is delivered as one HTTP chunk: the chunk length prefix plus
/// the part headers in a single write (so the parser never sees a partial
/// header), the body immediately after (sendfile for `StreamAsIs`, memory for
/// `TranscodeTo`), then the trailing CRLF that separates MIME parts.
fn serve_multipart_chunked(
    ctx: &mut RequestContext<'_>,
    plans: Vec<crate::handlers::wado_pure::PartPlan>,
    target_ts: Option<&str>,
) -> io::Result<()> {
    let boundary = clarus_adapters::boundary_id();
    let ka = ctx.keep_alive == KeepAlive::On;
    let content_type =
        format!("multipart/related; type=\"application/dicom\"; boundary=\"{boundary}\"");

    // Chunked head - Transfer-Encoding: chunked, no Content-Length.
    ctx.conn
        .chunked_start(200, "OK", content_type.as_bytes(), ka)?;

    // S3: the chunk terminator of part N is folded into the prefix write of
    // part N+1 (one set_timeouts instead of two per boundary); the last part's
    // terminator is flushed after the loop. Tracks a successfully-written
    // previous part so a skipped (missing/transcode-failed) part emits nothing.
    let mut pending_trailer = false;
    for (i, plan) in plans.iter().enumerate() {
        let is_first = i == 0;
        let kind = crate::handlers::wado_pure::resolve_part_kind(
            &plan.sop_class_uid,
            &plan.source_ts,
            target_ts,
        );

        // Resolve the file first - if it is gone we warn and skip cleanly.
        let file = match resolve_file(ctx.state, &plan.sha256, plan.storage_tier) {
            Ok(f) => f,
            Err(_) => {
                crate::clog_warn!(sha256 = %plan.sha256, "Multipart part missing on disk; skipping");
                continue;
            }
        };

        let boundary_header = crate::handlers::wado_pure::part_boundary_header(&boundary, is_first);
        // Step 0 (3008 stored-first): the part header names the ACTUAL
        // transfer syntax of the part bytes - the stored TS for as-is parts,
        // the transcode target for transcoded parts.
        let part_ts = match &kind {
            crate::handlers::wado_pure::PartKind::StreamAsIs => plan.source_ts.as_str(),
            crate::handlers::wado_pure::PartKind::TranscodeTo(ts) => ts.as_str(),
        };
        let ct_line = format!(
            "Content-Type: {}; transfer-syntax={part_ts}\r\n",
            String::from_utf8_lossy(DICOM_CT)
        );
        // Per-part `Content-Location` (DICOMweb path) - emitted at series-
        // AND study-level multipart. The series slot comes from the plan: a
        // study-level request has no series UID among ctx.uids.
        let cl_line = if plan.with_content_location {
            format!(
                "Content-Location: /dicomweb/studies/{}/series/{}/instances/{}\r\n",
                ctx.uid(0),
                plan.series_uid,
                plan.sop_uid
            )
        } else {
            String::new()
        };

        let (part_headers, body_mode) = match &kind {
            crate::handlers::wado_pure::PartKind::StreamAsIs => {
                // The part Content-Length must match the bytes actually sent:
                // take the REAL on-disk length of the opened CAS file. The
                // manifest `file_size` can be stale after a canonicalization
                // rewrite (e.g. placeholder PatientName / history append), and
                // a mismatched Content-Length desynchronizes Content-Length-
                // framed multipart consumers (bridge 02.09 field incident:
                // "Multipart buffer exceeded ... possible malformed response"
                // on an 85 MB angio study).
                let size = match file.metadata() {
                    Ok(m) => m.len(),
                    Err(e) => {
                        crate::clog_warn!(
                            sha256 = %plan.sha256,
                            error = %e,
                            "Multipart part stat failed; skipping"
                        );
                        continue;
                    }
                };
                let headers =
                    format!("{boundary_header}{ct_line}{cl_line}Content-Length: {size}\r\n\r\n");
                (headers, BodyMode::File(size))
            }
            crate::handlers::wado_pure::PartKind::TranscodeTo(ts) => {
                let bytes = match read_cas_bytes(ctx.state, &plan.sha256, plan.storage_tier) {
                    Ok(b) => b,
                    Err(_) => {
                        crate::clog_warn!(sha256 = %plan.sha256, "Multipart transcode read failed; skipping");
                        continue;
                    }
                };
                let (out, out_ts) = match crate::handlers::wado_pure::maybe_transcode(
                    bytes,
                    &plan.source_ts,
                    Some(ts),
                    &ctx.state.codec_registry,
                    &ctx.state.transcode_pool,
                    ctx.state.config.transcode.max_megapixels,
                ) {
                    Ok(v) => v,
                    // 2608 option (b): inside a chunked multipart stream a 503
                    // mid-body is impossible, so a saturated pool SKIPS the
                    // part (same graceful skip as a transcode failure) and
                    // keeps the stream intact. `Busy` is logged distinctly so
                    // the operator sees the saturation signal even though the
                    // wire carries no error here.
                    Err(WadoError::Busy) => {
                        crate::clog_warn!(sop_uid = %plan.sop_uid, "Multipart transcode: global pool busy; skipping part");
                        continue;
                    }
                    Err(e) => {
                        crate::clog_warn!(error = %e, "Multipart transcode failed; skipping");
                        continue;
                    }
                };
                // The part header must name the syntax of THESE bytes: a
                // transient decode failure serves the original bytes, so the
                // requested target would be a lie about the part body.
                let ct_line = format!(
                    "Content-Type: {}; transfer-syntax={out_ts}\r\n",
                    String::from_utf8_lossy(DICOM_CT)
                );
                let headers = format!(
                    "{boundary_header}{ct_line}{cl_line}Content-Length: {}\r\n\r\n",
                    out.len()
                );
                (headers, BodyMode::Bytes(out))
            }
        };

        // One write: the chunk length prefix glued to the part headers. For a
        // non-first part this single write also carries the PREVIOUS part's
        // chunk terminator `\r\n` (S3), so the boundary costs one write /
        // set_timeouts instead of two.
        let chunk_len = part_headers.len() as u64
            + match &body_mode {
                BodyMode::File(n) => *n,
                BodyMode::Bytes(b) => b.len() as u64,
            };
        let prefix = if pending_trailer {
            format!("\r\n{chunk_len:x}\r\n{part_headers}")
        } else {
            format!("{chunk_len:x}\r\n{part_headers}")
        };
        ctx.conn.write_all_configured(prefix.as_bytes())?;

        match body_mode {
            BodyMode::File(size) => {
                clarus_core::http::response::sendfile_response(
                    ctx.conn.stream(),
                    &[],
                    &file,
                    size,
                )?;
                // sendfile_response writes the body straight to the raw
                // stream, bypassing Conn::response_bytes - credit it here so
                // the access log `bytes=` (head + body) is honest for
                // streamed multipart parts.
                ctx.conn.add_response_bytes(size);
            }
            BodyMode::Bytes(bytes) => {
                ctx.conn.write_all_configured(&bytes)?;
            }
        }
        // This part is now written; its chunk terminator is folded into the
        // next part's prefix (or flushed after the loop).
        pending_trailer = true;
    }

    // The final part's chunk terminator (not folded forward).
    if pending_trailer {
        ctx.conn.write_all_configured(b"\r\n")?;
    }

    // Closing delimiter as its own chunk, then the chunked terminator.
    let closing = crate::handlers::wado_pure::closing_delimiter(&boundary);
    ctx.conn.chunked_data(closing.as_bytes())?;
    ctx.conn.chunked_end()
}

/// How a planned part delivers its body.
enum BodyMode {
    /// `StreamAsIs` - bytes come from the already-open CAS file.
    File(u64),
    /// `TranscodeTo` - bytes were transcoded in memory.
    Bytes(Vec<u8>),
}

/// `GET /dicomweb/studies/{study}` - whole study as chunked multipart, or the
/// QIDO-RS study JSON when the client prefers `application/dicom+json`.
///
/// PS3.18 overloads this path: 8.7 `RetrieveStudy` is the JSON transaction,
/// 10.4 `RetrieveStudy` is the WADO-RS one. The Accept header picks between
/// them (see [`classify_retrieve_accept`]); the JSON branch delegates to the
/// same search the SearchForStudies transaction uses, filtered to this UID,
/// so the two answers cannot drift apart.
pub(crate) fn h_wado_study(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    match classify_retrieve_accept(ctx.accept()) {
        RetrieveWanted::Json => return serve_retrieve_json_study(ctx),
        RetrieveWanted::Unacceptable => {
            return wado_err_response(ctx, not_acceptable(ctx.accept()));
        }
        RetrieveWanted::Wado => {}
    }
    let target_ts = crate::handlers::wado_pure::resolve_target_ts(&wado_query(ctx), ctx.accept());
    let plans =
        match crate::handlers::wado_pure::plan_study_multipart(ctx.state, ctx.uid(0), ctx.route.qa)
        {
            Ok(p) => p,
            Err(e) => return wado_err_response(ctx, e),
        };
    serve_multipart_chunked(ctx, plans, target_ts.as_deref())
}

/// `GET /dicomweb/studies/{study}/series/{series}` - one series multipart, or
/// the QIDO-RS series JSON on a JSON Accept (the same overload as the study).
pub(crate) fn h_wado_series(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    match classify_retrieve_accept(ctx.accept()) {
        RetrieveWanted::Json => return serve_retrieve_json_series(ctx),
        RetrieveWanted::Unacceptable => {
            return wado_err_response(ctx, not_acceptable(ctx.accept()));
        }
        RetrieveWanted::Wado => {}
    }
    let target_ts = crate::handlers::wado_pure::resolve_target_ts(&wado_query(ctx), ctx.accept());
    let plans = match crate::handlers::wado_pure::plan_series_multipart(
        ctx.state,
        ctx.uid(0),
        ctx.uid(1),
        ctx.route.qa,
    ) {
        Ok(p) => p,
        Err(e) => return wado_err_response(ctx, e),
    };
    serve_multipart_chunked(ctx, plans, target_ts.as_deref())
}

/// The QIDO-RS `RetrieveStudy` answer: the study as a DICOM JSON **object**,
/// which is what PS3.18 8.7.1.3 defines for this transaction (a search
/// returns an array; a retrieve returns the one resource).
///
/// Built by running the SearchForStudies transaction with this UID and
/// unwrapping the single result, so the attribute set is the search's own.
fn serve_retrieve_json_study(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let query = format!("StudyInstanceUID={}", ctx.uid(0));
    let rb = retrieve_base_of(ctx);
    let resp = match crate::handlers::qido_pure::qido_studies(
        ctx.state,
        Some(&query),
        Some(DICOM_JSON_MEDIA_TYPE),
        rb.as_deref(),
    ) {
        Ok(r) => r,
        Err(e) => return qido_err_response(ctx, e),
    };
    serve_single_json_from_array(ctx, resp)
}

/// The QIDO-RS `RetrieveSeries` answer: the series as a DICOM JSON object.
fn serve_retrieve_json_series(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let query = format!("SeriesInstanceUID={}", ctx.uid(1));
    let rb = retrieve_base_of(ctx);
    let resp = match crate::handlers::qido_pure::qido_series(
        ctx.state,
        ctx.uid(0),
        Some(&query),
        Some(DICOM_JSON_MEDIA_TYPE),
        rb.as_deref(),
    ) {
        Ok(r) => r,
        Err(e) => return qido_err_response(ctx, e),
    };
    serve_single_json_from_array(ctx, resp)
}

/// The QIDO-RS `RetrieveInstance` answer: the instance as a DICOM JSON object.
///
/// The instance metadata is rendered by the same helper the series-metadata
/// route uses, so an instance's JSON is identical whichever route serves it.
fn serve_retrieve_json_instance(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let plan = match crate::handlers::wado_pure::locate_object(
        ctx.state,
        ctx.uid(0),
        Some(ctx.uid(1)),
        ctx.uid(2),
        ctx.route.qa,
    ) {
        Ok((oref, _expected)) => oref,
        Err(e) => return wado_err_response(ctx, e),
    };
    // 0310 F3: metadata rendering stops at PixelData, so read only the bounded
    // header (`max_part_header`) instead of the whole instance. For a 13 GB
    // WSI this turns a 13 GB `read_to_end` into a <=4 MiB read; the output is
    // byte-identical (the renderer never needs the PixelData value).
    let head = match read_cas_head(
        ctx.state,
        &plan.sha256,
        plan.storage_tier,
        ctx.state.config.max_part_header,
    ) {
        Ok(b) => b,
        Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(plan.sha256)),
    };
    let json = match crate::handlers::wado_pure::metadata_from_dcm_bounded(
        &head,
        ctx.uid(0),
        ctx.uid(1),
        ctx.uid(2),
    ) {
        Ok(r) => r.body,
        Err(e) => return wado_err_response(ctx, e),
    };
    write_single_json(ctx, &json)
}

/// Turn a QIDO search response (an array) into the single-object answer the
/// retrieve transaction defines, using the first element. An empty result is
/// a 404, the same as an unknown UID on the retrieve path.
fn serve_single_json_from_array(
    ctx: &mut RequestContext<'_>,
    resp: crate::handlers::qido_pure::QidoResp,
) -> io::Result<()> {
    let body = unwrap_first_json_item(&resp.body);
    match body {
        Some(one) => write_single_json(ctx, &one),
        None => wado_err_response(ctx, WadoError::NotFound("study not found".to_string())),
    }
}

/// Extract the first element of a JSON array body, verbatim.
///
/// Deliberately a byte-level unwrap of `[a,b]` -> `a`: the elements are
/// already-rendered JSON, and re-parsing them would risk changing bytes the
/// frozen goldens pin. Returns `None` for an empty array.
fn unwrap_first_json_item(body: &[u8]) -> Option<Vec<u8>> {
    let trimmed = body.strip_prefix(b"[").unwrap_or(body).strip_suffix(b"]")?;
    if trimmed.is_empty() {
        return None;
    }
    // Walk to the end of the first top-level element, tracking nesting and
    // string literals so a comma inside a value does not split it early.
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in trimmed.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => return Some(trimmed[..i].to_vec()),
            _ => {}
        }
    }
    Some(trimmed.to_vec())
}

/// Write one DICOM JSON object with the QIDO content type.
fn write_single_json(ctx: &mut RequestContext<'_>, json: &[u8]) -> io::Result<()> {
    let ka = ctx.keep_alive == KeepAlive::On;
    clarus_core::http::response::build_head(
        &mut ctx.conn.header,
        200,
        reason_phrase(200),
        Some(crate::handlers::DICOM_JSON_CONTENT_TYPE.as_bytes()),
        Some(json.len() as u64),
        ka,
        &[],
    );
    ctx.conn.write_head_configured()?;
    ctx.conn.write_all_configured(json)
}

/// The base URL a QIDO item's `RetrieveURL` values are built from, mirroring
/// the dispatcher's own helper for the search routes.
fn retrieve_base_of(ctx: &RequestContext<'_>) -> Option<String> {
    let scheme = ctx.forwarded_proto().unwrap_or("http");
    let host = ctx.host()?;
    Some(format!("{scheme}://{host}"))
}

/// A `qido_pure` error as a response, reusing its shape so the JSON branch
/// reports a bad query exactly as the search route does.
fn qido_err_response(
    ctx: &mut RequestContext<'_>,
    err: crate::handlers::qido_pure::QidoError,
) -> io::Result<()> {
    let ka = ctx.keep_alive == KeepAlive::On;
    let body = format!("{{\"error\":{}}}", serde_json_string(&err.message));
    clarus_core::http::response::build_head(
        &mut ctx.conn.header,
        err.status,
        reason_phrase(err.status),
        Some(b"application/json"),
        Some(body.len() as u64),
        ka,
        &[],
    );
    ctx.conn.write_head_configured()?;
    ctx.conn.write_all_configured(body.as_bytes())
}

/// Minimal JSON string quoting for an error message (no serde dependency).
fn serde_json_string(message: &str) -> String {
    let mut out = String::with_capacity(message.len() + 2);
    out.push('"');
    for ch in message.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn wado_query(ctx: &RequestContext<'_>) -> crate::handlers::wado::types::WadoQueryParams {
    use crate::handlers::wado::types::WadoQueryParams;
    match ctx.query_str() {
        Some(qs) if !qs.is_empty() => WadoQueryParams::parse(qs),
        _ => WadoQueryParams::parse(""),
    }
}

/// True when the (lowercased) Accept header lists the given media type as one
/// A 406 for an Accept this resource cannot satisfy, naming what was asked
/// for so the client sees why. The body and status come from the existing
/// `WadoError::NotAcceptable` arm - no new error shape.
fn not_acceptable(accept: Option<&str>) -> WadoError {
    WadoError::NotAcceptable(format!(
        "cannot satisfy Accept: {}",
        accept.unwrap_or("<absent>")
    ))
}

/// The bare DICOM JSON media type, for comparing against a client's Accept.
///
/// Distinct from `handlers::DICOM_JSON_CONTENT_TYPE`, which is that type plus
/// `; charset=utf-8` for the *response* header. An Accept range names a media
/// type and MAY carry parameters, so the comparison must be against the type
/// alone - matching the response constant would never match a client that
/// sensibly sends `Accept: application/dicom+json`.
const DICOM_JSON_MEDIA_TYPE: &str = "application/dicom+json";

/// What a client asked for on a resource the standard overloads.
///
/// PS3.18 puts two transactions on one path: the QIDO-RS retrieve (8.7,
/// `application/dicom+json`) and WADO-RS retrieve (10.4,
/// `multipart/related; type="application/dicom"`). The Accept header is the
/// only distinguisher, so it is read here and nowhere else.
///
/// Public only so the ap101 allocation gate can call it from its isolated
/// test binary (`tests/ap101_accept_negotiation.rs`).
#[derive(Debug, PartialEq)]
pub enum RetrieveWanted {
    /// `application/dicom+json` is preferred -> the QIDO-RS JSON.
    Json,
    /// `multipart/related` (or a bare `application/dicom`) -> WADO-RS, or no
    /// preference at all, which keeps the WADO-RS default of this server.
    Wado,
    /// The client named types, none of which this resource can produce.
    Unacceptable,
}

/// Classify the Accept header for an overloaded retrieve resource.
///
/// Deliberately a simple ordered preference check over the comma-separated
/// ranges - media parameters are dropped, so `dicom+json;q=0.9` counts as a
/// preference for JSON. Full RFC 9110 negotiation (q-value ordering, wildcard
/// specificity) is not implemented and is not needed for these two choices.
///
/// A wildcard (`*/*`, `application/*`) or an absent header is "no
/// preference": this server answers WADO-RS by default, which is what every
/// deployed viewer already gets, so it must stay byte-identical.
///
/// Runs on every retrieve GET, so it compares the ranges in place without
/// allocating (pinned by the ap101 gate in tests/ap101_accept_negotiation.rs).
pub fn classify_retrieve_accept(accept: Option<&str>) -> RetrieveWanted {
    let Some(accept) = accept else {
        return RetrieveWanted::Wado;
    };

    let wants_json = accept_media_type_present_ci(accept, DICOM_JSON_MEDIA_TYPE);
    let wants_wado = accept_media_type_present_ci(accept, "multipart/related")
        || accept_media_type_present_ci(accept, "application/dicom");
    if wants_json {
        return RetrieveWanted::Json;
    }
    if wants_wado {
        return RetrieveWanted::Wado;
    }
    // No usable type named. A wildcard asks for "whatever you have", which is
    // the WADO-RS default; anything else is a type this resource cannot serve.
    let wildcard = accept_media_type_present_ci(accept, "*/*")
        || accept_media_type_present_ci(accept, "application/*");
    if wildcard {
        RetrieveWanted::Wado
    } else {
        RetrieveWanted::Unacceptable
    }
}

/// True when the Accept header lists the given media type as one of its
/// comma-separated ranges, compared case-insensitively in place - no
/// lowercased copy, because this runs on the hot retrieve path. The base of
/// each range must equal the media type exactly (modulo case), so
/// `application/dicom` never matches `application/dicom+json`.
///
/// Each range is compared through [`content_type_is`], so the Accept side and
/// the request side reduce a media type to its base with the SAME rule and
/// cannot come to disagree about one header.
fn accept_media_type_present_ci(accept: &str, media_type: &str) -> bool {
    accept
        .split(',')
        .any(|range| content_type_is(range, media_type))
}

/// True when a Content-Type header names the given media type, ignoring the
/// parameters after it.
///
/// A media type is `type/subtype *( ";" parameter )` (RFC 9110 8.3.1), and
/// the parameters are MODIFIERS of the base type rather than part of it: the
/// Accept side has taken the base before `;` for as long as it has existed,
/// and the request side must read a Content-Type the same way or the two
/// disagree about the same header. PS3.18 8.7.3.1 defines
/// `application/dicom` with an optional `transfer-syntax` parameter, so
/// `application/dicom; transfer-syntax=1.2.840.10008.1.2.1` IS an
/// `application/dicom` body - which is exactly the spelling a conformant
/// single-part STOW-RS client sends (10.5.1.4).
///
/// The comparison is against the substring before the FIRST `;`, so the
/// structured-syntax suffixes stay distinct: `application/dicom+json` carries
/// no parameter and its base is the whole string, which is not equal to
/// `application/dicom`. Case is ignored, in place, as on the Accept side.
///
/// An empty media type never matches: `;param` alone names no type.
fn content_type_is(content_type: &str, media_type: &str) -> bool {
    let base = content_type.split(';').next().unwrap_or("").trim();
    !base.is_empty() && base.eq_ignore_ascii_case(media_type)
}

// -------------------------------------------------------------------------
// 3. Buffered WADO responders (thumbnail / frame / bulkdata / metadata)
// -------------------------------------------------------------------------

/// `GET .../series/{se}/metadata` - every instance metadata as multipart/related
/// (the Required media type, PS3.18 10.4.4), OR as a single-part
/// `application/dicom+json` array when the client negotiates that type.
///
/// Content negotiation (02.09 field incidents): OHIF's dicomweb-client asks
/// `Accept: application/dicom+json` with `responseType='json'` and expects a
/// SINGLE JSON array; when the server answers multipart anyway, the browser
/// fails the JSON parse (response=null) and OHIF's multipart decoder throws
/// "Response message has no multipart mime header". Weasis asks
/// `multipart/related` explicitly and keeps the multipart path.
///
/// Default (no Accept header) is the single-part array - the exact behavior
/// of the SQL-era server this route was born with (docs/api.md: per-series
/// metadata array for cornerstone wadors), under which OHIF and Weasis were
/// field-verified. Multipart is emitted only on an explicit multipart Accept.
pub(crate) fn h_wado_series_metadata(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let plans = match crate::handlers::wado_pure::plan_series_multipart(
        ctx.state,
        ctx.uid(0),
        ctx.uid(1),
        ctx.route.qa,
    ) {
        Ok(p) => p,
        Err(e) => return wado_err_response(ctx, e),
    };
    let ka = ctx.keep_alive == KeepAlive::On;

    // Absent Accept OR explicit non-multipart accept (OHIF:
    // `application/dicom+json`) -> a single JSON array body.
    let wants_single_json = ctx
        .accept()
        .map(|a| !a.to_ascii_lowercase().contains("multipart/related"))
        .unwrap_or(true);
    if wants_single_json {
        let mut parts: Vec<Vec<u8>> = Vec::with_capacity(plans.len());
        for plan in &plans {
            if let Some(json) = instance_metadata_json(ctx, plan) {
                parts.push(json);
            }
        }
        let mut body =
            Vec::with_capacity(parts.iter().map(Vec::len).sum::<usize>() + parts.len() + 2);
        body.push(b'[');
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                body.push(b',');
            }
            body.extend_from_slice(part);
        }
        body.push(b']');
        clarus_core::http::response::build_head(
            &mut ctx.conn.header,
            200,
            reason_phrase(200),
            Some(crate::handlers::DICOM_JSON_CONTENT_TYPE.as_bytes()),
            Some(body.len() as u64),
            ka,
            &[],
        );
        ctx.conn.write_head_configured()?;
        return ctx.conn.write_all_configured(&body);
    }

    let boundary = clarus_adapters::boundary_id();
    let content_type = format!(
        "multipart/related; type=\"{}\"; boundary=\"{boundary}\"",
        crate::handlers::DICOM_JSON_CONTENT_TYPE
    );
    ctx.conn
        .chunked_start(200, "OK", content_type.as_bytes(), ka)?;

    for (i, plan) in plans.iter().enumerate() {
        let bh = crate::handlers::wado_pure::part_boundary_header(&boundary, i == 0);
        let json = match instance_metadata_json(ctx, plan) {
            Some(b) => b,
            None => continue,
        };
        let part = format!(
            "{bh}Content-Type: {}\r\nContent-Length: {}\r\n\r\n",
            crate::handlers::DICOM_JSON_CONTENT_TYPE,
            json.len()
        );
        let chunk_len = (part.len() + json.len()) as u64;
        let prefix = format!("{chunk_len:x}\r\n{part}");
        ctx.conn.write_all_configured(prefix.as_bytes())?;
        ctx.conn.write_all_configured(&json)?;
        ctx.conn.write_all_configured(b"\r\n")?;
    }

    let closing = crate::handlers::wado_pure::closing_delimiter(&boundary);
    ctx.conn.chunked_data(closing.as_bytes())?;
    ctx.conn.chunked_end()
}

/// `GET /dicomweb/studies/{study}/metadata` - the Study Metadata Resource
/// (PS3.18 10.4.1.1.2 Table 10.4.1-2): the metadata of EVERY instance in the
/// study, in study order. Same body negotiation as the series form: absent or
/// non-multipart Accept -> a single `application/dicom+json` array; a
/// multipart Accept -> `multipart/related; type="application/dicom+json"`.
///
/// The study route carries only one UID, so the per-instance series UID comes
/// from the part plan (each instance's own series), not from `ctx.uid(1)`.
pub(crate) fn h_wado_study_metadata(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let plans =
        match crate::handlers::wado_pure::plan_study_multipart(ctx.state, ctx.uid(0), ctx.route.qa)
        {
            Ok(p) => p,
            Err(e) => return wado_err_response(ctx, e),
        };
    let ka = ctx.keep_alive == KeepAlive::On;

    // Absent Accept OR explicit non-multipart accept (OHIF:
    // `application/dicom+json`) -> a single JSON array body.
    let wants_single_json = ctx
        .accept()
        .map(|a| !a.to_ascii_lowercase().contains("multipart/related"))
        .unwrap_or(true);
    if wants_single_json {
        let mut parts: Vec<Vec<u8>> = Vec::with_capacity(plans.len());
        for plan in &plans {
            if let Some(json) = instance_metadata_json_at(ctx, plan) {
                parts.push(json);
            }
        }
        let mut body =
            Vec::with_capacity(parts.iter().map(Vec::len).sum::<usize>() + parts.len() + 2);
        body.push(b'[');
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                body.push(b',');
            }
            body.extend_from_slice(part);
        }
        body.push(b']');
        clarus_core::http::response::build_head(
            &mut ctx.conn.header,
            200,
            reason_phrase(200),
            Some(crate::handlers::DICOM_JSON_CONTENT_TYPE.as_bytes()),
            Some(body.len() as u64),
            ka,
            &[],
        );
        ctx.conn.write_head_configured()?;
        return ctx.conn.write_all_configured(&body);
    }

    let boundary = clarus_adapters::boundary_id();
    let content_type = format!(
        "multipart/related; type=\"{}\"; boundary=\"{boundary}\"",
        crate::handlers::DICOM_JSON_CONTENT_TYPE
    );
    ctx.conn
        .chunked_start(200, "OK", content_type.as_bytes(), ka)?;

    for (i, plan) in plans.iter().enumerate() {
        let bh = crate::handlers::wado_pure::part_boundary_header(&boundary, i == 0);
        let json = match instance_metadata_json_at(ctx, plan) {
            Some(b) => b,
            None => continue,
        };
        let part = format!(
            "{bh}Content-Type: {}\r\nContent-Length: {}\r\n\r\n",
            crate::handlers::DICOM_JSON_CONTENT_TYPE,
            json.len()
        );
        let chunk_len = (part.len() + json.len()) as u64;
        let prefix = format!("{chunk_len:x}\r\n{part}");
        ctx.conn.write_all_configured(prefix.as_bytes())?;
        ctx.conn.write_all_configured(&json)?;
        ctx.conn.write_all_configured(b"\r\n")?;
    }

    let closing = crate::handlers::wado_pure::closing_delimiter(&boundary);
    ctx.conn.chunked_data(closing.as_bytes())?;
    ctx.conn.chunked_end()
}

/// Render one instance's metadata natively from CAS (no SQLite metadata_blob):
/// read the `.dcm` planned from the ALREADY-LOADED manifest, render + inject
/// via the serde-free path. A10: the part plan carries the CAS key and tier
/// resolved by `plan_series_multipart`, so no per-part `locate_object` (which
/// would re-load the whole manifest N times). Returns `None` (part skipped)
/// if the CAS object is missing.
fn instance_metadata_json(
    ctx: &RequestContext<'_>,
    plan: &crate::handlers::wado_pure::PartPlan,
) -> Option<Vec<u8>> {
    // 0310 F3: bounded header read (rendering stops at PixelData).
    let head = read_cas_head(
        ctx.state,
        &plan.sha256,
        plan.storage_tier,
        ctx.state.config.max_part_header,
    )
    .ok()?;
    crate::handlers::wado_pure::metadata_from_dcm_bounded(
        &head,
        ctx.uid(0),
        ctx.uid(1),
        &plan.sop_uid,
    )
    .ok()
    .map(|r| r.body)
}

/// The study-level twin of [`instance_metadata_json`]: the request carries a
/// single study UID, so the series UID used to build each instance's
/// `BulkDataURI` paths (PS3.18 10.4.1.1.3) comes from the part plan itself -
/// every instance's own (0020,000E) - not from a route segment.
fn instance_metadata_json_at(
    ctx: &RequestContext<'_>,
    plan: &crate::handlers::wado_pure::PartPlan,
) -> Option<Vec<u8>> {
    let head = read_cas_head(
        ctx.state,
        &plan.sha256,
        plan.storage_tier,
        ctx.state.config.max_part_header,
    )
    .ok()?;
    crate::handlers::wado_pure::metadata_from_dcm_bounded(
        &head,
        ctx.uid(0),
        &plan.series_uid,
        &plan.sop_uid,
    )
    .ok()
    .map(|r| r.body)
}

/// `GET .../instances/{sop}/thumbnail` (or `/rendered`) - a JPEG thumbnail.
/// C1 (22.08): no longer feature-gated. The `_any` wrapper checks the
/// transfer syntax itself - uncompressed -> raw path -> 200; compressed ->
/// transcode decode (see `_any`) in a transcode build, 406 in the default
/// build (wire-compatible with the old feature-gate 406).
///
/// PS3.18 10.4.1.1.4 Thumbnail Resource. The optional `viewport` parameter
/// (8.3.5.1.3/8.3.5.2) is resolved by [`thumbnail_request`], which also
/// enforces the Accept negotiation both the media type table (10.4.4) and the
/// viewport rules require.
/// Refuse an instance whose stored size exceeds `[storage] max_instance_size_mb`.
///
/// Every thumbnail renderer reads the WHOLE instance into memory before
/// scaling it down, so the instance size is the real allocation and the limit
/// is the one STOW already accepted. The instance path had this check and the
/// series and study paths did not, which is the same class one endpoint over:
/// a multi-GB slide was pulled in whole to produce a 128-pixel preview.
fn guard_instance_size(ctx: &RequestContext<'_>, file_size: u64) -> Result<(), WadoError> {
    let max_instance = ctx
        .state
        .config
        .max_instance_size_mb
        .saturating_mul(1024 * 1024);
    if file_size > max_instance {
        return Err(WadoError::PayloadTooLargeStatic(
            "instance exceeds [storage] max_instance_size_mb; cannot render a thumbnail",
        ));
    }
    Ok(())
}

pub(crate) fn h_wado_thumbnail(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let target = match thumbnail_request(ctx) {
        Ok(t) => t,
        Err(e) => return wado_err_response(ctx, e),
    };
    // Expected is irrelevant here (thumbnail is not a series slice); the
    // series UID is passed only to satisfy the aggregator lookup shape.
    let (oref, _expected) = match crate::handlers::wado_pure::locate_object(
        ctx.state,
        ctx.uid(0),
        Some(ctx.uid(1)),
        ctx.uid(2),
        ctx.route.qa,
    ) {
        Ok(v) => v,
        Err(e) => return wado_err_response(ctx, e),
    };
    // A thumbnail is rendered from the WHOLE instance held in memory, so the
    // instance size is the real allocation. Refusing here keeps a multi-GB
    // slide from being pulled in to produce a 128-pixel preview; the limit is
    // the same one STOW accepted, so nothing served can exceed it.
    if let Err(e) = guard_instance_size(ctx, oref.file_size) {
        return wado_err_response(ctx, e);
    }
    let bytes = match read_cas_bytes(ctx.state, &oref.sha256, oref.storage_tier) {
        Ok(b) => b,
        Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(oref.sha256)),
    };
    let mut resp = match crate::handlers::wado_pure::render_thumbnail_from_bytes_any(
        &bytes,
        ctx.uid(2),
        target,
        &ctx.state.codec_registry,
        &ctx.state.transcode_pool,
        ctx.state.config.transcode.max_megapixels,
    ) {
        Ok(r) => r,
        Err(e) => return wado_err_response(ctx, e),
    };
    // ETag is the quoted CAS key (BLAKE3 hex) - the wrapper (this handler)
    // owns the CAS key, so it injects the header here. `write_wado_resp`
    // forwards `resp.headers` onto the wire, so the ETag lands in the head
    // alongside the Cache-Control already set by the renderer.
    resp.headers.push(("ETag", format!("\"{}\"", oref.sha256)));
    write_wado_resp(ctx, &resp)
}

/// 0410: `GET .../instances/{sop}/rendered` - the Rendered Resource
/// (PS3.18 10.4.1.1.3 Rendered Resources; Rendered Media Types 8.7.4;
/// Rendering Query Parameters 8.3.5). Split from `/thumbnail`
/// (10.4.1.1.4 Thumbnail Resources; media types 10.4.4 Table 10.4.4-1;
/// thumbnail params 8.3.5.2, JPEG-only) so it can answer
/// `image/png`, which the Slim microscopy viewer's OVERVIEW/THUMBNAIL/LABEL
/// preview requests EXACTLY (`Accept: image/png`).
///
/// Bounded and O(frame): a bounded header read gives TS/NumberOfFrames/SPP/
/// Photometric/PlanarConfiguration/Rows/Cols/BitsAllocated; multi-frame
/// (VOLUME) and compressed instances are rejected BEFORE any full read, so a
/// 13 GB pyramid is never read whole. Single-frame instances are then read
/// (bounded by the pixel cap) and rendered.
///
/// Accept table (verdict 1.2.2, extended 1310 for D1):
///   image/png  -> PNG (RGB or grayscale)
///   image/gif  -> GIF, MONO only; RGB + image/gif -> 406
///   image/jpeg -> JPEG, MONO only; RGB + image/jpeg -> 406
///   absent, */*, image/* -> image/jpeg (default)
///   any other named type -> 406
pub(crate) fn h_wado_rendered(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    use clarus_adapters::dicom_rs::DicomRsParser;

    // STEP 1: negotiate the Accept header. `RenderedAccept` distinguishes the
    // explicit png/jpeg requests from the "no preference" default.
    let accept = match rendered_accept(ctx.accept()) {
        Ok(a) => a,
        Err(e) => return wado_err_response(ctx, e),
    };

    // STEP 2: locate the instance.
    let (oref, _expected) = match crate::handlers::wado_pure::locate_object(
        ctx.state,
        ctx.uid(0),
        Some(ctx.uid(1)),
        ctx.uid(2),
        ctx.route.qa,
    ) {
        Ok(v) => v,
        Err(e) => return wado_err_response(ctx, e),
    };

    // STEP 3: bounded header read - only `max_part_header` bytes.
    let head = match read_cas_head(
        ctx.state,
        &oref.sha256,
        oref.storage_tier,
        ctx.state.config.max_part_header,
    ) {
        Ok(b) => b,
        Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(oref.sha256)),
    };
    let parser = DicomRsParser;
    // Transfer syntax comes from the MANIFEST (`oref.transfer_syntax`, the
    // STOW-time value), NOT from re-parsing the head: `DicomReader::new`
    // consumes the group-0002 meta-header during construction, so
    // `extract_tag_value(&head, (0x0002,0x0010))` ALWAYS returned None and the
    // compressed-TS guard below was dead code.
    let rows = head_u16(&parser, &head, (0x0028, 0x0010)).unwrap_or(0) as u32;
    let cols = head_u16(&parser, &head, (0x0028, 0x0011)).unwrap_or(0) as u32;
    let spp = head_u16(&parser, &head, (0x0028, 0x0002)).unwrap_or(1);
    let bits_allocated = head_u16(&parser, &head, (0x0028, 0x0100)).unwrap_or(8) as u32;
    let pixel_rep = head_u16(&parser, &head, (0x0028, 0x0103)).unwrap_or(0);
    let planar_config = head_u16(&parser, &head, (0x0028, 0x0006)).unwrap_or(0);
    let num_frames = head_u32(&parser, &head, (0x0028, 0x0008)).unwrap_or(1);
    let photometric = parser
        .extract_tag_value(&head, (0x0028, 0x0004))
        .ok()
        .flatten()
        .map(|v| String::from_utf8_lossy(&v).trim_matches(' ').to_string())
        .unwrap_or_default();

    // STEP 4: reject BEFORE any full read.
    // A multi-frame (VOLUME) instance must never be read whole - clients
    // retrieve pyramid frames via `/frames/{N}`.
    if num_frames > 1 {
        return wado_err_response(
            ctx,
            WadoError::NotAcceptable(
                "multi-frame instances are not server-rendered; use /frames/{N}".to_string(),
            ),
        );
    }
    // A compressed instance is decoded further down (see the MONOCHROME
    // Jpeg arm): the guard that used to fire HERE ran before the capable
    // path, so a valid single-frame JPEG-Lossless instance answered 406 on
    // an Accept the server can satisfy - a 10.4.3.3.3 violation. The syntax
    // stays authoritative from the manifest (see STEP 3).
    //
    // Decoding to PNG or GIF is NOT wired: that refusal keeps firing from the
    // header, BEFORE the whole blob is read, so an unsatisfiable Accept still
    // costs no read (the cap-read accounting depends on it).
    let compressed_ts = is_compressed_transfer_syntax_str(&oref.transfer_syntax);
    if compressed_ts && (accept == RenderedAccept::Png || accept == RenderedAccept::Gif) {
        return wado_err_response(
            ctx,
            WadoError::NotAcceptable("rendered PNG/GIF from compressed not wired".to_string()),
        );
    }
    // Only 1 (gray) or 3 (RGB) samples per pixel are renderable here.
    if spp != 1 && spp != 3 {
        return wado_err_response(
            ctx,
            WadoError::NotAcceptable(format!("rendered samples-per-pixel {spp} unsupported")),
        );
    }
    // JPEG and GIF are MONOCHROME-only on this route (RGB + either -> 406).
    // GIF carries no quantiser, so an RGB gif would need one; PNG is the
    // lossless RGB answer and is what the route already serves.
    let is_rgb = spp == 3;
    if is_rgb && (accept == RenderedAccept::Jpeg || accept == RenderedAccept::Gif) {
        return wado_err_response(
            ctx,
            WadoError::NotAcceptable("rendered RGB requires Accept: image/png".to_string()),
        );
    }
    // RGB is only handled for the interleaved 8-bit case.
    if is_rgb && (bits_allocated != 8 || planar_config != 0) {
        return wado_err_response(
            ctx,
            WadoError::NotAcceptable(
                "rendered RGB requires 8-bit interleaved (PlanarConfiguration 0)".to_string(),
            ),
        );
    }
    if rows == 0 || cols == 0 {
        return wado_err_response(
            ctx,
            WadoError::Internal(format!("Invalid dimensions: {rows}x{cols}")),
        );
    }
    // STEP 4 (cont.): the render pixel budget fires HERE, BEFORE the full
    // read - so an over-budget single-frame instance is rejected from the
    // header alone and `read_cas_bytes` never reads the whole blob (the F3
    // "reject before full read" rule). The renderers keep their own check as
    // defense-in-depth (same shared bound).
    if let Err(e) = crate::handlers::wado_pure::check_render_pixel_budget(rows, cols) {
        return wado_err_response(ctx, e);
    }

    // STEP 5: bounded full read (single-frame + pixel cap below).
    let target = match rendered_viewport(ctx, (cols, rows)) {
        Ok(t) => t,
        Err(e) => return wado_err_response(ctx, e),
    };
    // Instrumentation: count how many times `/rendered` reaches the WHOLE-blob
    // read. The 413 pre-read gate above must keep this at zero for an
    // over-budget instance - the wire test asserts it did not move.
    RENDERED_FULL_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let bytes = match read_cas_bytes(ctx.state, &oref.sha256, oref.storage_tier) {
        Ok(b) => b,
        Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(oref.sha256)),
    };

    // STEP 6: render. The renderer re-extracts PixelData from `bytes`.
    let pixel_data = match parser.extract_tag_value(&bytes, (0x7FE0, 0x0010)) {
        Ok(Some(pd)) => pd,
        Ok(None) => {
            return wado_err_response(ctx, WadoError::NotFound("PixelData not found".into()));
        }
        Err(e) => {
            return wado_err_response(
                ctx,
                WadoError::Internal(format!("Failed to parse DICOM for rendered: {e}")),
            );
        }
    };

    let mut resp = if is_rgb {
        // RGB path is always PNG (a jpeg request over RGB was rejected above).
        match crate::handlers::wado_pure::render_rgb_png(
            &pixel_data,
            crate::handlers::wado_pure::RgbGeometry {
                rows,
                cols,
                bits_allocated,
            },
            target,
        ) {
            Ok(r) => r,
            Err(e) => return wado_err_response(ctx, e),
        }
    } else {
        // MONOCHROME: image/jpeg (default), image/png or image/gif (Gray8).
        let geom = crate::handlers::wado_pure::RawGeometry {
            rows,
            cols,
            bits_allocated,
            pixel_rep,
            invert: photometric == "MONOCHROME1",
        };
        let rendered = match accept {
            RenderedAccept::Png => {
                crate::handlers::wado_pure::render_gray_png(&pixel_data, geom, &bytes, target)
            }
            RenderedAccept::Gif => {
                crate::handlers::wado_pure::render_gray_gif(&pixel_data, geom, &bytes, target)
            }
            // PS3.18 8.7.4-1: image/jpeg is the single-frame Default. A
            // compressed source is decoded by the codec registry -
            // `render_full_from_bytes` routes through `render_full_compressed`
            // under `feature = "transcode"` (this build) and answers 406 only
            // in a default build, so the honest 406 remains where the decoder
            // is genuinely absent instead of firing before the attempt.
            RenderedAccept::Jpeg if compressed_ts => {
                crate::handlers::wado_pure::render_full_from_bytes(
                    &bytes,
                    ctx.uid(2),
                    &ctx.state.codec_registry,
                    &ctx.state.transcode_pool,
                    ctx.state.config.transcode.max_megapixels,
                )
            }
            RenderedAccept::Jpeg => crate::handlers::wado_pure::render_pixels_thumbnail(
                &pixel_data,
                geom,
                &bytes,
                target,
                ctx.uid(2),
            ),
        };
        match rendered {
            Ok(r) => r,
            Err(e) => return wado_err_response(ctx, e),
        }
    };
    resp.headers.push(("ETag", format!("\"{}\"", oref.sha256)));
    write_wado_resp(ctx, &resp)
}

/// The `/rendered` Accept decision (0410, verdict 1.2.2; Gif added 1310).
#[derive(PartialEq, Eq, Clone, Copy)]
enum RenderedAccept {
    Png,
    Gif,
    Jpeg,
}

/// Negotiate the `/rendered` Accept header. `image/png` -> PNG; `image/gif` ->
/// GIF; `image/jpeg` -> JPEG; absent / `*/*` / `image/*` -> the image/jpeg
/// default; anything else named -> 406. The new gif test sits after the png
/// and jpeg tests but before the wildcard test, so no existing header's
/// outcome changes.
fn rendered_accept(accept: Option<&str>) -> Result<RenderedAccept, WadoError> {
    let Some(accept) = accept else {
        return Ok(RenderedAccept::Jpeg);
    };
    let wildcard = accept_media_type_present_ci(accept, "*/*")
        || accept_media_type_present_ci(accept, "image/*");
    let png = accept_media_type_present_ci(accept, "image/png");
    let gif = accept_media_type_present_ci(accept, "image/gif");
    let jpeg = accept_media_type_present_ci(accept, "image/jpeg");
    if png {
        return Ok(RenderedAccept::Png);
    }
    if gif {
        return Ok(RenderedAccept::Gif);
    }
    if jpeg {
        return Ok(RenderedAccept::Jpeg);
    }
    if wildcard {
        return Ok(RenderedAccept::Jpeg);
    }
    Err(WadoError::NotAcceptable(
        "rendered requires Accept: image/png, image/gif or image/jpeg".to_string(),
    ))
}

/// Resolve the `/rendered` render size: ABSENT viewport -> NATIVE size (Slim's
/// `resizeFactor == 1`; do NOT default to 64 - that would destroy the
/// 609x1600 overview); PRESENT -> `ThumbnailViewport::parse` (same syntax/range
/// gate as `/thumbnail`, 400 on ill-formed) as the fit-in box.
fn rendered_viewport(
    ctx: &RequestContext<'_>,
    native: (u32, u32),
) -> Result<(u32, u32), WadoError> {
    use crate::handlers::wado::types::{ThumbnailViewport, ViewportError, parse_query_string};
    let Some(qs) = ctx.query_str() else {
        return Ok(native);
    };
    let pairs = parse_query_string(qs);
    let Some((_, value)) = pairs.iter().find(|(k, _)| k == "viewport") else {
        return Ok(native);
    };
    match ThumbnailViewport::parse(value) {
        Ok(v) => Ok((v.width, v.height)),
        Err(ViewportError::IllFormed) => Err(WadoError::BadRequest(format!(
            "viewport must be two positive integers 'vw,vh', got '{value}'"
        ))),
        Err(ViewportError::NotSupported) => Err(WadoError::BadRequest(format!(
            "viewport '{value}' exceeds the supported maximum of {}x{}",
            crate::handlers::wado::types::MAX_THUMBNAIL_VIEWPORT,
            crate::handlers::wado::types::MAX_THUMBNAIL_VIEWPORT
        ))),
    }
}

/// Read a u16 tag value from a bounded header via the parser.
fn head_u16(
    parser: &clarus_adapters::dicom_rs::DicomRsParser,
    head: &[u8],
    tag: (u16, u16),
) -> Option<u16> {
    let val = parser.extract_tag_value(head, tag).ok()??;
    match val.len() {
        0 => None,
        1 => Some(val[0] as u16),
        _ => Some(u16::from_le_bytes([val[0], val[1]])),
    }
}

/// Read a u32-ish (UL/US/IS) tag value from a bounded header.
fn head_u32(
    parser: &clarus_adapters::dicom_rs::DicomRsParser,
    head: &[u8],
    tag: (u16, u16),
) -> Option<u32> {
    let val = parser.extract_tag_value(head, tag).ok()??;
    let text = String::from_utf8_lossy(&val);
    let trimmed = text.trim_matches([' ', '\0']);
    if trimmed.is_empty() {
        return None;
    }
    // IS (text) first; fall back to a little-endian binary read.
    trimmed.parse::<u32>().ok().or_else(|| match val.len() {
        0 => None,
        1 => Some(val[0] as u32),
        2 => Some(u16::from_le_bytes([val[0], val[1]]) as u32),
        _ => Some(u32::from_le_bytes([val[0], val[1], val[2], val[3]])),
    })
}

/// Is a transfer-syntax UID one this route cannot render (compressed)?
fn is_compressed_transfer_syntax_str(ts: &str) -> bool {
    // The three uncompressed syntaxes are renderable; everything else
    // (JPEG/JPEG-LS/JPEG2000/RLE/Deflated) needs a decode.
    !matches!(
        ts.trim_matches('\0'),
        "1.2.840.10008.1.2" | "1.2.840.10008.1.2.1" | "1.2.840.10008.1.2.2"
    ) && !ts.trim_matches('\0').is_empty()
}

/// Resolve a Thumbnail Resource request before any bytes are read: negotiate
/// the Accept header and the optional `viewport` parameter.
///
/// PS3.18 10.4.4 Table 10.4.4-1 lists exactly one media type for Thumbnail
/// Resources, `image/jpeg` (Default), so a client that names types and none
/// of them is acceptable gets a 406 (10.4.4) rather than a JPEG it did not
/// ask for. An absent or wildcard Accept is "no preference" and gets the
/// default `image/jpeg`, which is what every deployed viewer already sends.
///
/// The viewport rules are the standard's own, not a lenient variant:
/// absent -> this server picks the size (8.3.5.2); ill-formed or outside the
/// supported range -> 400 (8.3.5.1.3). See
/// [`crate::handlers::wado::types::ThumbnailViewport`].
fn thumbnail_request(ctx: &RequestContext<'_>) -> Result<(u32, u32), WadoError> {
    use crate::handlers::wado::types::{
        DEFAULT_THUMBNAIL_VIEWPORT, ThumbnailViewport, ViewportError,
    };

    // Accept: image/jpeg is the only Thumbnail media type (Table 10.4.4-1).
    if let Some(accept) = ctx.accept() {
        let named = accept_media_type_present_ci(accept, "image/jpeg");
        let wildcard = accept_media_type_present_ci(accept, "*/*")
            || accept_media_type_present_ci(accept, "image/*");
        if !named && !wildcard {
            return Err(WadoError::NotAcceptable(
                "thumbnail requires Accept: image/jpeg".to_string(),
            ));
        }
    }

    let Some(qs) = ctx.query_str() else {
        return Ok((DEFAULT_THUMBNAIL_VIEWPORT, DEFAULT_THUMBNAIL_VIEWPORT));
    };
    let pairs = crate::handlers::wado::types::parse_query_string(qs);
    let Some((_, value)) = pairs.iter().find(|(k, _)| k == "viewport") else {
        return Ok((DEFAULT_THUMBNAIL_VIEWPORT, DEFAULT_THUMBNAIL_VIEWPORT));
    };
    match ThumbnailViewport::parse(value) {
        Ok(v) => Ok((v.width, v.height)),
        Err(ViewportError::IllFormed) => Err(WadoError::BadRequest(format!(
            "viewport must be two positive integers 'vw,vh', got '{value}'"
        ))),
        Err(ViewportError::NotSupported) => Err(WadoError::BadRequest(format!(
            "viewport '{value}' exceeds the supported maximum of {}x{}",
            crate::handlers::wado::types::MAX_THUMBNAIL_VIEWPORT,
            crate::handlers::wado::types::MAX_THUMBNAIL_VIEWPORT
        ))),
    }
}

/// `GET .../series/{se}/thumbnail` (or `/rendered`) - first instance thumbnail.
/// C1: same as instance - transparent to the transfer-syntax gate inside the
/// `_any` wrapper: uncompressed -> 200; compressed -> transcode decode
/// (see `_any`), 406 in the default build.
pub(crate) fn h_wado_series_thumbnail(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let target = match thumbnail_request(ctx) {
        Ok(t) => t,
        Err(e) => return wado_err_response(ctx, e),
    };
    let plans = match crate::handlers::wado_pure::plan_series_multipart(
        ctx.state,
        ctx.uid(0),
        ctx.uid(1),
        ctx.route.qa,
    ) {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => return wado_err_response(ctx, WadoError::NotFound("series".to_string())),
        Err(e) => return wado_err_response(ctx, e),
    };
    let first = &plans[0];
    // Same guard as the instance path: the representative instance is read
    // whole to render the preview, so an oversized one is refused before the
    // read rather than after it.
    if let Err(e) = guard_instance_size(ctx, first.file_size) {
        return wado_err_response(ctx, e);
    }
    let bytes = match read_cas_bytes(ctx.state, &first.sha256, first.storage_tier) {
        Ok(b) => b,
        Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(first.sha256.clone())),
    };
    let resp = match crate::handlers::wado_pure::render_thumbnail_from_bytes_any(
        &bytes,
        &first.sop_uid,
        target,
        &ctx.state.codec_registry,
        &ctx.state.transcode_pool,
        ctx.state.config.transcode.max_megapixels,
    ) {
        Ok(r) => r,
        Err(e) => return wado_err_response(ctx, e),
    };
    write_wado_resp(ctx, &resp)
}

/// C2 (B3 all-or-none, PS3.18 10.4.1.1.4): `GET .../studies/{study}/thumbnail` -
/// thumbnail of the study's representative instance (first plan of the study).
/// Same `_any` path as instance/series; uncompressed -> 200, compressed ->
/// transcode decode (see `_any`).
pub(crate) fn h_wado_study_thumbnail(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let target = match thumbnail_request(ctx) {
        Ok(t) => t,
        Err(e) => return wado_err_response(ctx, e),
    };
    let plans =
        match crate::handlers::wado_pure::plan_study_multipart(ctx.state, ctx.uid(0), ctx.route.qa)
        {
            Ok(p) if !p.is_empty() => p,
            Ok(_) => return wado_err_response(ctx, WadoError::NotFound("study".to_string())),
            Err(e) => return wado_err_response(ctx, e),
        };
    let first = &plans[0];
    // Same guard as the instance path (see `guard_instance_size`).
    if let Err(e) = guard_instance_size(ctx, first.file_size) {
        return wado_err_response(ctx, e);
    }
    let bytes = match read_cas_bytes(ctx.state, &first.sha256, first.storage_tier) {
        Ok(b) => b,
        Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(first.sha256.clone())),
    };
    let resp = match crate::handlers::wado_pure::render_thumbnail_from_bytes_any(
        &bytes,
        &first.sop_uid,
        target,
        &ctx.state.codec_registry,
        &ctx.state.transcode_pool,
        ctx.state.config.transcode.max_megapixels,
    ) {
        Ok(r) => r,
        Err(e) => return wado_err_response(ctx, e),
    };
    write_wado_resp(ctx, &resp)
}

/// C2 (B3 all-or-none, PS3.18 10.4.1.1.4): `.../instances/{sop}/frames/{n}/thumbnail` -
/// thumbnail of frame N of the instance. Any-TS path via
/// `render_frame_thumbnail_any`.
pub(crate) fn h_wado_frame_thumbnail(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let target = match thumbnail_request(ctx) {
        Ok(t) => t,
        Err(e) => return wado_err_response(ctx, e),
    };
    // Frame index is the penultimate segment (`.../frames/{n}/thumbnail`); the
    // router validates it is digits but does not capture it. `path_tail_segment`
    // returns "thumbnail"; re-parse the frame from the second-to-last segment.
    let path = std::str::from_utf8(ctx.head.path).unwrap_or("");
    let frame = path.rsplit('/').nth(1).unwrap_or("").to_string();
    // Expected is irrelevant here (a frame thumbnail is not a series slice).
    let (oref, _expected) = match crate::handlers::wado_pure::locate_object(
        ctx.state,
        ctx.uid(0),
        Some(ctx.uid(1)),
        ctx.uid(2),
        ctx.route.qa,
    ) {
        Ok(v) => v,
        Err(e) => return wado_err_response(ctx, e),
    };
    // 0310 F3 (D3): the thumbnail decodes ONE frame, but the decoder wants a
    // full instance slice (it looks up geometry/VOI/PixelData by tag). For a
    // multi-GB WSI that slice is the whole instance. When the manifest table
    // gives us the frame's byte range, build a BOUNDED buffer (head + that one
    // frame) instead of reading the whole blob; otherwise fall back to a
    // guarded whole-blob read that refuses a multi-GB instance.
    let max_frame_bytes = ctx.state.config.max_frame_bytes;
    let frame_number: Option<u32> = frame.parse().ok();
    let chunk = frame_number.and_then(|n| oref.frames.iter().find(|f| f.number == n));
    let large = oref.file_size > max_frame_bytes;
    let bytes = match chunk {
        Some(f) if large => {
            // Bounded: read the head + the single frame chunk, splice them.
            let head = match read_cas_head(
                ctx.state,
                &oref.sha256,
                oref.storage_tier,
                ctx.state.config.max_part_header,
            ) {
                Ok(b) => b,
                Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(oref.sha256)),
            };
            let chunk_bytes = match read_cas_range(
                ctx.state,
                &oref.sha256,
                oref.storage_tier,
                f.offset,
                f.compressed_len,
            ) {
                Ok(b) => b,
                Err(_) => return wado_err_response(ctx, WadoError::RangeNotSatisfiable),
            };
            match clarus_adapters::dicom_rs::frame::splice_frame_into_head(&head, &chunk_bytes) {
                Some(buf) => buf,
                None => return wado_err_response(ctx, WadoError::RangeNotSatisfiable),
            }
        }
        _ => {
            // A multi-GB instance with no usable chunk must never reach
            // `read_to_end`; a legacy/single-frame blob is small and fine.
            if large && chunk.is_none() {
                return wado_err_response(ctx, WadoError::RangeNotSatisfiable);
            }
            match read_cas_bytes(ctx.state, &oref.sha256, oref.storage_tier) {
                Ok(b) => b,
                Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(oref.sha256)),
            }
        }
    };
    let resp = match crate::handlers::wado_pure::render_frame_thumbnail_any(
        &bytes,
        ctx.uid(2),
        &frame,
        target,
        &ctx.state.codec_registry,
        &ctx.state.transcode_pool,
        ctx.state.config.transcode.max_megapixels,
    ) {
        Ok(r) => r,
        Err(e) => return wado_err_response(ctx, e),
    };
    write_wado_resp(ctx, &resp)
}

/// `GET .../instances/{sop}/frames/{frame}` - a single pixel frame.
/// Decode one stored frame fragment to raw Explicit VR LE samples.
///
/// Holds one permit of the bounded transcode pool until dropped.
///
/// The permit used to be released by hand after the decode returned. Every
/// early return in between (and any panic) skipped that line, so the pool
/// leaked permits until it was permanently saturated and every frame answered
/// 503 while the server looked healthy.
#[cfg(feature = "transcode")]
struct TranscodePermit<'a>(&'a crate::state::TranscodePool);

#[cfg(feature = "transcode")]
impl<'a> TranscodePermit<'a> {
    fn new(pool: &'a crate::state::TranscodePool) -> Self {
        Self(pool)
    }
}

#[cfg(feature = "transcode")]
impl Drop for TranscodePermit<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Split out of `h_wado_frame` so the bounded head read, the pool acquisition
/// and the pure decode are in one place. Returns `Ok(None)` when the stored
/// bytes are the right answer (see [`transcode_frame_to_evrle`]), `Err` when
/// the request must fail (over-budget -> 413, pool saturated -> 503,
/// non-conformant codestream -> 406).
#[cfg(feature = "transcode")]
fn transcode_one_frame(
    ctx: &RequestContext<'_>,
    fragment: &[u8],
    sha256: &str,
    tier: StorageTier,
    stored_ts: &str,
) -> Result<Option<Vec<u8>>, WadoError> {
    // The pixel module comes from a BOUNDED head, never the whole instance:
    // a multi-GB slide must not be read back just to learn Rows/Columns.
    // A head that does not carry the tags means "cannot transcode", not
    // "failure" - the stored bytes are still served.
    let head = match read_cas_head(ctx.state, sha256, tier, ctx.state.config.max_part_header) {
        Ok(h) => h,
        Err(_) => return Ok(None),
    };
    let Some(pixel_module) = crate::handlers::wado_pure::FramePixelModule::from_head(&head) else {
        return Ok(None);
    };
    // Bounded pool: no waiting, no queue. A saturated server answers 503 with
    // Retry-After rather than making every frame wait behind a decode.
    if !ctx.state.transcode_pool.try_acquire() {
        return Err(WadoError::Busy);
    }
    // Released when this scope ends, including on an early return or a panic
    // inside the decoder - a leaked permit would saturate the pool for good.
    let _permit = TranscodePermit::new(&ctx.state.transcode_pool);
    crate::handlers::wado_pure::transcode_frame_to_evrle(
        fragment,
        stored_ts,
        &pixel_module,
        &ctx.state.codec_registry,
        ctx.state.config.transcode.max_megapixels,
    )
}

pub(crate) fn h_wado_frame(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    // The router captures study/series/sop but not the frame index (it only
    // validates the frame is digits); re-parse it from the path tail.
    let frame = path_tail_segment(ctx);
    // Expected is irrelevant here (a frame extract is not a series slice).
    let (oref, _expected) = match crate::handlers::wado_pure::locate_object(
        ctx.state,
        ctx.uid(0),
        Some(ctx.uid(1)),
        ctx.uid(2),
        ctx.route.qa,
    ) {
        Ok(v) => v,
        Err(e) => return wado_err_response(ctx, e),
    };
    // Frame-chunk serve: when the manifest carries a per-frame byte range,
    // read ONLY that range. This is the ap101 fix for the tile route - a tile
    // from a multi-GB WSI costs one bounded read instead of a whole-blob Vec
    // plus an O(N) item walk. The fallback (no table, or a range that does
    // not read back exactly) keeps the pre-existing whole-blob extraction, so
    // an instance stored before the table existed still serves correctly.
    //
    // A2 (0310 F2/C3): the whole-blob fallback is itself BOUNDED. It exists
    // for legacy/single-frame instances, whose blob is small; a multi-GB blob
    // must never reach `read_to_end`. When the stored instance is larger than
    // `max_frame_bytes` and no usable chunk could be read, there is no bounded
    // way to answer - 416, never `read_to_end` of a multi-GB instance
    // (redirecting the refused `vec!` into a whole-blob read would be the same
    // DoS with a larger multiplier).
    let max_frame_bytes = ctx.state.config.max_frame_bytes;
    let too_large_to_fallback = || oref.file_size > max_frame_bytes;
    let frame_number: Option<u32> = frame.parse().ok();
    let chunk = frame_number.and_then(|n| oref.frames.iter().find(|f| f.number == n));
    let frame_bytes = match chunk {
        Some(f) => match read_cas_range(
            ctx.state,
            &oref.sha256,
            oref.storage_tier,
            f.offset,
            f.compressed_len,
        ) {
            Ok(b) => b,
            Err(_) => {
                // Blob and manifest disagree, or the chunk is over the cap:
                // fall back rather than serve a truncated tile - but only
                // while the whole blob is small enough to hold.
                if too_large_to_fallback() {
                    return wado_err_response(ctx, WadoError::RangeNotSatisfiable);
                }
                match read_cas_bytes(ctx.state, &oref.sha256, oref.storage_tier) {
                    Ok(b) => match crate::handlers::wado_pure::extract_frame(&b, &frame) {
                        Ok(r) => r.body,
                        Err(e) => return wado_err_response(ctx, e),
                    },
                    Err(_) => {
                        return wado_err_response(
                            ctx,
                            WadoError::FileNotFound(oref.sha256.clone()),
                        );
                    }
                }
            }
        },
        None => {
            if too_large_to_fallback() {
                return wado_err_response(ctx, WadoError::RangeNotSatisfiable);
            }
            let bytes = match read_cas_bytes(ctx.state, &oref.sha256, oref.storage_tier) {
                Ok(b) => b,
                Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(oref.sha256)),
            };
            match crate::handlers::wado_pure::extract_frame(&bytes, &frame) {
                Ok(r) => r.body,
                Err(e) => return wado_err_response(ctx, e),
            }
        }
    };
    // Per-tile decode (W22). A client that asks for Explicit VR LE while the
    // stored tile is compressed gets ONE decoded tile: the pixel samples only,
    // no element header, because a frame part is raw octet-stream. Compiled
    // only with the `transcode` feature: a build without the codecs answers
    // the same request through the stored-bytes path below, which is the
    // PS3.18 8.7.3 escape an origin server is allowed to take.
    #[cfg(feature = "transcode")]
    let (frame_bytes, served_ts) = {
        let mut served_ts = oref.transfer_syntax.clone();
        let mut frame_bytes = frame_bytes;
        if let Some(target_ts) =
            crate::handlers::wado_pure::resolve_target_ts(&wado_query(ctx), ctx.accept())
            && target_ts == crate::handlers::wado_pure::EVRLE_TS
            && served_ts != crate::handlers::wado_pure::EVRLE_TS
        {
            match transcode_one_frame(
                ctx,
                &frame_bytes,
                &oref.sha256,
                oref.storage_tier,
                &served_ts,
            ) {
                Ok(Some(raw)) => {
                    frame_bytes = raw;
                    served_ts = crate::handlers::wado_pure::EVRLE_TS.to_string();
                }
                // `None`: no decoder, already uncompressed, or a colour tile.
                // Serve the stored bytes under their own syntax.
                Ok(None) => {}
                Err(e) => return wado_err_response(ctx, e),
            }
        }
        (frame_bytes, served_ts)
    };
    #[cfg(not(feature = "transcode"))]
    let (frame_bytes, served_ts) = (frame_bytes, oref.transfer_syntax.clone());
    // PS3.18 10.4.4: the frames resource answers `multipart/related;
    // type="application/octet-stream"` with ONE part per requested frame.
    // OHIF's dicomweb-client decodes the frames response as multipart
    // unconditionally, so a bare octet-stream body breaks every image load
    // ("Response message has no multipart mime header").
    //
    // a frame part MUST name its real media AND
    // its stored transfer syntax. The cornerstone3D/dicomweb-client loader
    // resolves the transfer syntax ONLY from the part Content-Type
    // (getTransferSyntaxForContentType): a bare octet-stream with no
    // `transfer-syntax=` parameter collapsed every compressed frame to
    // Implicit VR LE and decoded it as uncompressed garbage. The stored
    // syntax comes from the manifest reference (`oref.transfer_syntax`) - no
    // per-frame parse - and `frame_part_media_type` maps it to the PS3.18
    // media type. The `transfer-syntax=` parameter is always appended so a
    // fallback octet-stream stays decodable for exotic-but-codec-backed
    // syntaxes.
    let stored_ts = served_ts.as_str();
    let media = frame_part_media_type(stored_ts);
    let boundary = single_part_boundary();
    let mut body = Vec::with_capacity(frame_bytes.len() + 256);
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Type: {media}; transfer-syntax={stored_ts}\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(&frame_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let ct =
        format!("multipart/related; type=\"application/octet-stream\"; boundary=\"{boundary}\"");
    let ka = ctx.keep_alive == KeepAlive::On;
    clarus_core::http::response::build_head(
        &mut ctx.conn.header,
        200,
        reason_phrase(200),
        Some(ct.as_bytes()),
        Some(body.len() as u64),
        ka,
        &[],
    );
    ctx.conn.write_head_configured()?;
    ctx.conn.write_all_configured(&body)
}

/// PS3.18 frame media type for a stored transfer syntax (the table that
/// dicomweb-client 0.10.4 `api.js` `supportedMediaTypes` mirrors). Returned
/// by [`h_wado_frame`] to label a pixel part; the caller ALWAYS appends
/// `; transfer-syntax=<uid>` regardless of the returned media.
fn frame_part_media_type(ts: &str) -> &'static str {
    match ts {
        // JPEG Baseline / Extended / Lossless / JPEG-LS (lossless) -> image/jpeg.
        "1.2.840.10008.1.2.4.50"
        | "1.2.840.10008.1.2.4.51"
        | "1.2.840.10008.1.2.4.57"
        | "1.2.840.10008.1.2.4.70" => "image/jpeg",
        // RLE Lossless.
        "1.2.840.10008.1.2.5" => "image/x-dicom-rle",
        // JPEG-LS (near-lossless / lossless).
        "1.2.840.10008.1.2.4.80" | "1.2.840.10008.1.2.4.81" => "image/x-jls",
        // JPEG 2000 (lossless / lossy).
        "1.2.840.10008.1.2.4.90" | "1.2.840.10008.1.2.4.91" => "image/jp2",
        // HTJ2K / HTJ2K Lossless / HTJ2K Multicomponent (+ JPEG XL HTJ2K).
        "1.2.840.10008.1.2.4.201"
        | "1.2.840.10008.1.2.4.202"
        | "1.2.840.10008.1.2.4.203"
        | "3.2.840.10008.1.2.4.96" => "image/jhc",
        // Uncompressed (Implicit/Explicit LE, Explicit BE, Deflated) and
        // unrecognized syntaxes -> generic. The transfer-syntax= parameter is
        // always present, so the loader can still pick a codec.
        _ => "application/octet-stream",
    }
}

/// Return the last path segment (`.../frames/{n}` -> `{n}`; `bulkdata/{tag}` -> tag).
fn path_tail_segment(ctx: &RequestContext<'_>) -> String {
    std::str::from_utf8(ctx.head.path)
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Parse an 8-char `GGGGEEEE` DICOM tag hex into `(group, element)`.
///
/// Mirror of the (module-private) `wado::validation::parse_tag_hex` so the sync
/// transport can resolve bulkdata tags without reaching into a private module.
fn parse_tag_hex(hex: &str) -> Result<(u16, u16), WadoError> {
    if hex.len() != 8 {
        return Err(WadoError::InvalidUid(format!(
            "Invalid tag format: {hex} - expected 8 hex chars (GGGGEEEE)"
        )));
    }
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(WadoError::InvalidUid(format!(
            "Invalid tag format: {hex} - non-hex characters"
        )));
    }
    let group =
        u16::from_str_radix(&hex[0..4], 16).map_err(|_| WadoError::InvalidUid(hex.to_string()))?;
    let element =
        u16::from_str_radix(&hex[4..8], 16).map_err(|_| WadoError::InvalidUid(hex.to_string()))?;
    Ok((group, element))
}

/// `GET .../instances/{sop}/bulkdata/{tag}` - a single DICOM tag value.
pub(crate) fn h_wado_bulkdata(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    // `bulkdata/{tag}` - 8 hex chars `ggggeeee` little-endian group/element.
    let tag = path_tail_segment(ctx);
    let (group, element) = match parse_tag_hex(&tag) {
        Ok(t) => t,
        Err(err) => return wado_err_response(ctx, err),
    };
    // Expected is irrelevant here (a bulkdata read is not a series slice).
    let (oref, _expected) = match crate::handlers::wado_pure::locate_object(
        ctx.state,
        ctx.uid(0),
        Some(ctx.uid(1)),
        ctx.uid(2),
        ctx.route.qa,
    ) {
        Ok(v) => v,
        Err(e) => return wado_err_response(ctx, e),
    };
    // 0310 F3: bounded header read. A bulk-data tag is normally a metadata
    // tag inside the head; read only `max_part_header` bytes and try to serve
    // it from there. If the tag is NOT in the head, do NOT read the whole
    // instance: seek directly to the SAME tag in the file and read ONLY its
    // value, capped at `max_frame_bytes` (the existing "max single served
    // element" knob - no new config key). A palette LUT / overlay larger than
    // that cap is refused (empty body), which is acceptable: such a value is
    // served via the metadata BulkDataURI path, not the bulkdata route.
    let parser = clarus_adapters::dicom_rs::DicomRsParser;
    let head = match read_cas_head(
        ctx.state,
        &oref.sha256,
        oref.storage_tier,
        ctx.state.config.max_part_header,
    ) {
        Ok(b) => b,
        Err(_) => return wado_err_response(ctx, WadoError::FileNotFound(oref.sha256)),
    };
    let in_head = parser.extract_tag_value(&head, (group, element));
    let body = match in_head {
        Ok(Some(v)) => v,
        Ok(None) if (head.len() as u64) < oref.file_size => {
            // Not in the head: seek-read ONLY this tag's value (bounded).
            // Not found OR over cap -> empty body (today's not-found
            // behaviour), never a whole-blob read.
            let cap = ctx.state.config.max_frame_bytes as usize;
            match resolve_path(ctx.state, &oref.sha256, oref.storage_tier) {
                Ok(path) => clarus_adapters::dicom_rs::frame::find_tag_value_in_file(
                    &path,
                    (group, element),
                    cap,
                )
                .unwrap_or_default(),
                Err(_) => Vec::new(),
            }
        }
        Ok(None) => Vec::new(),
        Err(_) => {
            return wado_err_response(
                ctx,
                WadoError::Internal("DICOM tag parse error".to_string()),
            );
        }
    };
    write_resp(
        ctx.conn,
        200,
        b"application/octet-stream",
        &body,
        ctx.keep_alive == KeepAlive::On,
    )
}

// -------------------------------------------------------------------------
// 4. STOW - temp-file + hash shim
// -------------------------------------------------------------------------

/// `POST /dicomweb/studies` - root STOW (any study UIDs).
pub(crate) fn h_stow_studies(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    stow_core(ctx, None)
}

/// `POST /dicomweb/studies/{study}` - study-scoped STOW (all parts must carry
/// the same Study Instance UID).
///
/// The scope is the router-captured path segment. `uid(0)` returns a `&'a str`
/// tied to the request path rather than to `&self`, so the borrow splits
/// cleanly from the `&mut ctx` the core needs.
pub(crate) fn h_stow_study(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let scope = ctx.uid(0);
    stow_core(ctx, Some(scope))
}

/// `POST /dicomweb/studies` (root) and `/dicomweb/studies/{study}` (scoped) -
/// store one or more DICOM instances.
///
/// Multer-style multipart lives only here; the pure core never sees a part
/// iterator. Each `application/dicom` part is streamed to a temp file;
/// the pure core then ingests it via `InstanceSource::TempFile{path, size}`.
/// Non-DICOM parts (e.g. the optional JSON metadata part) are drained and
/// ignored. Oversized parts become Failed-202 entries in the STOW response,
/// exactly as frozen by stage 3.
///
/// `scope` is `Some(study_uid)` for the study-scoped target: every instance's
/// Study Instance UID must equal it, and a mismatch is a whole-request 409
/// (PS3.18 10.5.3-1). `None` is the root target, which accepts any study.
fn stow_core(ctx: &mut RequestContext<'_>, scope: Option<&str>) -> io::Result<()> {
    use crate::handlers::stow_pure::preflight_disk;
    use crate::handlers::stow_pure::{InstanceSource, ingest_instance};
    use crate::handlers::stow_response::{StowAccum, build_failed_sop_entry};
    use clarus_ports::stow_response::InstanceOutcome;

    // The response content type is DICOM JSON, regardless of request framing.
    let ka = ctx.keep_alive == KeepAlive::On;
    // 2808 U-event: capture the identity slices before the body reader takes
    // the mutable `ctx.conn` borrow for the whole (multipart) loop.
    let ev_user = ctx.user;
    let ev_ip = ctx.client_ip;
    let ev_via = ctx.via;

    // request-level disk-space preflight -> 507 + Retry-After.
    // Runs before any body is drained so a disk-shortage STOW is rejected
    // up-front (documented 507 contract), and every request gets a response.
    if let Err(stow_err) = preflight_disk(ctx.state) {
        crate::clog_warn!("STOW 507: {}", String::from_utf8_lossy(&stow_err.body));
        let mut extras: Vec<(&[u8], &[u8])> = Vec::new();
        let retry_buf: Vec<u8>;
        if let Some(secs) = stow_err.retry_after {
            retry_buf = secs.to_string().into_bytes();
            extras.push((b"Retry-After", &retry_buf));
        }
        clarus_core::http::response::build_head(
            &mut ctx.conn.header,
            stow_err.status,
            reason_phrase(stow_err.status),
            Some(JSON_CT),
            Some(stow_err.body.len() as u64),
            ka,
            &extras,
        );
        let _ = ctx.conn.write_head_configured();
        let _ = ctx.conn.write_all_configured(&stow_err.body);
        return Ok(());
    }

    // G-06: single instance application/dicom (PS3.18 S10.5.1.1). A raw DICOM
    // body is stored as one instance and answered with 201, byte-for-byte the
    // same STOW JSON as the multipart path.
    let content_type = ctx.head.content_type.unwrap_or(b"");
    let ct_trimmed = std::str::from_utf8(content_type).unwrap_or("").trim();
    // The base media type, not the header verbatim: a conformant single-part
    // STOW-RS body may name its transfer syntax as a Content-Type parameter
    // (PS3.18 8.7.3.1), and matching the whole header refused such a body with
    // "Expected multipart/related Content-Type" on the branch below.
    if content_type_is(ct_trimmed, "application/dicom") {
        use std::io::Read;
        let max_bytes: u64 = ctx.state.config.max_instance_size_mb * 1024 * 1024;
        // Stream the raw body to a temp file (O(READ_BUF) memory), exactly like
        // the multipart part writer: a 12.5 GB single-part WSI base level must
        // not be buffered in RAM. Backpressure is the synchronous read->write:
        // the socket is read only as fast as the previous chunk is written.
        let temp_dir = ctx.state.cas.temp_dir();
        let temp_name = format!("clarus_stow_{}.dcm", clarus_adapters::unique_id());
        let temp_path = temp_dir.join(&temp_name);
        let file = match std::fs::File::create(&temp_path) {
            Ok(f) => f,
            Err(e) => {
                crate::clog_warn!(error = %e, "STOW 500: temp file create");
                let body = format!("{{\"error\":\"temp file: {e}\",\"status\":500}}").into_bytes();
                return write_resp(ctx.conn, 500, JSON_CT, &body, ka);
            }
        };
        // Fold the content hash over the bytes AS WRITTEN (one read, not two).
        let mut cas_write = clarus_core::hash::CasWrite::new(file);
        let mut body = BodyReader::new(ctx.conn, ctx.head);
        let mut oversized = false;
        let mut total: u64 = 0;
        let mut buf = [0u8; clarus_core::http::conn::READ_BUF];
        loop {
            let n = match body.read(&mut buf) {
                Ok(n) => n,
                Err(e) => {
                    // Client abort mid-stream: remove the partial temp file.
                    let _ = std::fs::remove_file(&temp_path);
                    return Err(e);
                }
            };
            if n == 0 {
                break;
            }
            total = total.saturating_add(n as u64);
            if oversized {
                continue;
            }
            if total > max_bytes {
                oversized = true;
                continue;
            }
            if let Err(e) = cas_write.write_all(&buf[..n]) {
                // ENOSPC mid-upload -> honest 507 + Retry-After; any other
                // write error stays 500.
                let _ = std::fs::remove_file(&temp_path);
                return write_stow_write_error(ctx, ka, e);
            }
        }
        // The serve loop compares the drained count against the announced
        // Content-Length before reusing the connection (PS3.18 8.1).
        let read_total = total;
        drop(body);
        ctx.conn.add_body_drained(read_total);
        if oversized {
            let _ = std::fs::remove_file(&temp_path);
            crate::clog_warn!(
                max_bytes = %max_bytes,
                "STOW 413: instance exceeds size limit"
            );
            let body = format!(
                "{{\"error\":\"Payload Too Large\",\"message\":\"Instance exceeds {} bytes limit\",\"status\":413}}",
                max_bytes
            )
            .into_bytes();
            // Single-instance 413 mirrors the axum `to_bytes` over-limit path:
            // a bare `application/json` (no charset), exactly as frozen.
            clarus_core::http::response::build_head(
                &mut ctx.conn.header,
                413,
                reason_phrase(413),
                Some(b"application/json"),
                Some(body.len() as u64),
                ka,
                &[],
            );
            let _ = ctx.conn.write_head_configured();
            let _ = ctx.conn.write_all_configured(&body);
            return Ok(());
        }
        // No flush here: std File writes are unbuffered into the page cache.
        // Durability is established LATER, by the CAS publish step - the
        // manifest P2 fsync for the manifest, and (0710) a blob `sync_all`
        // barrier inside `rename_temp_to_cas` before the CAS rename.
        let (folded_blake3, file) = cas_write.finish();
        drop(file);

        let mut batch = crate::handlers::stow_pure::ManifestBatch::default();
        // 2808: the (0400,0566) ModifyingOperator is the EFFECTIVE identity
        // (the same label the access journal uses), not a literal "STOW-RS".
        let modifier = std::str::from_utf8(ctx.user).unwrap_or("STOW-RS");
        let outcome = ingest_instance(
            ctx.state,
            InstanceSource::TempFile {
                path: &temp_path,
                size: total,
                blake3: folded_blake3,
            },
            modifier,
            &mut batch,
            scope,
        );
        let _ = std::fs::remove_file(&temp_path);
        let mut accum = StowAccum::default();
        match outcome {
            Ok(ingest) => {
                let (ref_sop, fail_sop) = build_json_entries(&ingest.outcome, ingest.coerced);
                accum.record(&ingest.outcome, ref_sop, fail_sop);
                if ingest.coerced {
                    accum.warned = accum.warned.saturating_add(1);
                    // The XML serializer reads the structured mirror, so the
                    // Warn Reason must be set there too or the two formats
                    // would describe different answers.
                    accum.mark_last_referenced_coerced();
                }
                emit_update_event(ev_user, ev_ip, ev_via, &ingest);
            }
            Err(stow_err) => {
                // resurrection-guard: a tombstoned study refuses the
                // WHOLE request (409), not one SOP. No other STOW path produces
                // 409, so this is unambiguous.
                if stow_err.status == 409 {
                    crate::clog_warn!("STOW 409: {}", String::from_utf8_lossy(&stow_err.body));
                    let body = &stow_err.body;
                    clarus_core::http::response::build_head(
                        &mut ctx.conn.header,
                        409,
                        reason_phrase(409),
                        Some(JSON_CT),
                        Some(body.len() as u64),
                        ka,
                        &[],
                    );
                    let _ = ctx.conn.write_head_configured();
                    let _ = ctx.conn.write_all_configured(body);
                    return Ok(());
                }
                let reason = String::from_utf8_lossy(&stow_err.body).into_owned();
                let failed = InstanceOutcome::Failed {
                    sop_class_uid: None,
                    sop_instance_uid: None,
                    reason: reason.clone(),
                };
                accum.record(
                    &failed,
                    None,
                    Some(build_failed_sop_entry(None, None, &reason)),
                );
            }
        }
        // P1 group commit: one manifest write per study for this request. An
        // instance whose study manifest could not be written is not
        // retrievable, so it is reported as failed (reason 272) instead of
        // counted as stored.
        let mut manifest_failed = crate::handlers::stow_pure::commit_manifests(ctx.state, &batch);
        manifest_failed.extend(batch.take_manifest_failed());
        for sop_instance_uid in &manifest_failed {
            let reason = "Failed to write the study manifest for this instance".to_string();
            let failed = InstanceOutcome::Failed {
                sop_class_uid: None,
                sop_instance_uid: Some(sop_instance_uid.clone()),
                reason: reason.clone(),
            };
            accum.record(
                &failed,
                None,
                Some(build_failed_sop_entry(
                    None,
                    Some(sop_instance_uid.as_str()),
                    &reason,
                )),
            );
        }
        // P2 durability: sync the written manifests before the 200/202 leaves.
        crate::handlers::stow_pure::sync_batch_manifests(ctx.state, &batch);
        // IOCM rate denominator: instances this request actually added. Exact
        // duplicates are excluded - they do not grow the stored population, and
        // counting them would let a client depress the measured rejection rate
        // by re-sending the same instances.
        crate::metrics::record_instances_stored(
            accum.accepted.saturating_sub(accum.idempotent) as u64
        );
        // IOCM: a rejection note in this batch sequesters what it names. Runs
        // after the commit so the referenced instances exist, and is a no-op
        // for a batch without a recognised note. The effective user goes with
        // it: a hide is a clinical sequester, so the trail must name who
        // triggered it.
        crate::handlers::stow_pure::apply_rejections(
            ctx.state,
            &batch,
            std::str::from_utf8(ctx.user).unwrap_or("anonymous"),
        );
        // F1: the studies are durable at this point, so the CAD workitems may
        // be created. Fire-and-forget - a failure is counted, never echoed.
        crate::handlers::cad_workitem::after_durable_stow(ctx.state, &batch);
        let wants_xml =
            accept_media_type_present_ci(ctx.accept().unwrap_or(""), "application/dicom+xml");
        let (status, ct, resp_body, warning) =
            crate::handlers::stow_response::build_stow_response(&accum, wants_xml);
        let user = std::str::from_utf8(ctx.user).ok();
        // Access journal: the 110104 line carries the accepted count.
        ctx.stow_accepted = accum.accepted as u64;
        // STOW log aggregation: one started line per new session and
        // one completed/partial line with totals, replacing the per-slice
        // code-10 token. `stored = accepted - idempotent` mirrors the old one.
        // bytes/us have no StowAccum source here -> None (fields omitted).
        if let Some(study) = accum.study_uid.as_deref() {
            let stored = accum.accepted.saturating_sub(accum.idempotent);
            let lines = crate::stow_agg::record(
                study.as_bytes(),
                user.unwrap_or("anonymous").as_bytes(),
                ctx.client_ip,
                stored as u64,
                accum.idempotent as u64,
                accum.failed as u64,
                accum.conflicts as u64,
                None,
                None,
            );
            for line in lines {
                crate::clog_event!(crate::clog::Level::Info, "STOW_STUDY", "", "{line}");
            }
        }
        // STOW ingest observability: a request that failed or conflicted any
        // SOP must not vanish silently. One WARN per request (no rate limit).
        if accum.failed > 0 || accum.conflicts > 0 {
            crate::clog_warn!(
                stored = accum.accepted.saturating_sub(accum.idempotent),
                idempotent = accum.idempotent,
                failed = accum.failed,
                conflicts = accum.conflicts,
                user = %std::str::from_utf8(ctx.user).unwrap_or("<bad-utf8>"),
                ip = %std::str::from_utf8(ctx.client_ip).unwrap_or("<bad-utf8>"),
                "STOW request rejected"
            );
        }
        // A fully rejected request also records a journal token (code 2).
        if accum.accepted == 0 && (accum.failed > 0 || accum.conflicts > 0) {
            ctx.state.shadow.record_stow_rejected(
                accum.failed,
                accum.conflicts,
                user,
                ctx.client_ip,
            );
        }
        return write_stow_response(
            ctx.conn,
            status,
            ct,
            &resp_body,
            warning,
            ctx.keep_alive == KeepAlive::On,
        );
    }

    // 1. Validate the request is multipart/related and extract the boundary.
    // A body whose Content-Type is neither `application/dicom` (handled above)
    // nor `multipart/related` is an unsupported media type, not a malformed
    // request: PS3.18 10.5.3-1 lists 415 for exactly this case.
    let ct_str = String::from_utf8_lossy(content_type);
    if !ct_str.to_ascii_lowercase().starts_with("multipart/related") {
        let body = b"{\"error\":\"Unsupported Media Type: expected multipart/related or application/dicom\",\"status\":415}".to_vec();
        return write_resp(ctx.conn, 415, JSON_CT, &body, ka);
    }
    let boundary = match extract_boundary(&ct_str) {
        Some(b) => b,
        None => {
            let body = b"{\"error\":\"Missing multipart boundary\",\"status\":400}".to_vec();
            return write_resp(ctx.conn, 400, JSON_CT, &body, ka);
        }
    };

    // 2a. A zero-byte body is a client-side no-op, not a parser failure:
    // answer honestly instead of the misleading "EOF before first boundary".
    if matches!(
        framing(ctx.head.content_length, ctx.head.transfer_encoding),
        Framing::Length(0)
    ) {
        crate::clog_warn!("STOW 400: empty body (zero parts), cl=0 te=-");
        let body = b"{\"error\":\"empty STOW body (zero parts)\",\"status\":400}".to_vec();
        return write_resp(ctx.conn, 400, JSON_CT, &body, ka);
    }

    // 2. Wrap the request body (Content-Length or chunked) in a bounded reader.
    let mut body = BodyReader::new(ctx.conn, ctx.head);

    // 3. Stream parts.
    let mut parser = match MultipartParser::new(&mut body, boundary.as_bytes()) {
        Ok(p) => p,
        Err(e) => {
            log_stow_parse_fail(ctx.head, &e);
            let body = format!("{{\"error\":\"{e}\",\"status\":400}}").into_bytes();
            return write_resp(ctx.conn, 400, JSON_CT, &body, ka);
        }
    };

    let mut accum = StowAccum::default();
    // P1 group commit: request-scoped accumulation of accepted instances,
    // flushed once per study after the loop.
    let mut batch = crate::handlers::stow_pure::ManifestBatch::default();
    let max_bytes = ctx.state.config.max_instance_size_mb * 1024 * 1024;
    let mut has_dicom = false;
    // Loop-local counters for the part-stream WARN: how many DICOM parts were
    // already accepted before the broken body, and who the operator is.
    let mut dicom_parts: u64 = 0;
    let agg_user = std::str::from_utf8(ctx.user).unwrap_or("anonymous");

    // `pending` holds the header of the next part when it was already consumed
    // by the inner data-drain loop (the pull parser yields it as the event
    // right after the previous part's body). Without this the next part's
    // header would be discarded and only the first part stored.
    let mut pending: Option<clarus_core::http::multipart::PartHeader> = None;

    loop {
        // Acquire the current part header: buffered, or the next event.
        let this_part = match pending.take() {
            Some(ph) => ph,
            None => match parser.next_event() {
                Ok(MultipartEvent::Part(ph)) => ph,
                Ok(MultipartEvent::Data(_)) => continue,
                Ok(MultipartEvent::End) => break,
                Err(e) => {
                    log_stow_parse_fail(ctx.head, &e);
                    let body = format!("{{\"error\":\"multipart parse: {e}\",\"status\":400}}")
                        .into_bytes();
                    return write_resp(ctx.conn, 400, JSON_CT, &body, ka);
                }
            },
        };

        let is_dicom = this_part
            .content_type()
            .eq_ignore_ascii_case(b"application/dicom");
        if !is_dicom {
            // Drain non-DICOM (metadata) part bytes without storing; keep any
            // following part header so it is not lost.
            loop {
                match parser.next_event() {
                    Ok(MultipartEvent::Part(next)) => {
                        pending = Some(next);
                        break;
                    }
                    Ok(MultipartEvent::Data(_)) => {}
                    Ok(MultipartEvent::End) => break,
                    Err(e) => {
                        log_stow_parse_fail(ctx.head, &e);
                        let body = format!("{{\"error\":\"multipart parse: {e}\",\"status\":400}}")
                            .into_bytes();
                        return write_resp(ctx.conn, 400, JSON_CT, &body, ka);
                    }
                }
            }
            continue;
        }

        has_dicom = true;
        let temp_dir = ctx.state.cas.temp_dir();
        let temp_name = format!("clarus_stow_{}.dcm", clarus_adapters::unique_id());
        let temp_path = temp_dir.join(&temp_name);
        let file = match std::fs::File::create(&temp_path) {
            Ok(f) => f,
            Err(e) => {
                crate::clog_warn!(error = %e, "STOW 500: temp file create");
                let body = format!("{{\"error\":\"temp file: {e}\",\"status\":500}}").into_bytes();
                return write_resp(ctx.conn, 500, JSON_CT, &body, ka);
            }
        };
        let mut total: u64 = 0;
        let mut oversized = false;

        // Fold the content hash over the bytes AS WRITTEN: `CasWrite` tees
        // every accepted chunk into the running digest, so ingest needs no
        // second read. Oversized chunks are drained WITHOUT hashing (they are
        // never written and the oversized part never uses the digest).
        let mut cas_write = clarus_core::hash::CasWrite::new(file);

        // Drain the DICOM part body: size cap on the fly. (the
        // legacy SHA-256 stream hash was removed; ingest folds BLAKE3 now.)
        // If the parser yields the NEXT part header, buffer it for the outer loop.
        let mut write_err: Option<io::Error> = None;
        let mut finished = false;
        while !finished {
            match parser.next_event() {
                Ok(MultipartEvent::Data(chunk)) => {
                    total = total.saturating_add(chunk.len() as u64);
                    if oversized {
                        continue; // already past the cap; keep draining
                    }
                    if total > max_bytes {
                        oversized = true;
                        continue;
                    }
                    if let Err(e) = cas_write.write_all(chunk) {
                        write_err = Some(e);
                    }
                }
                Ok(MultipartEvent::Part(next)) => {
                    pending = Some(next);
                    finished = true;
                }
                Ok(MultipartEvent::End) => {
                    finished = true;
                }
                Err(e) => {
                    let study = accum.study_uid.as_deref().unwrap_or("<none>");
                    crate::clog_warn!(
                        "STOW 400: part stream ({e}) got={} study={} user={} ip={}",
                        dicom_parts,
                        study,
                        agg_user,
                        std::str::from_utf8(ctx.client_ip).unwrap_or("")
                    );
                    // A mid-part abort leaves a partial temp file on disk; for a
                    // gigabyte WSI part that is a large leak until the next
                    // restart. Best-effort removal (same as the write-error
                    // branch), then the 400.
                    let _ = std::fs::remove_file(&temp_path);
                    let body =
                        format!("{{\"error\":\"part stream: {e}\",\"status\":400}}").into_bytes();
                    // RFC 9112 s9.6: after a broken body the request boundary
                    // is lost - keep-alive is unsafe, answer with close.
                    return write_resp(ctx.conn, 400, JSON_CT, &body, false);
                }
            }
        }
        if let Some(e) = write_err {
            let _ = std::fs::remove_file(&temp_path);
            return write_stow_write_error(ctx, ka, e);
        }
        // D6: no flush here - std File writes are unbuffered, so `flush()`
        // would be a no-op suggesting durability that is not there (the
        // durability comes later, from the manifest P2 fsync).
        let (folded_blake3, file) = cas_write.finish();
        drop(file);

        if oversized {
            // Frozen stage-3 behaviour: oversize -> Failed-202.
            accum.record(
                &InstanceOutcome::Failed {
                    sop_class_uid: None,
                    sop_instance_uid: None,
                    reason: "Instance exceeds size limit".to_string(),
                },
                None,
                Some(build_failed_sop_entry(
                    None,
                    None,
                    "Instance exceeds size limit",
                )),
            );
            let _ = std::fs::remove_file(&temp_path);
            dicom_parts += 1;
            continue;
        }

        // Multi-part event; store via temp file + size. The content hash was
        // folded over the written bytes by `CasWrite` (no second read).
        let modifier = std::str::from_utf8(ctx.user).unwrap_or("STOW-RS");
        let outcome = ingest_instance(
            ctx.state,
            InstanceSource::TempFile {
                path: &temp_path,
                size: total,
                blake3: folded_blake3,
            },
            modifier,
            &mut batch,
            scope,
        );
        let _ = std::fs::remove_file(&temp_path);
        match outcome {
            Ok(ingest) => {
                let (ref_sop, fail_sop) = build_json_entries(&ingest.outcome, ingest.coerced);
                accum.record(&ingest.outcome, ref_sop, fail_sop);
                // PS3.18 10.5.2 "stored with warnings" -> 202. Coercion is
                // per instance and is already published as Warn Reason B000 in
                // that instance's entry, but the status code is the only signal
                // a client that does not parse the body ever sees.
                if ingest.coerced {
                    accum.warned = accum.warned.saturating_add(1);
                    // The XML serializer reads the structured mirror, so the
                    // Warn Reason must be set there too or the two formats
                    // would describe different answers.
                    accum.mark_last_referenced_coerced();
                }
                emit_update_event(ev_user, ev_ip, ev_via, &ingest);
                dicom_parts += 1;
                // Per-part delta into the aggregator: this is the live
                // progress inside one multipart. Conflict/Failed carry no
                // study UID, so they are skipped here - dicom_parts already
                // counts the part, and totals still surface via TTL.
                if let InstanceOutcome::Stored {
                    study_instance_uid, ..
                }
                | InstanceOutcome::Updated {
                    study_instance_uid, ..
                }
                | InstanceOutcome::Idempotent {
                    study_instance_uid, ..
                } = &ingest.outcome
                {
                    let (stored, idem) = match &ingest.outcome {
                        InstanceOutcome::Idempotent { .. } => (0u64, 1u64),
                        _ => (1u64, 0u64),
                    };
                    let lines = crate::stow_agg::record(
                        study_instance_uid.as_bytes(),
                        agg_user.as_bytes(),
                        ctx.client_ip,
                        stored,
                        idem,
                        0,
                        0,
                        None,
                        None,
                    );
                    for line in lines {
                        crate::clog_event!(crate::clog::Level::Info, "STOW_STUDY", "", "{line}");
                    }
                }
            }
            Err(stow_err) => {
                // resurrection-guard: a tombstoned study refuses the
                // whole request (409). Short-circuit - remaining parts are
                // futile because the entire study is tombstoned.
                if stow_err.status == 409 {
                    crate::clog_warn!("STOW 409: {}", String::from_utf8_lossy(&stow_err.body));
                    let body = &stow_err.body;
                    clarus_core::http::response::build_head(
                        &mut ctx.conn.header,
                        409,
                        reason_phrase(409),
                        Some(JSON_CT),
                        Some(body.len() as u64),
                        ka,
                        &[],
                    );
                    let _ = ctx.conn.write_head_configured();
                    let _ = ctx.conn.write_all_configured(body);
                    return Ok(());
                }
                let reason = String::from_utf8_lossy(&stow_err.body).into_owned();
                let failed = InstanceOutcome::Failed {
                    sop_class_uid: None,
                    sop_instance_uid: None,
                    reason: reason.clone(),
                };
                accum.record(
                    &failed,
                    None,
                    Some(build_failed_sop_entry(None, None, &reason)),
                );
                dicom_parts += 1;
            }
        }
    }

    if !has_dicom {
        crate::clog_warn!("STOW 400: no application/dicom part");
        let body = b"{\"error\":\"No application/dicom part found in multipart/related request\",\"status\":400}".to_vec();
        return write_resp(ctx.conn, 400, JSON_CT, &body, ka);
    }

    // P1 group commit: one manifest write per study for this request (the
    // accepted instances accumulated across all parts). An instance whose
    // study manifest could not be written is not retrievable, so it moves from
    // the accepted count into FailedSOPSequence (reason 272).
    let manifest_failed = {
        let mut all = crate::handlers::stow_pure::commit_manifests(ctx.state, &batch);
        all.extend(batch.take_manifest_failed());
        all
    };
    for sop_instance_uid in &manifest_failed {
        let reason = "Failed to write the study manifest for this instance".to_string();
        let failed = InstanceOutcome::Failed {
            sop_class_uid: None,
            sop_instance_uid: Some(sop_instance_uid.clone()),
            reason: reason.clone(),
        };
        accum.record(
            &failed,
            None,
            Some(build_failed_sop_entry(
                None,
                Some(sop_instance_uid.as_str()),
                &reason,
            )),
        );
    }
    // P2 durability: sync the written manifests before the 200/202 leaves.
    crate::handlers::stow_pure::sync_batch_manifests(ctx.state, &batch);
    // IOCM rate denominator: instances this request actually added (see the
    // multipart path above for why exact duplicates are excluded).
    crate::metrics::record_instances_stored(accum.accepted.saturating_sub(accum.idempotent) as u64);
    // IOCM: a rejection note in this batch sequesters what it names. Runs
    // after the commit so the referenced instances exist, and is a no-op for
    // a batch without a recognised note. The effective user goes with it: a
    // hide is a clinical sequester, so the trail must name who triggered it.
    crate::handlers::stow_pure::apply_rejections(
        ctx.state,
        &batch,
        std::str::from_utf8(ctx.user).unwrap_or("anonymous"),
    );
    // F1: the studies are durable at this point, so the CAD workitems may
    // be created. Fire-and-forget - a failure is counted, never echoed.
    crate::handlers::cad_workitem::after_durable_stow(ctx.state, &batch);
    let wants_xml =
        accept_media_type_present_ci(ctx.accept().unwrap_or(""), "application/dicom+xml");
    let (status, ct, resp_body, warning) =
        crate::handlers::stow_response::build_stow_response(&accum, wants_xml);
    let user = std::str::from_utf8(ctx.user).ok();
    // Access journal: the 110104 line carries the accepted count.
    ctx.stow_accepted = accum.accepted as u64;
    // STOW ingest observability: a request that failed or conflicted any SOP
    // must not vanish silently. One WARN per request (no rate limit).
    if accum.failed > 0 || accum.conflicts > 0 {
        crate::clog_warn!(
            stored = accum.accepted.saturating_sub(accum.idempotent),
            idempotent = accum.idempotent,
            failed = accum.failed,
            conflicts = accum.conflicts,
            user = %std::str::from_utf8(ctx.user).unwrap_or("<bad-utf8>"),
            ip = %std::str::from_utf8(ctx.client_ip).unwrap_or("<bad-utf8>"),
            "STOW request rejected"
        );
    }
    // A fully rejected request also records a journal token (code 2).
    if accum.accepted == 0 && (accum.failed > 0 || accum.conflicts > 0) {
        ctx.state
            .shadow
            .record_stow_rejected(accum.failed, accum.conflicts, user, ctx.client_ip);
    }
    write_stow_response(
        ctx.conn,
        status,
        ct,
        &resp_body,
        warning,
        ctx.keep_alive == KeepAlive::On,
    )
}

/// Build the `(referenced_sop, failed_sop)` pair for an ingest outcome.
///
/// Mirrors the axum `build_json_entries` (stow.rs) destructuring so the wire
/// STOW JSON is byte-for-byte identical between the two transports.
fn build_json_entries(
    outcome: &clarus_ports::stow_response::InstanceOutcome,
    coerced: bool,
) -> (Option<String>, Option<String>) {
    use crate::handlers::stow_response::{
        build_conflict_failed_sop_entry, build_failed_sop_entry, build_referenced_sop_entry,
    };
    use clarus_ports::stow_response::InstanceOutcome;
    match outcome {
        InstanceOutcome::Stored {
            sop_class_uid,
            sop_instance_uid,
            study_instance_uid,
            series_instance_uid,
            ..
        }
        | InstanceOutcome::Updated {
            sop_class_uid,
            sop_instance_uid,
            study_instance_uid,
            series_instance_uid,
            ..
        }
        | InstanceOutcome::Idempotent {
            sop_class_uid,
            sop_instance_uid,
            study_instance_uid,
            series_instance_uid,
            ..
        } => (
            Some(build_referenced_sop_entry(
                sop_class_uid,
                sop_instance_uid,
                study_instance_uid,
                series_instance_uid,
                coerced,
            )),
            None,
        ),
        InstanceOutcome::Conflict {
            sop_class_uid,
            sop_instance_uid,
            existing_patient_id,
            ..
        } => {
            // The sender must be told WHICH object conflicted, not whose it
            // was: the existing Patient ID stays in the log, where it is
            // already masked, rather than on the wire to a stranger.
            let _ = existing_patient_id;
            (
                None,
                Some(build_conflict_failed_sop_entry(
                    sop_class_uid,
                    sop_instance_uid,
                )),
            )
        }
        InstanceOutcome::Failed {
            sop_class_uid,
            sop_instance_uid,
            reason,
        } => (
            None,
            Some(build_failed_sop_entry(
                sop_class_uid.as_deref(),
                sop_instance_uid.as_deref(),
                reason,
            )),
        ),
    }
}

/// 2808 + 3008: emit the access-journal U line (event="attribute_update",
/// action="U", ATNA 110103). Updated outcomes carry the CORRECT/COERCE
/// counters; a coerced INSERT is journaled with reason "coerce_insert"
/// (old="" -> generated AUTOPAT_/Anonymous^ values, 3008: the alibi blob is
/// GONE, the journal is the record); a hidden-PHI sanitization with reason
/// "hidden_phi_excised" (distinct from CORRECT/COERCE). Identifiers only -
/// old values live exclusively inside the stored file's (0400,0561).
fn emit_update_event(user: &[u8], ip: &[u8], via: &'static str, ingest: &IngestOutcome) {
    use clarus_ports::stow_response::InstanceOutcome;
    match &ingest.outcome {
        InstanceOutcome::Updated {
            sop_instance_uid,
            study_instance_uid,
            series_instance_uid,
            patient_id,
            changed_attrs,
            bytes_replaced,
            ..
        } => {
            crate::access_log::emit_update(
                user,
                ip,
                via,
                study_instance_uid,
                series_instance_uid,
                sop_instance_uid,
                patient_id,
                *changed_attrs,
                *bytes_replaced,
                if ingest.hidden_phi_excised {
                    "hidden_phi_excised"
                } else {
                    ""
                },
            );
        }
        InstanceOutcome::Stored {
            sop_class_uid: _,
            sop_instance_uid,
            study_instance_uid,
            series_instance_uid,
            patient_id,
        } if ingest.hidden_phi_excised => {
            crate::access_log::emit_update(
                user,
                ip,
                via,
                study_instance_uid,
                series_instance_uid,
                sop_instance_uid,
                patient_id,
                0,
                false,
                "hidden_phi_excised",
            );
        }
        InstanceOutcome::Stored {
            sop_class_uid: _,
            sop_instance_uid,
            study_instance_uid,
            series_instance_uid,
            patient_id,
        } if ingest.coerced => {
            // 3008: placeholder coercion is journaled ONLY (old="" ->
            // generated value; the U line carries the generated-field count,
            // never the values).
            crate::access_log::emit_update(
                user,
                ip,
                via,
                study_instance_uid,
                series_instance_uid,
                sop_instance_uid,
                patient_id,
                ingest.generated,
                false,
                "coerce_insert",
            );
        }
        _ => {}
    }
}

/// Extract the `boundary="..."` value from a multipart Content-Type header.
fn extract_boundary(ct: &str) -> Option<String> {
    let lower = ct.to_ascii_lowercase();
    let idx = lower.find("boundary=")?;
    let rest = &ct[idx + "boundary=".len()..];
    let rest = rest.trim_start();
    // Weasis (Java HttpClient) sends compact unquoted params with no space
    // after `;` (`boundary=weasisDicomBoundary;`): an unquoted value runs to
    // the next `;` or the end of the header. A quoted value is delimited by
    // its closing quote, as before.
    let b = if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"').unwrap_or(stripped.len());
        &stripped[..end]
    } else {
        let end = rest.find(';').unwrap_or(rest.len());
        rest[..end].trim_end()
    };
    if b.is_empty() {
        None
    } else {
        Some(b.to_string())
    }
}

// -------------------------------------------------------------------------
// STOW request-body reader (Content-Length or chunked over the socket)
// -------------------------------------------------------------------------

/// A `Read` adapter over the raw connection body for `MultipartParser`.
///
/// Selects the framing from the request head: a known `Content-Length` bounds
/// the read to exactly that many bytes; `Transfer-Encoding: chunked` is decoded
/// on the fly via `ChunkedReader`. This is the only seam that adapts the
/// byte-stream (chunked/CL) into the parser, keeping the pure core free of any
/// knowledge of the wire framing.
enum BodyReader<'a> {
    Length(LimitedLength<PrefixedReader<&'a mut std::net::TcpStream>>),
    Chunked(ChunkedReader<PrefixedReader<&'a mut std::net::TcpStream>>),
}

impl<'a> BodyReader<'a> {
    fn new(conn: &'a mut Conn, head: &RequestHead<'_>) -> Self {
        // `read_head` may have read body bytes into the head buffer in the
        // same TCP segment(s) as the request head. Those bytes must reach the
        // multipart parser first - otherwise the body stream is truncated.
        let prefix = conn.take_prefetched();
        let src = PrefixedReader::new(conn.stream_mut(), prefix);
        match framing(head.content_length, head.transfer_encoding) {
            Framing::Length(n) => BodyReader::Length(LimitedLength {
                inner: src,
                remaining: n,
            }),
            Framing::Chunked => BodyReader::Chunked(ChunkedReader::new(src)),
        }
    }
}

impl Read for BodyReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            BodyReader::Length(r) => r.read(buf),
            BodyReader::Chunked(r) => r.read(buf),
        }
    }
}

/// Read exactly up to `remaining` bytes (Content-Length body).
struct LimitedLength<R: Read> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for LimitedLength<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(self.remaining) as usize;
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "body ended before Content-Length satisfied",
            ));
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Log a STOW multipart parse failure together with the wire framing
/// (Content-Length / Transfer-Encoding) so a field report is self-contained
/// without a packet capture.
fn log_stow_parse_fail(head: &RequestHead<'_>, e: &dyn std::fmt::Display) {
    let cl = head
        .content_length
        .map_or_else(|| "-".to_string(), |n| n.to_string());
    let te = match head.transfer_encoding {
        Some(t) => String::from_utf8_lossy(t).into_owned(),
        None => "-".to_string(),
    };
    crate::clog_warn!(cl = %cl, te = %te, error = %e, "STOW 400: multipart parse");
}

// -------------------------------------------------------------------------
// 5. UPS events SSE - synchronous fan-out
// -------------------------------------------------------------------------

/// `GET /dicomweb/workitems/{wi}/events` - SSE stream of UPS state changes.
///
/// This is the stage-8b *receiver*: it subscribes to the synchronous mpsc
/// fan-out (not the axum broadcast) and pushes SSE frames to the socket. Wire
/// compatibility with the stage-6 axum endpoint is preserved frame-for-frame:
/// each event is emitted as `event: StateChange`, `id: {event_id}`,
/// `data: {json}`; a keep-alive comment is sent every 15 s when idle. The
/// receiver dies with the connection thread, so no explicit unsubscribe is
/// needed - dropping the `Receiver` lets the fan-out GC the sender.
pub(crate) fn h_ups_events(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let uid = ctx.uid(0);
    let ka = ctx.keep_alive == KeepAlive::On;

    // Validate the workitem UID and existence (like the axum endpoint).
    if uid.is_empty() || !uid.bytes().all(|b| b == b'.' || b.is_ascii_digit()) {
        let body = b"{\"error\":\"invalid workitem UID\",\"status\":400}".to_vec();
        return write_resp(ctx.conn, 400, JSON_CT, &body, ka);
    }
    // existence check against the CBOR workitem store (was
    // `state.backend.find_workitem_by_uid` - the SQLite UPS path is replaced).
    let exists = clarus_core::ups::WorkItemStore::new(ctx.state.workitems_dir.clone())
        .find_by_uid(uid)
        .ok()
        .flatten()
        .is_some();
    if !exists {
        // Freeze-parity with the axum `UpsSseError::NotFound` (cap W), which
        // the smoke test greps for in the response body.
        let body = crate::handlers::json_common::err_json(&format!("Workitem not found: {uid}"));
        return write_resp(ctx.conn, 404, JSON_CT, &body, ka);
    }

    // Subscribe FIRST, before any bytes reach the client: once the client
    // sees the initial `: keep-alive` comment, the subscription is already
    // registered, so a state-change PUT can never overtake the GET and have
    // its event dropped (12.09 CI flake in ups_sse_state_change: the PUT won
    // the race and the event was published to zero subscribers).
    //
    // The per-UID subscriber cap is enforced here. A refusal is a 503 rather
    // than a silent subscription: the client asked for a stream that the
    // server will not serve, and pretending otherwise would look like a
    // stalled workflow. `Retry-After` is deliberately absent - the condition
    // clears only when another client disconnects, which the server cannot
    // predict.
    let (rx, subscribers) = match ctx.state.ups_events.fanout_try_subscribe(uid) {
        Ok(v) => v,
        Err(_) => {
            // `rx` is not started (the client sees only the status), so the
            // refusal message is a plain JSON error body.
            let body = crate::handlers::json_common::err_json(&format!(
                "Too many event subscribers for workitem {uid}"
            ));
            return write_resp(ctx.conn, 503, JSON_CT, &body, ka);
        }
    };
    crate::clog_debug!(
        workitem_uid = %uid,
        subscribers,
        "UPS SSE subscriber added"
    );

    // Open the stream: SSE content type, then a keep-alive comment so the
    // client knows the connection is live before any event arrives.
    //
    // `X-Accel-Buffering: no` is part of the HEAD, not the body: a reverse
    // proxy in front of this server (nginx is the documented deployment)
    // buffers a proxied response by default, which would hold events back
    // until a buffer fills or the stream ends - the subscriber would see a
    // silent stream. nginx honours this header directly, so no site
    // configuration is needed.
    // Every body write goes through `chunked_data`, NOT `write_all_configured`.
    // The head declares `Transfer-Encoding: chunked`, so the body must be a
    // sequence of `{hex}\r\n{payload}\r\n` frames. A raw `write_all` puts the
    // payload on the wire with no chunk-size line, and a strict HTTP client
    // (httpx -> h11, the one the requester sidecars use) then parses the first
    // payload byte as a chunk size, fails, and waits forever - the 15 s
    // keep-alive keeps resetting its read timeout, so the hang never ends.
    // The SSE payload itself is unchanged and stays LF-terminated; only the
    // HTTP framing is added.
    ctx.conn
        .chunked_start_with(200, "OK", SSE_CT, ka, &[(b"X-Accel-Buffering", b"no")])?;
    ctx.conn.chunked_data(b": keep-alive\n\n")?;

    loop {
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(event) => {
                let json = ups_event_json(&event);
                let frame = ups_sse_frame(event.event_id, &json);
                ctx.conn.chunked_data(&frame)?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                ctx.conn.chunked_data(b": keep-alive\n\n")?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Fan-out GC'd our sender (receiver dropped by writer only here);
                // end the stream cleanly.
                break;
            }
        }
    }
    ctx.conn.chunked_end()
}

/// Build one UPS SSE frame: `event:` + `id:` + `data:` (stage-6 wire order).
///
/// Public only so the ap101 allocation gate can call it from its isolated
/// test binary (`tests/ap101_sse.rs`). `chunked_data` now carries this frame on
/// the wire, which made the SSE path its first real consumer - the ap101 gate
/// pins the framing cost so a future edit cannot start allocating per event.
///
/// It returns a `Vec<u8>` and the body is built with `format!`, so allocations
/// are INHERENT here; the gate pins the measured total rather than demanding
/// zero, which is the difference between a budget and a wish.
pub fn ups_sse_frame(event_id: u64, json: &str) -> Vec<u8> {
    format!("event: StateChange\nid: {event_id}\ndata: {json}\n\n").into_bytes()
}

// -------------------------------------------------------------------------
// 5b. UPS WebSocket Notification Connection (PS3.18 8.10.4, RAD-109/RAD-87)
// -------------------------------------------------------------------------

/// Publish one UPS event to every open Notification Connection that should
/// receive it.
///
/// Called from `DualEventSink::publish`, i.e. from the SINGLE place where an
/// event is raised. The payload is produced by the same `ups_event_json` the
/// SSE stream uses, so the two transports carry byte-identical Event Reports.
///
/// # Which channels receive it
///
/// PS3.18 8.10.4 opens the channel; PS3.18 11.10 subscriptions decide which
/// events the requester wants. A channel is served only when its `{requester}`
/// holds a subscription that accepts this event, using the same filter the
/// POST push uses - the same function, not a copy of its rules. The event
/// carries its workitem's filter keys (Procedure Step Label and Scheduled
/// Workitem Code), so both transports answer from the same two facts. That is
/// why a requester with no subscription receives nothing even though its
/// connection is open.
///
/// # Never blocks the writer
///
/// Delivery goes through a bounded per-channel queue with `try_send`; a
/// subscriber that stopped reading loses events and is counted
/// (`WS_CHANNEL_DROPPED`), never stalls the workitem write path. The AIW-I
/// Reliability statement commits to exactly this: notifications are a latency
/// optimisation, and the durable path is UPS Search.
pub(crate) fn publish_event_to_channels(event: &crate::handlers::ups::event_bus::UpsEvent) {
    let channels = crate::ws_channels::channels();
    if channels.live_count() == 0 {
        // The common case on a server with no open connections: no
        // serialisation work at all.
        return;
    }

    // Which filter keys this event carries. They are the workitem's own
    // values, filled in by the notification that raised the event; a decoded
    // JSON body could not legitimately reach this struct.
    //
    // Reading them from the EVENT is the whole point: the label alone is not
    // the filter. A subscription narrowed by Coding Scheme Designator (the
    // channel-only subscriber shape) matched nothing while only the label was
    // passed, because the code-less question is answered `false` for a
    // code-filtered subscription rather than guessed `true`.
    let values = crate::push::FilterValues::from_event(event);

    let json = ups_event_json(event);
    let delivered = channels.broadcast(&json, |requester| {
        crate::push::subscriptions().accepts_for_with_values(
            requester,
            &event.workitem_uid,
            &values,
        )
    });
    if delivered > 0 {
        crate::metrics::WS_EVENTS_DELIVERED
            .fetch_add(delivered as u64, std::sync::atomic::Ordering::Relaxed);
    }
}

/// `GET /dicomweb/subscribers/{requester}` with WebSocket upgrade headers.
///
/// PS3.18 8.10.4 opens a Notification Connection scoped to `{requester}` - the
/// AE Title of the requesting user agent (Table 8.10.4-1a). Event Reports
/// (RAD-87) then travel over it.
///
/// # One writer, two transports
///
/// The Event Report payload is byte-identical to the SSE `data:` body: both
/// are produced by [`ups_event_json`] at the single publishing site
/// (`publish_ups_event`). Adding a transport must not add a second
/// serialisation, or the two would drift.
///
/// # Why the upgrade is decided here and not in the router
///
/// The route matches a plain GET as well. A request without the upgrade
/// headers is answered with the existing route-matrix behaviour (a normal JSON
/// response), so a client that mistakes the path for a REST resource gets a
/// coherent answer rather than a protocol error.
pub(crate) fn h_ups_notification_connect(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    let requester = ctx.uid(0);
    let ka = ctx.keep_alive == KeepAlive::On;

    // RFC 6455 4.2.1: both headers must be present, and `Upgrade` must name
    // the `websocket` token. Token comparison, not substring: `Upgrade:
    // websocketx` is not an upgrade request.
    let upgrade = ctx.head.upgrade;
    let is_upgrade = upgrade
        .is_some_and(|v| crate::handlers::websocket::header_has_token(v, "websocket"))
        && ctx.head.connection_upgrade;

    if !is_upgrade {
        // Not a WebSocket request: the resource answered as a plain GET. This
        // preserves the pre-existing behaviour on this path and is asserted by
        // the route-matrix test.
        let json = format!(
            "{{\"notificationConnection\":\"/dicomweb/subscribers/{requester}\",\
             \"transport\":\"websocket\",\"status\":\"upgrade required\"}}"
        );
        return write_resp(ctx.conn, 200, JSON_CT, json.as_bytes(), ka);
    }

    if requester.is_empty() {
        let body = crate::handlers::json_common::err_json("missing requester AE Title");
        return write_resp(ctx.conn, 400, JSON_CT, &body, ka);
    }

    // Validate the key and version before touching the registry, so a
    // malformed handshake never leaves a channel behind.
    let key = ctx
        .head
        .sec_websocket_key
        .and_then(|v| std::str::from_utf8(v).ok());
    let version = ctx
        .head
        .sec_websocket_version
        .and_then(|v| std::str::from_utf8(v).ok());

    let accept = match crate::handlers::websocket::evaluate_upgrade(key, version) {
        crate::handlers::websocket::Handshake::Accept(accept) => accept,
        crate::handlers::websocket::Handshake::VersionUnsupported => {
            // RFC 6455 4.2.2: 426 with the version we do support.
            ctx.conn.header.extend_from_slice(
                format!(
                    "HTTP/1.1 426 Upgrade Required\r\n\
                     Sec-WebSocket-Version: {}\r\n\
                     Content-Length: 0\r\n\
                     Connection: close\r\n\r\n",
                    crate::handlers::websocket::WS_VERSION
                )
                .as_bytes(),
            );
            return ctx.conn.write_head();
        }
        crate::handlers::websocket::Handshake::BadKey => {
            let body = crate::handlers::json_common::err_json(
                "Sec-WebSocket-Key must be 16 bytes of base64",
            );
            return write_resp(ctx.conn, 400, JSON_CT, &body, ka);
        }
    };

    // Register the channel BEFORE the 101 reaches the client, for the same
    // reason the SSE branch subscribes before its first keep-alive: once the
    // client believes the connection is open, a state change must not be able
    // to overtake the registration and be dropped.
    let Some((rx, channel_id)) = crate::ws_channels::channels().open(requester) else {
        let body = crate::handlers::json_common::err_json(&format!(
            "Too many notification connections for requester {requester}"
        ));
        return write_resp(ctx.conn, 503, JSON_CT, &body, ka);
    };
    crate::metrics::WS_CHANNELS_OPENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    crate::clog_info!(
        requester = %requester,
        channel_id,
        live = crate::ws_channels::channels().live_count(),
        "UPS notification connection opened"
    );

    // 101 Switching Protocols. `Sec-WebSocket-Protocol` is deliberately NOT
    // sent: this channel carries JSON text frames and defines no subprotocol,
    // so echoing a client offer would claim a negotiation we do not honour.
    ctx.conn.header.extend_from_slice(
        format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .as_bytes(),
    );
    ctx.conn.write_head()?;

    // From here on the connection is a WebSocket: every failure path must
    // remove the channel, so they all funnel through one loop exit.
    let result = ws_channel_loop(ctx, &rx, requester);

    let removed = crate::ws_channels::channels().close(channel_id);
    crate::clog_info!(
        requester = %requester,
        channel_id,
        removed,
        live = crate::ws_channels::channels().live_count(),
        "UPS notification connection closed"
    );
    result
}

/// Serve one open channel until it ends.
///
/// Two things happen here at once, which is why this is a loop rather than a
/// simple select: the connection thread must both PUSH queued Event Reports
/// and CONSUME client frames (close, ping). The socket carries a read timeout
/// so an idle channel still drains its queue promptly instead of waiting for
/// the client to say something.
fn ws_channel_loop(
    ctx: &mut RequestContext<'_>,
    rx: &std::sync::mpsc::Receiver<String>,
    requester: &str,
) -> io::Result<()> {
    use crate::handlers::websocket as ws;

    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];

    // The socket keeps the keep-alive deadline from the head read, which is far
    // longer than a notification channel should idle before re-checking its
    // queue. A short receive timeout here is what makes the loop below a poll
    // rather than a block; a timeout is an idle tick, not a closed connection.
    let idle = Duration::from_millis(250);

    loop {
        // 1. Drain everything the publishers queued. Non-blocking, so a fast
        //    producer cannot starve the read side.
        while let Ok(payload) = rx.try_recv() {
            let frame = ws::encode_frame(ws::opcode::TEXT, payload.as_bytes());
            if ctx.conn.write_all_configured(&frame).is_err() {
                return Ok(()); // peer went away mid-write
            }
            crate::metrics::WS_EVENTS_DELIVERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        // 2. Read whatever the client sent, with a short timeout so the queue
        //    above is revisited regularly. No server heartbeat: the AIW-I
        //    contract puts reconnection on the subscriber (Reliability
        //    statement), so an idle connection is a valid state.
        use std::io::Read as _;
        let _ =
            clarus_core::http::conn::set_timeouts(ctx.conn.stream(), idle, ctx.conn.send_timeout());
        match ctx.conn.stream_mut().read(&mut buf) {
            Ok(0) => return Ok(()), // TCP EOF
            Ok(n) => pending.extend_from_slice(&buf[..n]),
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                continue; // idle tick: go back and drain the queue
            }
            Err(e) => return Err(e),
        }

        // 3. Decode as many complete frames as the buffer holds.
        loop {
            match ws::decode_client_frame(&pending) {
                Ok((frame, used)) => {
                    pending.drain(..used);
                    match frame.opcode {
                        ws::opcode::CLOSE => {
                            // RFC 6455 5.5.1: answer with a close frame, then
                            // end the connection. The channel is removed by the
                            // caller's single exit path.
                            let _ = ctx.conn.write_all_configured(&ws::encode_close(1000));
                            return Ok(());
                        }
                        ws::opcode::PING => {
                            let pong = ws::encode_frame(ws::opcode::PONG, &frame.payload);
                            let _ = ctx.conn.write_all_configured(&pong);
                        }
                        _ => {
                            // Text/binary/continuation from the client: this
                            // channel is server-to-client, so the payload is
                            // read and discarded (fragments included - each is
                            // simply dropped). A subscriber has no way to
                            // influence the workitem through this channel.
                        }
                    }
                }
                Err(ws::FrameError::Incomplete) => break, // need more bytes
                Err(other) => {
                    crate::clog_debug!(
                        requester = %requester,
                        error = %format_args!("{other:?}"),
                        "notification connection: protocol error, closing"
                    );
                    // A protocol error is a close, not a crash: tell the client
                    // (1002 = protocol error) and end, so the registry entry is
                    // released by the same single exit path.
                    let _ = ctx.conn.write_all_configured(&ws::encode_close(1002));
                    return Ok(());
                }
            }
        }
    }
}

/// Build one log SSE data frame (`data: {json}\n\n`), byte-parity with 7b.
fn sse_data_frame(json: &str) -> Vec<u8> {
    format!("data: {json}\n\n").into_bytes()
}

/// The keep-alive / empty-poll tick comment emitted on an idle stream.
fn sse_tick() -> &'static str {
    ": tick\n\n"
}

/// Native `LogEntry` -> JSON string (byte-exact with the legacy
/// `serde_json::to_string` render): timestamp/level/component/message always,
/// tag present as string or null (no skip_serializing_if).
fn log_entry_json(entry: &crate::state::LogEntry) -> String {
    let mut b = Vec::new();
    b.extend_from_slice(b"{\"timestamp\":");
    push_json_str_loose(&mut b, &entry.timestamp);
    b.extend_from_slice(b",\"level\":");
    push_json_str_loose(&mut b, &entry.level);
    b.extend_from_slice(b",\"component\":");
    push_json_str_loose(&mut b, &entry.component);
    b.extend_from_slice(b",\"message\":");
    push_json_str_loose(&mut b, &entry.message);
    b.extend_from_slice(b",\"tag\":");
    match &entry.tag {
        Some(t) => push_json_str_loose(&mut b, t),
        None => b.extend_from_slice(b"null"),
    }
    b.push(b'}');
    // JSON here is pure ASCII (log lines are UTF-8 but need no escaping beyond
    // the control set); serde_json emits UTF-8 verbatim, so from_utf8 is safe.
    String::from_utf8(b).expect("log_entry_json is valid UTF-8")
}

/// Native `UpsEvent` -> the Event Report as a DICOM JSON Data Set.
///
/// PS3.18 8.10.5.1.1: "The data frame payload data shall be a DICOM JSON Data
/// Set containing the Attributes of the Event Report." An application-specific
/// envelope is therefore non-conformant, however convenient.
///
/// The Data Set carries the three attributes that identify and describe the
/// event, matching the Data Set the POST push already sends
/// (`push::create_body`), so a subscriber parses one shape from both
/// transports:
///
/// - `0000,1000` Affected SOP Instance UID - identifies the affected UPS. The
///   Event Report carries it as a COMMAND attribute alongside the affected SOP
///   Class UID, Message ID and Event Type ID; the group-0000 attributes are
///   what the report adds to the event-specific Data Set, and this is the one
///   a subscriber needs to route the report to a workitem.
/// - `0040,4041` Input Readiness State - PS3.4 CC.2.4.3: "the SCP shall give
///   the current value". The workitem's own value travels with the event; a
///   constant would report a readiness the workitem may not have.
/// - `0074,1000` Procedure Step State - the state the workitem is now in.
///
/// `previous_state`, `timestamp` and `event_id` are deliberately absent: they
/// are routing/transport facts, not Event Report attributes. `event_id` still
/// travels where it belongs - the SSE `id:` field - so Last-Event-ID resume is
/// unaffected.
///
/// Public only so the ap101 allocation gate can call it from its isolated
/// test binary (`tests/ap101_sse.rs`), where the cost of ONE event is pinned.
pub fn ups_event_json(e: &crate::handlers::ups::event_bus::UpsEvent) -> String {
    let mut b = Vec::with_capacity(220);
    b.extend_from_slice(b"{\"00001000\":{\"vr\":\"UI\",\"Value\":[");
    push_json_str_loose(&mut b, &e.workitem_uid);
    b.extend_from_slice(b"]},\"00404041\":{\"vr\":\"CS\",\"Value\":[");
    push_json_str_loose(&mut b, &e.input_readiness);
    b.extend_from_slice(b"]},\"00741000\":{\"vr\":\"CS\",\"Value\":[");
    push_json_str_loose(&mut b, &e.new_state);
    b.extend_from_slice(b"]}}");
    String::from_utf8(b).expect("ups_event_json is valid UTF-8")
}

// -------------------------------------------------------------------------
// 6. admin logs_stream - SSE of the ring buffer
// -------------------------------------------------------------------------

/// `GET /admin/v1/logs/stream` - SSE of recent + new log entries.
///
/// Mirrors the stage-7b golden: a snapshot of buffered entries that match the
/// filter, then a 500 ms poll loop that emits new entries; an empty poll emits
/// `: tick` so idle streams stay alive. The cursor is a wrapper-local variable;
/// reads take the buffer's read lock, writers hold the write lock, so there is
/// no race. Byte-parity with 7b is kept frame-for-frame.
pub(crate) fn h_admin_logs_stream(ctx: &mut RequestContext<'_>) -> io::Result<()> {
    use crate::handlers::admin::pure::LogFilter;

    let ka = ctx.keep_alive == KeepAlive::On;
    let filter = LogFilter::from_params(&log_params(ctx));

    // `chunked_start` declares `Transfer-Encoding: chunked`, so every body
    // write must be a `{hex}\r\n{payload}\r\n` frame - see the same note in
    // `h_ups_events`. A raw write here has the identical effect on a strict
    // client: it reads a payload byte as a chunk size and blocks.
    ctx.conn.chunked_start(200, "OK", SSE_CT, ka)?;

    // Phase 1: buffered snapshot (matches the filter), one SSE frame each.
    let snapshot = crate::handlers::admin::pure::initial_log_snapshot(ctx.state, &filter);
    for entry in &snapshot {
        let json = log_entry_json(entry);
        ctx.conn.chunked_data(&sse_data_frame(&json))?;
    }

    // Phase 2: poll every 500 ms for new entries; empty poll emits a tick.
    let mut cursor = ctx.state.log_buffer.read().map(|b| b.len()).unwrap_or(0);
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let fresh = crate::handlers::admin::pure::poll_logs(ctx.state, &mut cursor, &filter);
        if fresh.is_empty() {
            ctx.conn.chunked_data(sse_tick().as_bytes())?;
            continue;
        }
        for entry in &fresh {
            let json = log_entry_json(entry);
            ctx.conn.chunked_data(&sse_data_frame(&json))?;
        }
    }
}

/// Parse `LogStreamParams` from the query string (defaults when absent).
fn log_params(ctx: &RequestContext<'_>) -> crate::handlers::admin::pure::LogStreamParams {
    use crate::handlers::admin::pure::LogStreamParams;
    let mut p = LogStreamParams {
        level: "INFO".to_string(),
        component: "all".to_string(),
        tags: None,
    };
    if let Some(qs) = ctx.query_str() {
        let mut tags: Vec<String> = Vec::new();
        for pair in qs.split('&') {
            let mut it = pair.splitn(2, '=');
            let (Some(k), Some(v)) = (it.next(), it.next()) else {
                continue;
            };
            match k {
                "level" => p.level = v.to_string(),
                "component" => p.component = v.to_string(),
                "tags" => tags.push(v.to_string()),
                _ => {}
            }
        }
        if !tags.is_empty() {
            p.tags = Some(tags.join(","));
        }
    }
    p
}

// -------------------------------------------------------------------------
// Unit tests (streaming transport groups)
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    /// Build a connected `(server_conn, client_stream)` loopback pair.
    fn conn_pair() -> (Conn, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || TcpStream::connect(addr).unwrap());
        let (server, _) = listener.accept().unwrap();
        let client = client.join().unwrap();
        let conn = Conn::new(server, b"127.0.0.1".to_vec());
        (conn, client)
    }

    fn drain(mut c: TcpStream) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        loop {
            match c.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        out
    }

    // -- Group 1: sendfile (temp file + loopback; copy_loop fallback) -----

    /// The media type this crate serves a transcoded body under names the
    /// transfer syntax those bytes are actually in.
    ///
    /// `WadoResp::content_type` is a `&'static str` and `concat!` needs
    /// literals, so the UID is written once here rather than derived. This
    /// test is what keeps that copy honest: a drift between it and
    /// `ts_uid::EXPLICIT_VR_LE` fails the build's tests instead of shipping a
    /// response whose header contradicts its body.
    #[test]
    fn the_served_media_type_names_the_explicit_vr_little_endian_syntax() {
        assert_eq!(
            DICOM_EVRLE_CT,
            format!(
                "application/dicom; transfer-syntax={}",
                clarus_adapters::codec::ts_uid::EXPLICIT_VR_LE
            ),
            "the served media type has to name the syntax the bytes are in"
        );
    }

    #[test]
    fn sendfile_response_streams_file_bytes() {
        let bytes: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
        let dir = std::env::temp_dir();
        let path = dir.join(format!("clarus_sendfile_test_{}", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();

        let (conn, client) = conn_pair();
        let file = std::fs::File::open(&path).expect("open file");
        let worker = std::thread::spawn(move || {
            let head = b"HTTP/1.1 200 OK\r\nContent-Length: 2000\r\n\r\n";
            clarus_core::http::response::sendfile_response(conn.stream(), head, &file, bytes2len())
        });
        let received = drain(client);
        worker.join().unwrap().expect("sendfile ok");
        // On Windows this is the copy_loop fallback; both paths must round-trip.
        assert!(received.starts_with(b"HTTP/1.1 200 OK"));
        assert_eq!(&received[received.len() - bytes.len()..], &bytes[..]);
        let _ = std::fs::remove_file(&path);
    }

    fn bytes2len() -> u64 {
        2000
    }

    #[test]
    fn sendfile_copy_loop_fallback_matches() {
        // The fallback (copy_loop) is what Windows uses behind sendfile_response;
        // verify a non-trivial payload round-trips byte-for-byte through the
        // connection so the fallback path is exercised, not just sendfile.
        let payload: Vec<u8> = (0..8192u32).map(|i| (i * 7 % 256) as u8).collect();
        let dir = std::env::temp_dir();
        let path = dir.join(format!("clarus_sendfile_test2_{}", std::process::id()));
        std::fs::write(&path, &payload).unwrap();
        let (mut conn, client) = conn_pair();
        let file = std::fs::File::open(&path).unwrap();
        let worker = std::thread::spawn(move || {
            conn.send_file(200, "OK", DICOM_CT, &file, payload_len(), false)
        });
        let received = drain(client);
        worker.join().unwrap().unwrap();
        assert_eq!(&received[received.len() - payload.len()..], &payload[..]);
        let _ = std::fs::remove_file(&path);
    }

    fn payload_len() -> u64 {
        8192
    }

    // -- Group 2: chunked multipart wire framing --------------------------

    #[test]
    fn chunked_multipart_two_parts_hex_framing() {
        let boundary = "testboundary123";
        let (mut conn, client) = conn_pair();
        let worker = std::thread::spawn(move || {
            conn.chunked_start(
                200,
                "OK",
                b"multipart/related; boundary=testboundary123",
                true,
            )?;
            let p1 = format!(
                "--{boundary}\r\nContent-Type: application/dicom\r\nContent-Type: application/dicom\r\nContent-Length: 3\r\n\r\nAAA"
            );
            conn.chunked_data(p1.as_bytes())?;
            let p2 = format!(
                "\r\n--{boundary}\r\nContent-Type: application/dicom\r\nContent-Length: 3\r\n\r\nBBB"
            );
            conn.chunked_data(p2.as_bytes())?;
            let close = format!("\r\n--{boundary}--\r\n");
            conn.chunked_data(close.as_bytes())?;
            conn.chunked_end()
        });
        let raw = drain(client);
        worker.join().unwrap().unwrap();
        let raw_str = String::from_utf8_lossy(&raw);
        // Hex-framed: each chunk begins with a length prefix and "0\r\n\r\n" ends it.
        assert!(raw_str.contains("\r\n--testboundary123\r\n"));
        assert!(raw_str.contains("\r\n--testboundary123--\r\n"));
        assert!(raw_str.ends_with("0\r\n\r\n"));
        // The HTTP head must be chunked (no Content-Length); part-level
        // Content-Length headers inside the MIME body are expected.
        let head_part = raw_str.split("\r\n\r\n").next().unwrap_or("");
        assert!(head_part.contains("Transfer-Encoding: chunked"));
        assert!(!head_part.contains("Content-Length"));
    }

    // -- Honest bytes= reporting --------------------------------------------

    /// Minimal AppState for streaming-part tests (mirror of serve_smoke.rs).
    fn stream_minimal_state() -> (crate::state::AppState, tempfile::TempDir) {
        use crate::config::Config;
        use crate::shadow::ShadowTier;
        use crate::sse_fanout::DualEventSink;
        use crate::state::MaintenanceState;
        use clarus_adapters::storage::ContentAddressedStorage;
        use clarus_core::query::StudyScanner;
        use std::collections::VecDeque;
        use std::sync::{Arc, RwLock as StdRwLock};

        let temp = tempfile::tempdir().expect("temp dir");
        let storage_dir = temp.path().join("cas");
        std::fs::create_dir_all(&storage_dir).expect("cas dir");
        let btree_root = temp.path().join("btree");
        let manifest_dir = btree_root.join("manifests");
        let idx_dir = btree_root.join("idx");
        std::fs::create_dir_all(&manifest_dir).unwrap();
        std::fs::create_dir_all(&idx_dir).unwrap();
        let cas = ContentAddressedStorage::new(storage_dir.clone()).expect("open cas");
        let mut config = Config::test_default();
        config.storage_dir = storage_dir.to_string_lossy().to_string();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;
        let state = crate::state::AppState {
            cas,
            codec_registry: clarus_adapters::codec::CodecRegistry::new(),
            ups_events: DualEventSink::new(),
            start_time: std::time::Instant::now(),
            metrics_ring: Arc::new(StdRwLock::new(VecDeque::new())),
            log_buffer: Arc::new(StdRwLock::new(VecDeque::new())),
            maintenance: Arc::new(StdRwLock::new(MaintenanceState::default())),
            #[cfg(feature = "s3")]
            s3: Arc::new(std::sync::OnceLock::new()),
            cold_cas: None,
            shadow: ShadowTier::new(btree_root.join("idx")),
            study_scanner: StudyScanner::new(manifest_dir, idx_dir),
            config,
            workitems_dir: btree_root.join("workitems"),
            commitments_dir: btree_root.join("commitments"),
            transcode_pool: crate::state::TranscodePool::new(32),
            readiness: crate::handlers::readiness::Readiness::new(),
        };
        (state, temp)
    }

    /// A two-part StreamAsIs study multipart: the access-log `bytes=` counter
    /// must equal the bytes actually written to the wire (head + body). RED
    /// pre-fix: the File body is written via sendfile_copy_loop bypassing
    /// Conn::response_bytes, so bytes= undercounted (observed 66575 B for a
    /// ~468 MB study).
    #[test]
    fn multipart_access_bytes_counts_streamed_file_body() {
        use crate::handlers::wado_pure::{PartKind, PartPlan};

        let (state, _temp) = stream_minimal_state();

        // Two distinct stored CAS blobs; each becomes one StreamAsIs part.
        // `cas_path` returns the two-level path but does not create parents.
        let body1: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let body2: Vec<u8> = (4096..8192u32).map(|i| (i % 251) as u8).collect();
        let h1 = clarus_adapters::hex_encode(&clarus_core::hash::cas_hash_one(&body1));
        let h2 = clarus_adapters::hex_encode(&clarus_core::hash::cas_hash_one(&body2));
        let p1 = state.cas.cas_path(&h1);
        let p2 = state.cas.cas_path(&h2);
        std::fs::create_dir_all(p1.parent().unwrap()).unwrap();
        std::fs::create_dir_all(p2.parent().unwrap()).unwrap();
        std::fs::write(&p1, &body1).unwrap();
        std::fs::write(&p2, &body2).unwrap();
        // Leak so the `&AppState` borrow can cross the worker-thread 'static
        // bound (test-only; the store is deliberately kept alive).
        let state: &'static crate::state::AppState = Box::leak(Box::new(state));

        let plans = vec![
            PartPlan {
                sha256: h1,
                sop_uid: "1.2.3.4.5.6.1".to_string(),
                sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".to_string(),
                source_ts: "1.2.840.10008.1.2.1".to_string(),
                storage_tier: StorageTier::Hot,
                file_size: body1.len() as u64,
                kind: PartKind::StreamAsIs,
                series_uid: String::new(),
                with_content_location: false,
            },
            PartPlan {
                sha256: h2,
                sop_uid: "1.2.3.4.5.6.2".to_string(),
                sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".to_string(),
                source_ts: "1.2.840.10008.1.2.1".to_string(),
                storage_tier: StorageTier::Hot,
                file_size: body2.len() as u64,
                kind: PartKind::StreamAsIs,
                series_uid: String::new(),
                with_content_location: false,
            },
        ];

        let (mut conn, client) = conn_pair();
        // Drain the client on a separate thread while this thread serves into
        // the loopback pair (so the serving `ctx` stays on this thread - its
        // `&head`/`&mut conn` borrows are not 'static).
        let client_reader = std::thread::spawn(move || drain(client));
        let head = RequestHead::borrow_empty();
        let empty: [&[u8]; 3] = [&[], &[], &[]];
        // The multipart writer never consults the route: it already has the
        // plans it must stream, and the expose decision was made upstream.
        let route = clarus_core::http::router::RouteMatch {
            id: "wado_study",
            access: clarus_core::http::router::Access::Default,
            keep_alive: KeepAlive::Off,
            uids: empty,
            n_uids: 0,
            qa: false,
        };
        let counter = {
            let mut ctx = RequestContext {
                head: &head,
                uids: empty,
                n_uids: 0,
                client_ip: b"127.0.0.1".as_slice(),
                user: b"(loopback)".as_slice(),
                via: "loopback",
                state,
                keep_alive: KeepAlive::On,
                conn: &mut conn,
                wado_series_expected: None,
                stow_accepted: 0,
                proxy_trusted: false,
                route,
            };
            serve_multipart_chunked(&mut ctx, plans, None).expect("serve multipart");
            ctx.conn.response_bytes()
        };
        // Close the server side so the drainer sees EOF and returns promptly.
        drop(conn);
        let received = client_reader.join().unwrap();
        assert!(!received.is_empty(), "multipart response should have bytes");
        assert_eq!(
            counter,
            received.len() as u64,
            "access bytes= must equal actual bytes written (head + both file bodies)"
        );
    }

    // -- Group 3: STOW shim (MultipartParser over CL and chunked) ---------

    #[test]
    fn stow_multipart_parses_content_length_body() {
        let boundary = "abc123";
        let body = format!(
            "--{boundary}\r\nContent-Type: application/dicom\r\n\r\nBODY1\r\n--{boundary}\r\nContent-Type: application/json\r\n\r\n{{\"x\":1}}\r\n--{boundary}--\r\n"
        );
        let mut parser = MultipartParser::new(body.as_bytes(), boundary.as_bytes()).unwrap();
        let mut parts = 0;
        let mut data = Vec::new();
        loop {
            match parser.next_event().unwrap() {
                MultipartEvent::Part(_) => parts += 1,
                MultipartEvent::Data(d) => data.extend_from_slice(d),
                MultipartEvent::End => break,
            }
        }
        assert_eq!(parts, 2);
        // DICOM body bytes are present; the JSON metadata part is drained too.
        assert!(data.windows(5).any(|w| w == b"BODY1"));
    }

    #[test]
    fn stow_multipart_parses_chunked_body() {
        let boundary = "xyz";
        let payload = format!("--{boundary}\r\n\r\nABCD\r\n--{boundary}--\r\n");
        // Encode as a chunked body and decode via ChunkedReader.
        let chunked = format!("{:x}\r\n{}\r\n0\r\n\r\n", payload.len(), payload);
        let mut cr = ChunkedReader::new(chunked.as_bytes());
        let mut out = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            let n = cr.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        let mut parser = MultipartParser::new(&out[..], boundary.as_bytes()).unwrap();
        let mut body = Vec::new();
        loop {
            match parser.next_event().unwrap() {
                MultipartEvent::Part(_) => {}
                MultipartEvent::Data(d) => body.extend_from_slice(d),
                MultipartEvent::End => break,
            }
        }
        assert_eq!(body, b"ABCD");
    }

    #[test]
    fn extract_boundary_parses_quoted_value() {
        assert_eq!(
            extract_boundary("multipart/related; boundary=\"zzz9\"").as_deref(),
            Some("zzz9")
        );
        assert_eq!(
            extract_boundary("multipart/related; boundary=bare").as_deref(),
            Some("bare")
        );
        assert_eq!(extract_boundary("multipart/related"), None);
    }

    #[test]
    fn extract_boundary_parses_unquoted_no_space_value() {
        // Weasis (Java HttpClient) form: compact separators, unquoted value.
        assert_eq!(
            extract_boundary(
                "multipart/related;type=\"application/dicom\";boundary=weasisDicomBoundary;"
            )
            .as_deref(),
            Some("weasisDicomBoundary")
        );
        // Quoted value followed by `;` still works.
        assert_eq!(
            extract_boundary("multipart/related;type=x;boundary=\"q1\";foo=bar").as_deref(),
            Some("q1")
        );
    }

    // -- Group 5: logs_stream SSE frames (byte-parity with stage 7b) -----

    #[test]
    fn logs_data_frame_matches_golden() {
        // The axum 7b endpoint emits `data: {json}\n\n`; the sync wrapper must
        // reproduce it byte-for-byte.
        let json = "{\"level\":\"INFO\",\"message\":\"hi\"}";
        assert_eq!(
            sse_data_frame(json),
            b"data: {\"level\":\"INFO\",\"message\":\"hi\"}\n\n"
        );
        // Empty poll -> keep-alive tick comment.
        assert_eq!(sse_tick(), ": tick\n\n");
    }

    #[test]
    fn ups_sse_frame_wire_order() {
        let f = ups_sse_frame(7, "{\"workitem_uid\":\"1.2.3\"}");
        let s = String::from_utf8_lossy(&f);
        assert!(s.starts_with("event: StateChange\nid: 7\ndata:"));
        assert!(s.ends_with("\n\n"));
    }

    /// `ups_event_json` renders the Event Report as a DICOM JSON Data Set
    /// (PS3.18 8.10.5.1.1), with the three attributes PS3.4 CC.2.4.3 and the
    /// UPS IOD require - and NOTHING of the retired envelope.
    #[test]
    fn ups_event_json_is_a_dicom_json_data_set() {
        use crate::handlers::ups::event_bus::UpsEvent;
        let e = UpsEvent {
            workitem_uid: "1.2.3.workitem".into(),
            previous_state: "SCHEDULED".into(),
            new_state: "IN PROGRESS".into(),
            timestamp: "1700000000".into(),
            event_id: 42,
            step_label: String::new(),
            scheduled_code: None,
            input_readiness: "READY".into(),
            filter: Box::default(),
        };
        assert_eq!(
            ups_event_json(&e),
            "{\"00001000\":{\"vr\":\"UI\",\"Value\":[\"1.2.3.workitem\"]},\
             \"00404041\":{\"vr\":\"CS\",\"Value\":[\"READY\"]},\
             \"00741000\":{\"vr\":\"CS\",\"Value\":[\"IN PROGRESS\"]}}"
        );
    }

    /// The negative half: the retired envelope keys must be ABSENT, not merely
    /// accompanied. A payload that carried both would still be non-conformant,
    /// and a test asserting only the new keys could not tell.
    #[test]
    fn ups_event_json_carries_no_envelope_keys() {
        use crate::handlers::ups::event_bus::UpsEvent;
        let e = UpsEvent {
            workitem_uid: "1.2.3.workitem".into(),
            previous_state: "SCHEDULED".into(),
            new_state: "IN PROGRESS".into(),
            timestamp: "1700000000".into(),
            event_id: 42,
            step_label: String::new(),
            scheduled_code: None,
            input_readiness: "READY".into(),
            filter: Box::default(),
        };
        let json = ups_event_json(&e);
        for key in [
            "\"workitem_uid\"",
            "\"previous_state\"",
            "\"new_state\"",
            "\"timestamp\"",
            "\"event_id\"",
        ] {
            assert!(
                !json.contains(key),
                "the Event Report must not carry the envelope key {key}: {json}"
            );
        }
        // And every top-level key really is a DICOM tag.
        for tag in ["\"00001000\"", "\"00404041\"", "\"00741000\""] {
            assert!(json.contains(tag), "missing {tag} in {json}");
        }
    }

    // -- content_type_is: the base media type of a Content-Type -----------

    #[test]
    fn a_content_type_parameter_does_not_hide_the_media_type() {
        // PS3.18 8.7.3.1 gives application/dicom an optional transfer-syntax
        // parameter, and 8.7.3.1's spelling is what a conformant single-part
        // STOW-RS client sends. Matching the header verbatim refused it with
        // "Expected multipart/related Content-Type", which is the defect this
        // pins.
        assert!(content_type_is(
            "application/dicom; transfer-syntax=1.2.840.10008.1.2.1",
            "application/dicom"
        ));
        // The parameter may be spaced, quoted, or absent.
        assert!(content_type_is("application/dicom", "application/dicom"));
        assert!(content_type_is(
            "application/dicom;transfer-syntax=1.2.840.10008.1.2.1",
            "application/dicom"
        ));
        assert!(content_type_is(
            "  application/dicom ; transfer-syntax=1.2.840.10008.1.2.1  ",
            "application/dicom"
        ));
        assert!(content_type_is(
            "application/dicom; transfer-syntax=\"1.2.840.10008.1.2.1\"",
            "application/dicom"
        ));
    }

    #[test]
    fn a_content_type_is_compared_case_insensitively() {
        // RFC 9110 8.3.1: type and subtype are case-insensitive.
        assert!(content_type_is("APPLICATION/DICOM", "application/dicom"));
        assert!(content_type_is(
            "Application/Dicom; transfer-syntax=1.2.840.10008.1.2.1",
            "application/dicom"
        ));
    }

    #[test]
    fn a_structured_syntax_subtype_is_not_the_base_type() {
        // The suffix is part of the subtype, not a parameter: `+json` has no
        // `;` in front of it, so the base of `application/dicom+json` is the
        // whole string and must not equal `application/dicom`. This is the
        // invariant the Accept-side comment states, held on the request side
        // too by sharing one reduction rule.
        assert!(!content_type_is(
            "application/dicom+json",
            "application/dicom"
        ));
        assert!(!content_type_is(
            "application/dicom+xml",
            "application/dicom"
        ));
        assert!(!content_type_is(
            "application/dicom+json; charset=utf-8",
            "application/dicom"
        ));
        // The other direction: the base is a strict equality, not a prefix.
        assert!(!content_type_is(
            "application/dicom",
            "application/dicom+json"
        ));
        assert!(!content_type_is("multipart/related", "application/dicom"));
        assert!(!content_type_is(
            "multipart/related; type=\"application/dicom\"",
            "application/dicom"
        ));
        assert!(!content_type_is(
            "application/octet-stream",
            "application/dicom"
        ));
    }

    #[test]
    fn an_empty_media_type_matches_nothing() {
        // A header of `;param` names no type; an absent Content-Type arrives
        // as the empty string. Neither is an application/dicom body.
        assert!(!content_type_is("", "application/dicom"));
        assert!(!content_type_is(
            ";transfer-syntax=1.2.3",
            "application/dicom"
        ));
        assert!(!content_type_is("   ", "application/dicom"));
    }

    #[test]
    fn the_accept_and_content_type_sides_agree_on_a_media_type() {
        // The two sides reduce a media type with the same rule now; a single
        // range of an Accept header and a Content-Type header carrying the
        // same value must classify the same way.
        for header in [
            "application/dicom",
            "application/dicom; transfer-syntax=1.2.840.10008.1.2.1",
            "APPLICATION/DICOM",
            "application/dicom+json",
            "multipart/related",
        ] {
            assert_eq!(
                accept_media_type_present_ci(header, "application/dicom"),
                content_type_is(header, "application/dicom"),
                "the two sides disagree about {header}"
            );
        }
    }
}
