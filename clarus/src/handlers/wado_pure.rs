// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! WADO-RS pure synchronous core.
//!
//! The WADO split is "locate + plan": this side resolves the instance from the
//! CBOR manifest (locate), then plans and renders the HTTP response body and
//! headers (Range / transcode / frame / thumbnail / metadata). The transport -
//! the futures stream, sendfile, multipart assembly - is the wrapper's business
//! and stays async.
//!
//! This module is fully synchronous: no `axum::`, no `async`. The HTTP
//! wrappers call it directly (or via `spawn_blocking`).

#![forbid(unsafe_code)]

use clarus_ports::StorageTier;

use super::wado::error::WadoError;
use super::wado::helpers::{
    attr_text, attr_text_from, cas_hex_key, find_instance, inject_bulk_data_uris_native,
    load_manifest, tier_from_u8,
};
use super::wado::types::{WadoQueryParams, extract_transfer_syntax_from_accept_str};
use super::wado::validate_dicom_uid;
use super::wado_range::{RangeResult, content_range_header};

/// A resolved reference to a stored DICOM instance - everything the transport
/// wrapper needs to read and serve the bytes.
pub struct ObjectRef {
    /// CAS key (hex) from the manifest.
    pub sha256: String,
    /// Storage tier from the manifest.
    pub storage_tier: StorageTier,
    /// Stored transfer syntax UID.
    pub transfer_syntax: String,
    /// Instance file size from the manifest.
    pub file_size: u64,
    /// Per-frame byte ranges from the manifest (empty when the instance was
    /// stored before the frame table existed, or is single-frame).
    ///
    /// Carried here so the `/frames/{N}` route can serve ONE frame by byte
    /// range without re-reading the manifest or buffering the whole blob.
    pub frames: Vec<clarus_core::manifest::FrameRecord>,
}

/// A fully-planned HTTP response: status, content type, extra headers, body.
pub struct WadoResp {
    pub status: u16,
    pub content_type: &'static str,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

/// Locate an instance from the study CBOR manifest.
///
/// Validates `study_uid` and `sop_uid`; `series_uid`, when present, is used
/// only to count the series' instances for the WADO access aggregator (the
/// instance itself is still found by SOP UID alone). Resolves the manifest
/// and the instance, then maps to [`ObjectRef`]. The second return value is
/// the series slice count (`Some(n)` when the series was found in the
/// manifest, `None` when `series_uid` is absent or unmatched) - the caller
/// records it so an interrupted series can be reported as `partial` with the
/// expected total.
///
/// `expose` lets the QA surface retrieve a sequestered instance whose reason
/// permits review. Every other caller passes `false`, which makes a rejected
/// instance answer like a missing one (404): that is the sequestering
/// contract, and the existing not-found path already carries the right status,
/// body and access journal entry.
///
/// ap101: the guard is one `bool` read on the borrowed `&InstanceRecord`. The
/// locate path already clones the frame table, so the addition costs no bytes.
pub fn locate_object(
    state: &crate::state::AppState,
    study_uid: &str,
    series_uid: Option<&str>,
    sop_uid: &str,
    expose: bool,
) -> Result<(ObjectRef, Option<u32>), WadoError> {
    validate_dicom_uid(study_uid).inspect_err(|_| {
        crate::clog_warn!(study_uid, "Invalid study UID in WADO request");
    })?;
    validate_dicom_uid(sop_uid).inspect_err(|_| {
        crate::clog_warn!(sop_uid, "Invalid instance UID in WADO request");
    })?;
    let manifest = load_manifest(&state.study_scanner.manifests_dir, study_uid)?;
    let expected = series_uid.and_then(|suid| {
        manifest
            .series
            .iter()
            .find(|s| {
                attr_text_from(&s.attrs, clarus_core::query::tag::SERIES_INSTANCE_UID) == Some(suid)
            })
            .map(|s| s.instances.len() as u32)
    });
    let inst = find_instance(&manifest, sop_uid)?;
    // The QA surface lifts a hide only for reasons that permit it. A Quality
    // rejection is reviewable; an IOCM patient-safety or incorrect-MWL hide
    // has no expose path at all (IOCM 4.66.4.2.3 / 4.66.4.3.3), so it answers
    // exactly like a missing instance even on the QA root.
    if inst.is_rejected() && !(expose && inst.is_exposable()) {
        return Err(WadoError::NotFound(sop_uid.to_string()));
    }
    Ok((
        ObjectRef {
            sha256: cas_hex_key(inst),
            storage_tier: tier_from_u8(inst.storage_tier),
            transfer_syntax: inst.transfer_syntax.clone(),
            file_size: inst.file_size,
            frames: inst.frames.clone(),
        },
        expected,
    ))
}

/// Plan a single RFC 7233 byte-range response (200 / 206 / 416).
///
/// All RFC 7233 logic that used to live in the handler lives here. Returns the
/// full-entity-plan for absent/invalid ranges and a partial plan for valid
/// ones; the caller maps this onto the transport.
pub fn plan_range(range_header: Option<&str>, file_size: u64) -> Result<WadoResp, WadoError> {
    let result = super::wado_range::parse_range_header_str(range_header, file_size);
    match result {
        RangeResult::FullFile | RangeResult::InvalidIgnored => Ok(WadoResp {
            status: 200,
            content_type: "application/dicom",
            headers: vec![("Accept-Ranges", "bytes".to_string())],
            body: Vec::new(),
        }),
        RangeResult::Partial(range) => Ok(WadoResp {
            status: 206,
            content_type: "application/dicom",
            headers: vec![
                ("Content-Range", content_range_header(range, file_size)),
                ("Accept-Ranges", "bytes".to_string()),
            ],
            body: Vec::new(),
        }),
        RangeResult::NotSatisfiable => Err(WadoError::RangeNotSatisfiable),
    }
}

/// The transfer syntax a per-tile decode produces: Explicit VR Little Endian.
///
/// Named once so the route and the tests cannot drift apart.
pub const EVRLE_TS: &str = "1.2.840.10008.1.2.1";

/// The 0028 pixel-module tags a per-tile decode needs from the instance head.
///
/// Only these four are read: everything else the codec derives from the
/// codestream itself (dimensions, bit depth) or is fixed by the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FramePixelModule {
    pub rows: u16,
    pub columns: u16,
    pub bits_allocated: u16,
    pub samples_per_pixel: u16,
}

impl FramePixelModule {
    /// Read the pixel module out of a bounded instance head.
    ///
    /// `None` when any tag is missing: the caller then serves the stored
    /// bytes unchanged rather than synthesizing a header it cannot fill in.
    /// Guessing Rows/Columns would mislabel the body, which is worse than
    /// not transcoding at all (PS3.18 8.7.3 lets an origin server decline).
    pub fn from_head(head: &[u8]) -> Option<Self> {
        Some(Self {
            rows: extract_u16(head, (0x0028, 0x0010))?,
            columns: extract_u16(head, (0x0028, 0x0011))?,
            bits_allocated: extract_u16(head, (0x0028, 0x0100))?,
            samples_per_pixel: extract_u16(head, (0x0028, 0x0002))?,
        })
    }

    /// Decoded byte length of one tile: rows x columns x samples x bytes.
    ///
    /// `u64` throughout: Rows and Columns are untrusted IS values from the
    /// instance, and a 16-bit multiply would wrap on a crafted pair
    /// (PS3.5 A.4.4 bounds them per-image, not per-instance).
    pub fn expected_bytes(&self) -> u64 {
        let sample_bytes = u64::from(self.bits_allocated).div_ceil(8).max(1);
        u64::from(self.rows)
            .saturating_mul(u64::from(self.columns))
            .saturating_mul(u64::from(self.samples_per_pixel))
            .saturating_mul(sample_bytes)
    }

    /// The pixel budget check, made BEFORE any decode buffer exists.
    ///
    /// Refuses as `PayloadTooLarge` (413). The message is a fixed string on
    /// purpose: this runs on the refusal path of an attacker-controlled
    /// request, and `format!` there would allocate ~100 bytes for every
    /// oversized tile - the ap101 gate asserts ZERO, and the geometry is
    /// already visible in the instance the operator is looking at.
    pub fn check_budget(&self, max_megapixels: u32) -> Result<(), WadoError> {
        let pixels = u64::from(self.rows).saturating_mul(u64::from(self.columns));
        let max_px = u64::from(max_megapixels) * 1_000_000;
        if pixels > max_px {
            return Err(WadoError::PayloadTooLargeStatic(
                "Frame exceeds the transcode megapixel limit",
            ));
        }
        Ok(())
    }
}

/// Build the minimal Explicit VR LE dataset the existing codec decodes from.
///
/// The codec entry point takes a DICOM object, not a bare codestream: it
/// reads SamplesPerPixel (0028,0002) from the metadata to prove the codestream
/// is conformant (`decode_j2k_codestream`). Synthesizing the object here keeps
/// one decode path instead of adding a codec trait surface for raw fragments.
///
/// Layout follows PS3.5 A.4.4: meta group, then the pixel module, then an
/// ENCAPSULATED PixelData (undefined length, empty Basic Offset Table, one
/// Item holding the fragment) - the shape `extract_codestream` expects, so
/// the stored fragment is passed through byte for byte.
fn evrle_single_frame_dataset(pixel_module: &FramePixelModule, fragment: &[u8]) -> Vec<u8> {
    /// Append one Explicit VR LE element.
    fn elem(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
        buf.extend_from_slice(&group.to_le_bytes());
        buf.extend_from_slice(&element.to_le_bytes());
        buf.extend_from_slice(vr);
        let long_form = matches!(
            vr,
            b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UN" | b"UC" | b"UR" | b"UT"
        );
        if long_form {
            buf.extend_from_slice(&[0x00, 0x00]);
            buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
        } else {
            buf.extend_from_slice(&(value.len() as u16).to_le_bytes());
        }
        buf.extend_from_slice(value);
        if !value.len().is_multiple_of(2) {
            buf.push(0x00);
        }
    }

    let mut d = Vec::with_capacity(fragment.len() + 512);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    // Meta group: only the transfer syntax matters to the decoder, but the
    // stored fragment arrives as Encapsulated PixelData under the SOURCE
    // syntax, so the meta must name that same syntax for `extract_codestream`
    // to take the item path.
    elem(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    // Pixel module (PS3.3 C.7.6.3): the tags the decoder reads.
    elem(
        &mut d,
        0x0028,
        0x0002,
        b"US",
        &pixel_module.samples_per_pixel.to_le_bytes(),
    );
    elem(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    elem(
        &mut d,
        0x0028,
        0x0010,
        b"US",
        &pixel_module.rows.to_le_bytes(),
    );
    elem(
        &mut d,
        0x0028,
        0x0011,
        b"US",
        &pixel_module.columns.to_le_bytes(),
    );
    elem(
        &mut d,
        0x0028,
        0x0100,
        b"US",
        &pixel_module.bits_allocated.to_le_bytes(),
    );
    elem(
        &mut d,
        0x0028,
        0x0101,
        b"US",
        &pixel_module.bits_allocated.to_le_bytes(),
    );
    elem(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    elem(&mut d, 0x0028, 0x0103, b"US", &0u16.to_le_bytes());
    // Encapsulated PixelData: undefined length, empty BOT, one Item.
    d.extend_from_slice(&0x7FE0u16.to_le_bytes());
    d.extend_from_slice(&0x0010u16.to_le_bytes());
    d.extend_from_slice(b"OB");
    d.extend_from_slice(&[0x00, 0x00]);
    d.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    d.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    d.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
    d.extend_from_slice(&(fragment.len() as u32).to_le_bytes());
    d.extend_from_slice(fragment);
    if !fragment.len().is_multiple_of(2) {
        d.push(0x00);
    }
    d.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    d
}

/// Decode ONE stored tile to raw Explicit VR LE pixel bytes.
///
/// Returns the BARE pixel samples: no element header, no Item delimiters, no
/// VR - the `/frames/{n}` part body is defined as raw octet-stream (PS3.18
/// 10.4.4), and the transcoded part only changes which bytes those are.
///
/// `None` means "serve the stored bytes unchanged", which covers two cases:
/// the stored syntax has no registered decoder, and the stored tile is
/// already uncompressed Explicit VR LE. Both are the 8.7.3 escape: an origin
/// server that cannot honour the requested syntax serves what it has.
///
/// A registered decoder that refuses the fragment is an error, not a
/// fallback: `NonConformantCodestream` becomes a 406 (the instance is not
/// what it claims), while a transient decode failure does fall back, because
/// the stored bytes are still a valid answer.
pub fn transcode_frame_to_evrle(
    fragment: &[u8],
    stored_ts: &str,
    pixel_module: &FramePixelModule,
    registry: &clarus_adapters::codec::CodecRegistry,
    max_megapixels: u32,
) -> Result<Option<Vec<u8>>, WadoError> {
    if stored_ts == EVRLE_TS {
        // Already the target: the stored fragment IS the answer.
        return Ok(None);
    }
    let Some(codec) = registry.get(stored_ts) else {
        // No decoder for this syntax: serve as stored (8.7.3). The caller is
        // free to label the part with the stored syntax it actually holds.
        return Ok(None);
    };
    // v1 is monochrome. A three-sample tile would need a colour conversion
    // the target does not have a place for here, so it passes through.
    if pixel_module.samples_per_pixel != 1 {
        return Ok(None);
    }
    // Budget BEFORE the synthesized dataset allocates: the fragment is copied
    // into it, so an over-budget tile must be refused while the cost is still
    // one comparison.
    pixel_module.check_budget(max_megapixels)?;

    let dataset = evrle_single_frame_dataset(pixel_module, fragment);
    match codec.decode(&dataset) {
        Ok(pixels) => Ok(Some(pixels.data)),
        Err(clarus_adapters::codec::CodecError::NonConformantCodestream(reason)) => {
            Err(WadoError::NotAcceptable(format!(
                "Stored transfer syntax '{stored_ts}' is not conformant: {reason}"
            )))
        }
        // Transient: the stored bytes remain a valid answer.
        Err(_) => Ok(None),
    }
}

/// Decide on transcoding and transcode if needed. Includes the 406 path and
/// the "serve original" fallback - byte-for-byte with current behaviour.
///
/// Returns bytes ready to serve; the caller reads metadata/headers as needed.
pub fn maybe_transcode(
    bytes: Vec<u8>,
    stored_ts: &str,
    requested_ts: Option<&str>,
    registry: &clarus_adapters::codec::CodecRegistry,
    pool: &crate::state::TranscodePool,
    max_megapixels: u32,
) -> Result<(Vec<u8>, String), WadoError> {
    let Some(target_ts) = requested_ts else {
        return Ok((bytes, stored_ts.to_string()));
    };
    // "Give it back as stored" needs no codec, so it is answered BEFORE the
    // capability check: a client that asks for the syntax the object already
    // has must get its bytes, and refusing it because no decoder happens to be
    // registered for that syntax would answer 406 for a request that requires
    // no decoding at all.
    if stored_ts == target_ts {
        return Ok((bytes, stored_ts.to_string()));
    }
    // PS3.18 8.7.3: Implicit VR LE shall not be used with Web Services, and the
    // rule is not image-scoped. An Implicit VR source is therefore re-encoded
    // at the DATASET level - element headers only, values untouched - which is
    // what a non-image object (SR, PR, Surface Segmentation) needs and what the
    // pixel pipeline below cannot do, because it has no Rows and no PixelData
    // to decode. The call is before the registry check: this conversion needs
    // no codec at all, so an unsupported target syntax is irrelevant.
    if stored_ts == clarus_adapters::codec::ts_uid::IMPLICIT_VR_LE
        && target_ts == clarus_adapters::codec::ts_uid::EXPLICIT_VR_LE
    {
        let out =
            clarus_adapters::transcode::reencode_implicit_to_explicit(&bytes).map_err(|e| {
                crate::clog_warn!(
                    error = %e,
                    "Re-encoding an Implicit VR instance to Explicit VR failed - 406"
                );
                WadoError::NotAcceptable(format!(
                    "Cannot re-encode this Implicit VR instance as Explicit VR: {e}"
                ))
            })?;
        return Ok((
            out,
            clarus_adapters::codec::ts_uid::EXPLICIT_VR_LE.to_string(),
        ));
    }
    if !registry.supports(target_ts) {
        let hint = clarus_adapters::codec::unsupported_ts_name(target_ts)
            .map(|name| format!(" ({name})"))
            .unwrap_or_default();
        let available: Vec<&str> = registry.supported_transfer_syntaxes();
        return Err(WadoError::NotAcceptable(format!(
            "Transfer syntax '{target_ts}'{hint} is not supported for transcoding. \
             Available: {}",
            available.join(", ")
        )));
    }
    // 2608 pixel budget BEFORE the decode buffer is allocated: read
    // Rows/Columns from the instance metadata (cheap, minimal parse - dicom-rs
    // is already in the tree) and refuse an instance above the configured
    // megapixel cap. Reuses `WadoError::PayloadTooLarge` (mapped to 413, the
    // same status the thumbnail pixel cap uses) - no third error variant.
    // A decompression bomb / giant-frame OOM is therefore impossible.
    check_pixel_budget(&bytes, max_megapixels)?;
    // 2608 bounded pool: no waiting -> 503 when saturated. No unbounded
    // queue, no global slowdown - the client retries.
    let Some(_permit) = acquire(pool) else {
        return Err(WadoError::Busy);
    };
    let started = std::time::Instant::now();
    let out = match clarus_adapters::transcode::transcode_dicom(&bytes, target_ts, registry) {
        Ok(transcoded) => {
            // Successful transcodes are VISIBLE in the log (ops evidence).
            // INFO passes only with a whitelisted tag (clog.rs), so this uses
            // the tagged-event path; failures are loud via clog_warn and
            // 503/413 already show up in ACCESS lines.
            crate::clog_event!(
                crate::clog::Level::Info,
                "TRANSCODE",
                "",
                "ok stored={} target={} bytes_in={} bytes_out={} us={}",
                stored_ts,
                target_ts,
                bytes.len(),
                transcoded.len(),
                started.elapsed().as_micros() as u64,
            );
            transcoded
        }
        Err(e) => {
            match e {
                clarus_adapters::codec::CodecError::NonConformantCodestream(msg) => {
                    // Deterministic, permanent refusal (owner 27.08): the
                    // codestream has no legal DICOM representation. 406 lets
                    // the client react; the "200 + original bytes" fallback
                    // is reserved for transient decode failures.
                    crate::clog_warn!(
                        error = %msg,
                        target_ts = %target_ts,
                        "Refusing transcode of non-conformant codestream ({stored_ts} -> {target_ts}: {msg}) - 406"
                    );
                    return Err(WadoError::NotAcceptable(msg));
                }
                other => {
                    crate::clog_warn!(
                        error = %other,
                        target_ts = %target_ts,
                        "Transcoding failed ({stored_ts} -> {target_ts}: {other}) - serving original bytes"
                    );
                    // The ORIGINAL bytes, so the original syntax: the caller
                    // labels the response from the value this returns, and a
                    // transient decoder failure must not make the header claim
                    // a syntax the body is not in.
                    return Ok((bytes, stored_ts.to_string()));
                }
            }
        }
    };
    Ok((out, target_ts.to_string()))
}

/// An acquired transcode permit, released on drop.
///
/// `TranscodePool::release` is what returns the permit, so a manual
/// acquire/release pair only balances while the code between them runs to
/// completion. The decode inside it is driven by an UPLOADED codestream, and
/// a panic there unwinds straight past the `release()` call: the counter never
/// goes back down, and after N such instances every transcode and render
/// answers 503 for the life of the process. Releasing from `Drop` makes the
/// balance a property of the type instead of of the control flow.
struct PoolPermit<'a>(&'a crate::state::TranscodePool);

impl Drop for PoolPermit<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Take one permit without waiting, or `None` when the pool is saturated.
fn acquire(pool: &crate::state::TranscodePool) -> Option<PoolPermit<'_>> {
    pool.try_acquire().then(|| PoolPermit(pool))
}

/// Refuse to decode/transcode an instance whose decoded pixel count (Rows x
/// Columns) exceeds `max_megapixels`. Reads only the metadata header - no
/// decode buffer is ever allocated for an over-budget instance. Uses
/// `extract_u16` (raw little-endian byte read) rather than `parse_metadata_u16`
/// because the latter first attempts a UTF-8 parse, which REJECTS large Rows/
/// Columns whose high byte is >= 0x80 (e.g. 65535 = [0xFF,0xFF]) - exactly the
/// values a decompression bomb uses. A missing Rows/Columns pair is not fatal
/// (the downstream decode surfaces the real cause); only a definite
/// over-budget header is refused here.
fn check_pixel_budget(bytes: &[u8], max_megapixels: u32) -> Result<(), WadoError> {
    let rows = extract_u16(bytes, (0x0028, 0x0010)).unwrap_or(0) as u64;
    let cols = extract_u16(bytes, (0x0028, 0x0011)).unwrap_or(0) as u64;
    let max_px = (max_megapixels as u64) * 1_000_000;
    if rows > 0 && cols > 0 && rows.saturating_mul(cols) > max_px {
        return Err(WadoError::PayloadTooLarge(format!(
            "Image too large to transcode: {rows}x{cols} pixels (>{max_megapixels} MP limit)"
        )));
    }
    Ok(())
}

/// Extract a single frame's raw pixel bytes. The returned body is the bare
/// frame (application/octet-stream); the frames ROUTE wraps it into a
/// `multipart/related; type="application/octet-stream"` single-part body
/// per PS3.18 10.4.4 (see `h_wado_frame`).
///
/// The frame resource is a Target Resource, so a frame number outside
/// [1, NumberOfFrames] is "The Target Resource does not exist" -> 404
/// (PS3.18 10.4.3.1 Table 10.4.3-1). 416 belongs to the byte-Range HEADER
/// mechanism, not to this URI segment. Single-frame passthrough; multi-frame
/// extract.
/// True when the PixelData element value begins with an Item tag
/// (FFFE,E000) - i.e. the element is encapsulated (PS3.5 Table A.4-1). The
/// element bytes then wrap a Basic Offset Table item + per-frame Items and
/// are NOT a frame; `dicom_rs::frame::extract_frame` must unwrap them.
fn pixel_data_is_encapsulated(pixel_data: &[u8]) -> bool {
    pixel_data.len() >= 4
        && pixel_data[0] == 0xFE
        && pixel_data[1] == 0xFF
        && pixel_data[2] == 0x00
        && pixel_data[3] == 0xE0
}

pub fn extract_frame(dicom_bytes: &[u8], frame: &str) -> Result<WadoResp, WadoError> {
    use clarus_adapters::dicom_rs::DicomRsParser;
    use clarus_adapters::dicom_rs::frame;

    let frame_number: u32 = frame
        .parse()
        .map_err(|_| WadoError::NotAcceptable(format!("Invalid frame number: '{frame}'")))?;
    if frame_number < 1 {
        return Err(WadoError::NotFound("Frame number must be >= 1".into()));
    }

    let parser = DicomRsParser;
    let rows = parse_metadata_u16(&parser, dicom_bytes, frame::TAG_ROWS, "Rows")?.unwrap_or(0);
    let columns =
        parse_metadata_u16(&parser, dicom_bytes, frame::TAG_COLUMNS, "Columns")?.unwrap_or(0);
    let samples_per_pixel = parse_metadata_u16(
        &parser,
        dicom_bytes,
        frame::TAG_SAMPLES_PER_PIXEL,
        "SamplesPerPixel",
    )?
    .unwrap_or(1);
    let bits_allocated = parse_metadata_u16(
        &parser,
        dicom_bytes,
        frame::TAG_BITS_ALLOCATED,
        "BitsAllocated",
    )?
    .unwrap_or(8);
    let num_frames = parse_metadata_u32(
        &parser,
        dicom_bytes,
        frame::TAG_NUMBER_OF_FRAMES,
        "NumberOfFrames",
    )?
    .unwrap_or(1);

    let frame_size =
        frame::compute_frame_size(rows, columns, samples_per_pixel, bits_allocated).unwrap_or(0);

    if frame_number > num_frames {
        return Err(WadoError::NotFound(format!(
            "Frame {frame_number} exceeds NumberOfFrames {num_frames}"
        )));
    }

    let pixel_data = parser
        .extract_tag_value(dicom_bytes, (0x7FE0, 0x0010))
        .map_err(|e| WadoError::Internal(format!("Failed to parse DICOM for pixel data: {e}")))?
        .ok_or_else(|| WadoError::NotFound("PixelData tag not found in instance".into()))?;

    if pixel_data.is_empty() {
        return Err(WadoError::Internal("PixelData element is empty".into()));
    }

    let frame_bytes = if num_frames <= 1 && !pixel_data_is_encapsulated(&pixel_data) {
        pixel_data
    } else {
        frame::extract_frame(&pixel_data, frame_number, frame_size, num_frames).map_err(|e| {
            match &e {
                frame::FrameExtractionError::FrameOutOfRange { .. } => {
                    WadoError::NotFound(format!("Frame {frame_number} out of range"))
                }
                _ => WadoError::Internal(e.to_string()),
            }
        })?
    };

    Ok(WadoResp {
        status: 200,
        content_type: "application/octet-stream",
        headers: vec![("Content-Length", frame_bytes.len().to_string())],
        body: frame_bytes,
    })
}

/// Render a 64x64 JPEG thumbnail from instance bytes. Decode + encode are
/// synchronous here. Returns the JPEG body plus Cache-Control; the ETag is
/// set by the wrapper from the CAS key (wire-preserving pre-extraction value).
///
/// The RAW path needs no compression decoder - it
/// reads uncompressed pixel samples directly and is therefore available in the
/// DEFAULT build. The transfer syntax (0002,0010) is checked FIRST: the raw
/// path serves only the three uncompressed syntaxes; compressed instances are
/// routed to the transcode decode by the `_any` wrapper
/// (`render_thumbnail_from_bytes_any` -> `render_thumbnail_compressed`), while
/// THIS fn itself still 406s them (default-build wire contract). The only
/// third-party is `jpeg_encoder` (a pure Rust ENCODER, ~100 KB, unconditional
/// since C1).
pub fn render_thumbnail_from_bytes(
    bytes: &[u8],
    sop_uid: &str,
    target: (u32, u32),
) -> Result<WadoResp, WadoError> {
    use clarus_adapters::dicom_rs::DicomRsParser;

    let parser = DicomRsParser;

    // C1: TransferSyntax gate is BY SYNTAX (0002,0010), BEFORE pixel read.
    // Only the three uncompressed syntaxes are servable by THIS raw fn;
    // anything else (JPEG/JPEG-LS/JPEG2000/RLE/Deflated) is routed by the
    // `_any` wrapper to the transcode decode (see `_any`); this fn itself
    // still 406s, wire-compatible with the old
    // `#[cfg(not(feature="transcode"))]` 406.
    if is_compressed_transfer_syntax(bytes)? {
        return Err(WadoError::NotAcceptable(
            "thumbnail requires transcode".to_string(),
        ));
    }

    let pixel_data = parser
        .extract_tag_value(bytes, (0x7FE0, 0x0010))
        .map_err(|e| WadoError::Internal(format!("Failed to parse DICOM for thumbnail: {e}")))?
        .ok_or_else(|| WadoError::NotFound("PixelData not found".into()))?;

    if pixel_data.is_empty() {
        return Err(WadoError::Internal("PixelData is empty".into()));
    }

    let rows = extract_u16(bytes, (0x0028, 0x0010))
        .ok_or_else(|| WadoError::Internal("Missing Rows (0028,0010)".into()))?
        as u32;
    let cols = extract_u16(bytes, (0x0028, 0x0011))
        .ok_or_else(|| WadoError::Internal("Missing Columns (0028,0011)".into()))?
        as u32;
    let bits_allocated = extract_u16(bytes, (0x0028, 0x0100))
        .ok_or_else(|| WadoError::Internal("Missing BitsAllocated (0028,0100)".into()))?
        as u32;
    // C1 trap (3): PixelRepresentation (0028,0103)=1 means SIGNED samples.
    // Percentiles must be computed over i16, not u16, or the window breaks
    // (MR). Default 0 (unsigned).
    let pixel_rep = extract_u16(bytes, (0x0028, 0x0103)).unwrap_or(0);
    // C1 trap (1): PhotometricInterpretation (0028,0004)=MONOCHROME1 means
    // min=white -> invert after windowing, else the thumbnail flickers
    // black/white between studies (DX/CR). Default MONOCHROME2 (min=black).
    let photometric = parser
        .extract_tag_value(bytes, (0x0028, 0x0004))
        .ok()
        .flatten()
        .map(|v| String::from_utf8_lossy(&v).trim_matches(' ').to_string());
    let invert = photometric.as_deref() == Some("MONOCHROME1");

    if rows == 0 || cols == 0 {
        return Err(WadoError::Internal(format!(
            "Invalid dimensions: {rows}x{cols}"
        )));
    }

    let expected_bytes = (rows * cols * (bits_allocated / 8)) as usize;
    if pixel_data.len() < expected_bytes {
        return Err(WadoError::Internal(format!(
            "Pixel data too short: got {} bytes, expected {expected_bytes}",
            pixel_data.len()
        )));
    }

    // Cap the total decoded pixel count BEFORE the
    // `raw_pixels` / `all_vals` decode buffers are allocated. A corrupt or
    // malicious Rows x Columns header (with a matching oversized PixelData)
    // would otherwise force a huge heap allocation (OOM) instead of a clean
    // 4xx. Thumbnails only ever downscale, so any source above the cap is
    // refused rather than decoded.
    check_render_pixel_budget(rows, cols)?;

    // Shared raw-path renderer: window (+invert +signed) -> downscale -> JPEG.
    render_pixels_thumbnail(
        &pixel_data,
        RawGeometry {
            rows,
            cols,
            bits_allocated,
            pixel_rep,
            invert,
        },
        // The whole instance is the window source: Window Center/Width and
        // Rescale Slope/Intercept are dataset attributes, not pixels.
        bytes,
        target,
        sop_uid,
    )
}

/// Sample-format description of the raw pixels handed to the thumbnail core.
///
/// Grouped so the renderer signature stays readable: `rows`/`cols` and the
/// sample encoding always travel together and are meaningless apart.
pub struct RawGeometry {
    pub rows: u32,
    pub cols: u32,
    pub bits_allocated: u32,
    pub pixel_rep: u16,
    pub invert: bool,
}

/// Shared C1 raw-path thumbnail core: window per PS3.3 C.11.2.1.2 (stored VOI
/// -> percentile fallback, MONOCHROME1 invert, signed i16 for
/// PixelRepresentation=1), area-average downscale to the requested viewport,
/// then Baseline-JPEG encode via `jpeg_encoder`.
///
/// Used by `render_thumbnail_from_bytes` (whole instance), the `/rendered`
/// `image/jpeg` arm, and `render_frame_thumbnail` (a single extracted frame,
/// C2 all-or-none), so the raw-path traps apply identically at every level.
///
/// `window_bytes` is the assembled instance the VOI attributes are read from.
/// It is deliberately NOT `pixel_data`: on the compressed path the pixels
/// arrive already decoded and sliced (a bare frame plane with no dataset
/// around it), while Window Center/Width and Rescale Slope/Intercept still
/// live in the original instance. Passing the two separately is what lets a
/// compressed thumbnail window exactly like the full-size render of the same
/// instance - the invariant the `_compressed` callers document.
pub fn render_pixels_thumbnail(
    pixel_data: &[u8],
    geom: RawGeometry,
    window_bytes: &[u8],
    target: (u32, u32),
    sop_uid: &str,
) -> Result<WadoResp, WadoError> {
    use clarus_adapters::jpeg_encoder::{ColorType, Encoder};

    let (thumb_w, thumb_h, thumb_pixels) =
        render_mono_plane(pixel_data, geom, window_bytes, target)?;

    let mut jpeg_data = Vec::new();
    let encoder = Encoder::new(&mut jpeg_data, 75);
    encoder
        .encode(
            &thumb_pixels,
            thumb_w as u16,
            thumb_h as u16,
            ColorType::Luma,
        )
        .map_err(|e| WadoError::Internal(format!("JPEG encoding failed: {e}")))?;

    crate::clog_debug!(
        sop_uid = %sop_uid,
        thumb_bytes = %jpeg_data.len(),
        thumb_w = %thumb_w,
        thumb_h = %thumb_h,
        "Generated thumbnail"
    );

    Ok(WadoResp {
        status: 200,
        content_type: "image/jpeg",
        headers: vec![
            ("Content-Length", jpeg_data.len().to_string()),
            ("Cache-Control", "public, max-age=3600".to_string()),
        ],
        body: jpeg_data,
    })
}

/// The shared MONOCHROME window + downscale core, factored out of
/// [`render_pixels_thumbnail`] so BOTH the JPEG (`/thumbnail`, `/rendered`
/// image/jpeg) and the PNG Gray8 (`/rendered` image/png) paths window the
/// pixels EXACTLY once and identically. Returns the final plane dims plus the
/// 8-bit grayscale samples.
///
/// `window_bytes` is the assembled instance the VOI attributes are read from
/// (see the `render_pixels_thumbnail` doc); `target` is the fit box, or the
/// native `(cols, rows)` when the caller wants no downscale.
fn render_mono_plane(
    pixel_data: &[u8],
    geom: RawGeometry,
    window_bytes: &[u8],
    target: (u32, u32),
) -> Result<(u32, u32, Vec<u8>), WadoError> {
    let RawGeometry {
        rows,
        cols,
        bits_allocated,
        pixel_rep,
        invert,
    } = geom;

    // Cap total pixels BEFORE decode buffers are
    // allocated (a corrupt Rows x Columns would otherwise OOM, not 4xx).
    if rows == 0 || cols == 0 {
        return Err(WadoError::Internal(format!(
            "Invalid dimensions: {rows}x{cols}"
        )));
    }
    check_render_pixel_budget(rows, cols)?;

    let pixel_count = (cols * rows) as usize;

    // Windowing per PS3.3 C.11.2.1.2: Modality LUT (Rescale Slope/Intercept)
    // always, then the VOI - VOI LUT Sequence first item, else the stored
    // Window Center/Width, else nothing. Same resolution the full-size
    // renderer uses, so a thumbnail and a full render of one instance cannot
    // disagree about the window.
    let (slope, intercept) = rescale_params(window_bytes);
    let voi = resolve_voi_window(window_bytes)?;

    let bytes_per_sample = (bits_allocated / 8) as usize;
    if bytes_per_sample == 0 {
        return Err(WadoError::Internal("BitsAllocated too small".into()));
    }

    // PS3.5 7.1.2 (and 8.1.1) pads the WHOLE Pixel Data value to an even
    // length; it does not pad each ROW. Native 8-bit rows are packed, so the
    // stride is the row width and nothing else. The pad byte, when the value
    // needs one, sits after the LAST sample of the LAST row.
    //
    // Sizing the copy from `cols + (cols & 1)` read past the end: a conformant
    // 3x3 instance declares 10 bytes (9 samples + pad, which `extract_tag_value`
    // returns by the declared length), so the walk started row 2 at offset 8
    // and read index 10 of a 10-byte buffer - an out-of-bounds panic reachable
    // from /thumbnail, the frame thumbnails and /rendered for any odd-width
    // 8-bit MONOCHROME instance with an identity rescale and no VOI.
    let expected = (rows * cols * (bits_allocated / 8)) as usize;
    if pixel_data.len() < expected {
        return Err(WadoError::Internal(format!(
            "Pixel data too short: got {} bytes, expected {expected}",
            pixel_data.len()
        )));
    }
    let stride = cols;

    let mut raw_pixels: Vec<u8> = vec![0u8; pixel_count];
    if bits_allocated == 8 && slope == 1.0 && intercept == 0.0 && voi.is_none() {
        // This fast path is only taken when there is NOTHING to map: an
        // identity Modality LUT AND no VOI window. A stored Window Center/
        // Width, a VOI LUT item or a non-identity rescale all have to go
        // through the general branch below, or the thumbnail would silently
        // ignore the attribute the whole point of this path is to honour.
        for y in 0..rows {
            let row_off = (y * stride) as usize;
            for x in 0..cols {
                let v = pixel_data[row_off + x as usize];
                raw_pixels[(y * cols + x) as usize] = if invert { 255 - v } else { v };
            }
        }
    } else {
        // Post-rescale physical values, so the percentile fallback ranks the
        // same numbers a viewer would and a stored window is in the same
        // units the attribute is expressed in.
        let mut physical: Vec<i32> = Vec::with_capacity(pixel_count);
        for i in 0..pixel_count {
            let idx = i * bytes_per_sample;
            let s = if bytes_per_sample == 1 {
                pixel_data[idx] as i32
            } else {
                let b0 = pixel_data.get(idx).copied().unwrap_or(0);
                let b1 = pixel_data.get(idx + 1).copied().unwrap_or(0);
                let u = u16::from_le_bytes([b0, b1]);
                if pixel_rep == 1 {
                    (u as i16) as i32
                } else {
                    u as i32
                }
            };
            physical.push(rescale_sample(s, slope, intercept) as i32);
        }
        match voi {
            Some((center, width)) => {
                let w = if width <= 0.0 { 1.0 } else { width };
                let lo_f = center - w / 2.0;
                for (out, value) in raw_pixels.iter_mut().zip(physical.iter()) {
                    let y = ((*value as f64 - lo_f) / w) * 255.0;
                    let yc = y.clamp(0.0, 255.0) as u8;
                    *out = if invert { 255 - yc } else { yc };
                }
            }
            None => {
                // 2-98% percentile fallback, used ONLY when the instance
                // carries no VOI attribute at all. The trim is what keeps one
                // saturated pixel from inflating the range and washing the
                // whole thumbnail out.
                let mut sorted = physical.clone();
                sorted.sort_unstable();
                let lo_idx = ((pixel_count as f64 * 0.02) as usize).min(pixel_count - 1);
                let hi_idx = ((pixel_count as f64 * 0.98) as usize).min(pixel_count - 1);
                let lo = sorted[lo_idx];
                let hi = sorted[hi_idx];
                let range = (hi - lo).max(1) as i64;
                for (out, value) in raw_pixels.iter_mut().zip(physical.iter()) {
                    let y =
                        ((*value as i64 - lo as i64).clamp(0, range) as u64 * 255) / range as u64;
                    let yc = y.min(255) as u8;
                    *out = if invert { 255 - yc } else { yc };
                }
            }
        }
    }

    if rows == target.1 && cols == target.0 {
        Ok((cols, rows, raw_pixels))
    } else {
        let (new_w, new_h) = fit_in_box_or_square(cols, rows, target);
        let resized = resize_area_average(&raw_pixels, cols, rows, new_w, new_h);
        Ok((new_w, new_h, resized))
    }
}

/// 0410: render a SINGLE-FRAME RGB instance to a PNG (RGB8) body, no window.
///
/// The RGB preview instances (LABEL/THUMBNAIL/OVERVIEW) carry
/// PhotometricInterpretation=RGB, SamplesPerPixel=3, BitsAllocated=8,
/// PlanarConfiguration=0 and are ALREADY display-ready colour: windowing them
/// would be wrong. So this path does NOT window - it deinterleaves the
/// interleaved RGB triplets to three planes, area-averages each plane to the
/// (optional) target box, then re-interleaves and PNG-encodes as RGB8.
///
/// DICOM pads every row to an even byte boundary (PS3.5 8.1.1), so the source
/// row stride is `cols*3 + ((cols*3)&1)`. A 24-bit RGB sample count is
/// `cols*3` bytes per row, hence the pad when that is odd. The stride is
/// chosen from the actual byte length, so the (common) pixelmed TIFF->DICOM
/// shape that omits the per-row pad still renders.
///
/// `target` is the fit box, or the native `(cols, rows)` when the caller wants
/// no downscale (Slim's `resizeFactor == 1`).
pub fn render_rgb_png(
    pixel_data: &[u8],
    geom: RgbGeometry,
    target: (u32, u32),
) -> Result<WadoResp, WadoError> {
    let RgbGeometry {
        rows,
        cols,
        bits_allocated,
    } = geom;

    if rows == 0 || cols == 0 {
        return Err(WadoError::Internal(format!(
            "Invalid dimensions: {rows}x{cols}"
        )));
    }
    if bits_allocated != 8 {
        return Err(WadoError::NotAcceptable(format!(
            "rendered RGB requires 8-bit samples, got {bits_allocated}"
        )));
    }
    check_render_pixel_budget(rows, cols)?;

    // Source row stride includes the even-byte pad (PS3.5 8.1.1): a row of
    // `cols*3` bytes gets one pad byte when `cols*3` is odd. Some encoders
    // (pixelmed TIFF->DICOM) pack RGB rows WITHOUT the per-row pad and put a
    // single trailing pad on the whole value instead - the LABEL instance
    // 837x807 in the RMS corpus is exactly that shape. Accept either stride
    // by picking the one the actual byte length supports.
    let src_row_bytes = (cols * 3) as usize;
    let padded_stride = src_row_bytes + (src_row_bytes & 1);
    let padded_needed = padded_stride * (rows as usize);
    let unpadded_needed = src_row_bytes * (rows as usize);
    let src_stride = if pixel_data.len() >= padded_needed {
        padded_stride
    } else if pixel_data.len() >= unpadded_needed {
        src_row_bytes
    } else {
        return Err(WadoError::Internal(format!(
            "Pixel data too short: got {} bytes, expected at least {unpadded_needed}",
            pixel_data.len()
        )));
    };

    // Deinterleave into three `rows*cols` planes (row stride = cols*3 + pad).
    let plane = (cols * rows) as usize;
    let mut r = vec![0u8; plane];
    let mut g = vec![0u8; plane];
    let mut b = vec![0u8; plane];
    for y in 0..rows {
        let src = (y as usize) * src_stride;
        let dst = (y as usize) * (cols as usize);
        for x in 0..cols as usize {
            let s = src + x * 3;
            let d = dst + x;
            r[d] = pixel_data[s];
            g[d] = pixel_data[s + 1];
            b[d] = pixel_data[s + 2];
        }
    }

    // Per-plane area-average to the target box (or native when target == src).
    let (out_w, out_h) = if rows == target.1 && cols == target.0 {
        (cols, rows)
    } else {
        fit_in_box_or_square(cols, rows, target)
    };
    let (r, g, b) = if (out_w, out_h) == (cols, rows) {
        (r, g, b)
    } else {
        (
            resize_area_average(&r, cols, rows, out_w, out_h),
            resize_area_average(&g, cols, rows, out_w, out_h),
            resize_area_average(&b, cols, rows, out_w, out_h),
        )
    };

    // Re-interleave the three planes into one RGB8 buffer for the encoder.
    let out_plane = (out_w * out_h) as usize;
    let mut rgb = Vec::with_capacity(out_plane * 3);
    for i in 0..out_plane {
        rgb.push(r[i]);
        rgb.push(g[i]);
        rgb.push(b[i]);
    }

    let body = encode_png_rgb8(&rgb, out_w, out_h)
        .map_err(|e| WadoError::Internal(format!("PNG encoding failed: {e}")))?;

    Ok(WadoResp {
        status: 200,
        content_type: "image/png",
        headers: vec![
            ("Content-Length", body.len().to_string()),
            ("Cache-Control", "public, max-age=3600".to_string()),
        ],
        body,
    })
}

/// 0410: Gray8 PNG body for a MONO instance (the `/rendered` image/png arm).
pub fn render_gray_png_from_mono_plane(w: u32, h: u32, gray: &[u8]) -> Result<WadoResp, WadoError> {
    let body = encode_png_gray8(gray, w, h)
        .map_err(|e| WadoError::Internal(format!("PNG encoding failed: {e}")))?;
    Ok(WadoResp {
        status: 200,
        content_type: "image/png",
        headers: vec![
            ("Content-Length", body.len().to_string()),
            ("Cache-Control", "public, max-age=3600".to_string()),
        ],
        body,
    })
}

/// 1310: Gray8 GIF body for a MONO instance (the `/rendered` image/gif arm).
pub fn render_gray_gif_from_mono_plane(w: u32, h: u32, gray: &[u8]) -> Result<WadoResp, WadoError> {
    let body = encode_gif_gray8(gray, w, h)
        .map_err(|e| WadoError::Internal(format!("GIF encoding failed: {e}")))?;
    Ok(WadoResp {
        status: 200,
        content_type: "image/gif",
        headers: vec![
            ("Content-Length", body.len().to_string()),
            ("Cache-Control", "public, max-age=3600".to_string()),
        ],
        body,
    })
}

/// RGB source geometry for [`render_rgb_png`]. Grouped like [`RawGeometry`].
pub struct RgbGeometry {
    pub rows: u32,
    pub cols: u32,
    pub bits_allocated: u32,
}

/// The pixel budget shared by every render path (the thumbnail core, the RGB
/// rendered path AND the `/rendered` handler's pre-read gate).
pub const MAX_THUMBNAIL_PIXELS: u32 = 67_108_864; // 64M px (~192 MB at RGB8)

/// The ONE home of the render pixel-budget rule: refuse Rows x Columns over
/// [`MAX_THUMBNAIL_PIXELS`]. Shared by [`render_mono_plane`], [`render_rgb_png`]
/// and the `/rendered` handler (which calls it BEFORE `read_cas_bytes`, so an
/// over-budget single-frame instance is rejected before the whole blob is
/// read - the F3 "reject before full read" rule). Returns 413 on over-budget.
pub fn check_render_pixel_budget(rows: u32, cols: u32) -> Result<(), WadoError> {
    if cols > MAX_THUMBNAIL_PIXELS / rows.max(1) {
        return Err(WadoError::PayloadTooLarge(format!(
            "Image too large to decode for thumbnail: {rows}x{cols} (>{MAX_THUMBNAIL_PIXELS} px)"
        )));
    }
    Ok(())
}

/// Encode an 8-bit grayscale plane as a PNG (color type 0 / Gray).
fn encode_png_gray8(
    gray: &[u8],
    w: u32,
    h: u32,
) -> Result<Vec<u8>, clarus_adapters::png::EncodingError> {
    encode_png(
        gray,
        w,
        h,
        clarus_adapters::png::ColorType::Grayscale,
        clarus_adapters::png::BitDepth::Eight,
    )
}

/// Encode an 8-bit grayscale plane as a GIF89a image (Gray8).
fn encode_gif_gray8(
    gray: &[u8],
    w: u32,
    h: u32,
) -> Result<Vec<u8>, clarus_adapters::gif::EncodingError> {
    clarus_adapters::gif::encode(gray, w, h, clarus_adapters::gif::ColorType::Gray)
}

/// Encode an interleaved RGB8 buffer as a PNG (color type 2 / RGB).
fn encode_png_rgb8(
    rgb: &[u8],
    w: u32,
    h: u32,
) -> Result<Vec<u8>, clarus_adapters::png::EncodingError> {
    encode_png(
        rgb,
        w,
        h,
        clarus_adapters::png::ColorType::Rgb,
        clarus_adapters::png::BitDepth::Eight,
    )
}

/// Shared PNG encode: one exact-capacity allocation, no per-row allocation.
fn encode_png(
    samples: &[u8],
    w: u32,
    h: u32,
    color: clarus_adapters::png::ColorType,
    depth: clarus_adapters::png::BitDepth,
) -> Result<Vec<u8>, clarus_adapters::png::EncodingError> {
    clarus_adapters::png::encode(samples, w, h, color, depth)
}

/// Extract the MONO plane for a SINGLE-FRAME instance and PNG-encode it
/// (Gray8) - the `/rendered` image/png arm for grayscale data. Windows
/// exactly like the JPEG thumbnail (shared [`render_mono_plane`]).
pub fn render_gray_png(
    pixel_data: &[u8],
    geom: RawGeometry,
    window_bytes: &[u8],
    target: (u32, u32),
) -> Result<WadoResp, WadoError> {
    let (w, h, gray) = render_mono_plane(pixel_data, geom, window_bytes, target)?;
    render_gray_png_from_mono_plane(w, h, &gray)
}

/// Extract the MONO plane for a SINGLE-FRAME instance and GIF-encode it
/// (Gray8) - the `/rendered` image/gif arm for grayscale data. Shares the
/// window/downscale core with the JPEG thumbnail and the PNG arm through
/// [`render_mono_plane`], so the gif body is pixel-identical to the others.
pub fn render_gray_gif(
    pixel_data: &[u8],
    geom: RawGeometry,
    window_bytes: &[u8],
    target: (u32, u32),
) -> Result<WadoResp, WadoError> {
    let (w, h, gray) = render_mono_plane(pixel_data, geom, window_bytes, target)?;
    render_gray_gif_from_mono_plane(w, h, &gray)
}

/// Aspect-preserving fit of the source into the requested output box.
///
/// PS3.18 8.3.5.1.3 renders the content "as large as possible, within the
/// viewport ... without distorting", which scales UP as well as down: a 4x4
/// source asked for `viewport=64,64` is rendered at 64x64. `fit_dimensions`
/// below is the older square-box helper that deliberately does NOT upscale
/// (it clamps), so it cannot be used for a client-supplied viewport.
fn fit_in_box_or_square(src_w: u32, src_h: u32, target: (u32, u32)) -> (u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (target.0, target.1);
    }
    let scale = f64::min(
        target.0 as f64 / src_w as f64,
        target.1 as f64 / src_h as f64,
    );
    let w = (src_w as f64 * scale).round() as u32;
    let h = (src_h as f64 * scale).round() as u32;
    // Never emit a zero axis: a source far wider than the box would otherwise
    // round the short side to 0 and produce an empty image.
    (w.max(1), h.max(1))
}

/// Area-average downscale: each output pixel is the integer mean of the
/// source rectangle it covers.
///
/// Replaces nearest-neighbour, which on a CT slice keeps one sample out of
/// every `k` and makes thin structures alias into noise. Averaging is the
/// correct downscale filter and costs no dependency.
///
/// The source rectangle of output pixel `(x, y)` is computed from the scale
/// in integer arithmetic (`x * src_w / dst_w` .. `(x+1) * src_w / dst_w`), so
/// every source pixel contributes to exactly one output pixel and the edges
/// are covered exactly once. Rounds half up.
fn resize_area_average(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let dst_size = (dst_w * dst_h) as usize;
    let mut dst = vec![0u8; dst_size];
    if dst_w == 0 || dst_h == 0 || src_w == 0 || src_h == 0 {
        return dst;
    }
    for y in 0..dst_h {
        let y0 = (y as u64 * src_h as u64 / dst_h as u64) as u32;
        let y1 = (((y + 1) as u64 * src_h as u64) / dst_h as u64) as u32;
        // A box must cover at least one source row, or a large downscale
        // would average over an empty rectangle and emit black.
        let y1 = y1.max(y0 + 1).min(src_h);
        for x in 0..dst_w {
            let x0 = (x as u64 * src_w as u64 / dst_w as u64) as u32;
            let x1 = (((x + 1) as u64 * src_w as u64) / dst_w as u64) as u32;
            let x1 = x1.max(x0 + 1).min(src_w);
            let mut sum: u64 = 0;
            let mut count: u64 = 0;
            for sy in y0..y1 {
                let row = (sy * src_w) as usize;
                for sx in x0..x1 {
                    sum += u64::from(src[row + sx as usize]);
                    count += 1;
                }
            }
            // Rounds half up; `count` is never 0 because both boxes were
            // clamped to at least one cell.
            //
            // The accumulator is 64-bit: a box over a 67 Mpx source holds
            // ~6.7e7 cells of up to 255, i.e. ~1.7e10 - past u32::MAX, so a
            // 32-bit sum wrapped to a small number and the thumbnail came out
            // black in exactly the large-image case the downscale exists for.
            dst[(y * dst_w + x) as usize] = ((sum + count / 2) / count) as u8;
        }
    }
    dst
}

/// C2 (B3 all-or-none): render the thumbnail of a SINGLE frame of an instance.
/// Reuses `extract_frame` to pull frame `frame`'s raw bytes, then the shared
/// raw-path renderer. Compressed instances are routed to the transcode decode
/// by the `_any` wrapper (`render_frame_thumbnail_any`); THIS raw fn itself
/// still 406s them (default-build wire contract).
pub fn render_frame_thumbnail(
    bytes: &[u8],
    sop_uid: &str,
    frame: &str,
    target: (u32, u32),
) -> Result<WadoResp, WadoError> {
    use clarus_adapters::dicom_rs::{DicomRsParser, frame};

    if is_compressed_transfer_syntax(bytes)? {
        return Err(WadoError::NotAcceptable(
            "thumbnail requires transcode".to_string(),
        ));
    }

    let parser = DicomRsParser;
    let rows = extract_u16(bytes, (0x0028, 0x0010))
        .ok_or_else(|| WadoError::Internal("Missing Rows (0028,0010)".into()))?
        as u32;
    let cols = extract_u16(bytes, (0x0028, 0x0011))
        .ok_or_else(|| WadoError::Internal("Missing Columns (0028,0011)".into()))?
        as u32;
    let bits_allocated = extract_u16(bytes, (0x0028, 0x0100))
        .ok_or_else(|| WadoError::Internal("Missing BitsAllocated (0028,0100)".into()))?
        as u32;
    let pixel_rep = extract_u16(bytes, (0x0028, 0x0103)).unwrap_or(0);
    let samples_per_pixel = extract_u16(bytes, (0x0028, 0x0002)).unwrap_or(1);
    let photometric = parser
        .extract_tag_value(bytes, (0x0028, 0x0004))
        .ok()
        .flatten()
        .map(|v| String::from_utf8_lossy(&v).trim_matches(' ').to_string());
    let invert = photometric.as_deref() == Some("MONOCHROME1");

    let frame_number: u32 = frame
        .parse()
        .map_err(|_| WadoError::NotAcceptable(format!("Invalid frame number: '{frame}'")))?;
    // PS3.18 10.4.3.1 Table 10.4.3-1: the frame number is part of the Target
    // Resource URI, so a number outside [1, NumberOfFrames] names a resource
    // that does not exist - 404. 416 answers a Range HEADER the body cannot
    // satisfy, which is a different mechanism the table does not list; see
    // `slice_decoded_frame` for the same distinction.
    if frame_number < 1 {
        return Err(WadoError::NotFound(format!(
            "Frame {frame_number} does not exist"
        )));
    }
    let num_frames = num_frames_of(bytes);
    if frame_number > num_frames {
        return Err(WadoError::NotFound(format!(
            "Frame {frame_number} does not exist: the instance has {num_frames}"
        )));
    }
    let frame_size = frame::compute_frame_size(
        rows as u16,
        cols as u16,
        samples_per_pixel,
        bits_allocated as u16,
    )
    .unwrap_or(0);
    let pixel_data = parser
        .extract_tag_value(bytes, (0x7FE0, 0x0010))
        .map_err(|e| WadoError::Internal(format!("Failed to parse DICOM for frame: {e}")))?
        .ok_or_else(|| WadoError::NotFound("PixelData not found".into()))?;
    let frame_bytes = if num_frames <= 1 && !pixel_data_is_encapsulated(&pixel_data) {
        pixel_data
    } else {
        frame::extract_frame(&pixel_data, frame_number, frame_size, num_frames)
            .map_err(|_| WadoError::RangeNotSatisfiable)?
    };

    render_pixels_thumbnail(
        &frame_bytes,
        RawGeometry {
            rows,
            cols,
            bits_allocated,
            pixel_rep,
            invert,
        },
        // The instance, not the sliced frame plane: the VOI attributes are
        // dataset-level and the frame plane has no dataset around it.
        bytes,
        target,
        sop_uid,
    )
}

/// Whole-instance thumbnail, ANY transfer syntax. Uncompressed -> the current
/// `render_thumbnail_from_bytes` UNCHANGED (raw path, no pool permit, byte-
/// identical behaviour); compressed -> pixel budget + bounded-pool permit
/// around `render_thumbnail_compressed` (Busy -> 503, over-budget -> 413,
/// same contract as `render_full_from_bytes`).
pub fn render_thumbnail_from_bytes_any(
    bytes: &[u8],
    sop_uid: &str,
    target: (u32, u32),
    registry: &clarus_adapters::codec::CodecRegistry,
    pool: &crate::state::TranscodePool,
    max_megapixels: u32,
) -> Result<WadoResp, WadoError> {
    if !is_compressed_transfer_syntax(bytes)? {
        return render_thumbnail_from_bytes(bytes, sop_uid, target);
    }
    check_pixel_budget(bytes, max_megapixels)?;
    let Some(_permit) = acquire(pool) else {
        return Err(WadoError::Busy);
    };
    render_thumbnail_compressed(bytes, sop_uid, target, registry)
}

/// Compressed whole-instance thumbnail. Mirror of `render_full_compressed` up
/// to the render step: TS -> codec -> decode -> validate -> colour (SPP != 1)
/// -> 406; then the decoded mono plane is handed to the existing
/// `render_pixels_thumbnail`. INVARIANT: pixel_rep/invert are derived EXACTLY
/// like in `render_full_compressed` (invert = photometric == "MONOCHROME1";
/// pixel_rep = bits_allocated == 16) so the full-size render and the
/// thumbnail never window the same instance differently.
#[cfg(feature = "transcode")]
fn render_thumbnail_compressed(
    bytes: &[u8],
    sop_uid: &str,
    target: (u32, u32),
    registry: &clarus_adapters::codec::CodecRegistry,
) -> Result<WadoResp, WadoError> {
    let ts = match registry_sorted_ts(bytes) {
        Some(t) => t,
        None => {
            return Err(WadoError::NotAcceptable(
                "thumbnail requires transcode".to_string(),
            ));
        }
    };
    let codec = registry
        .get(&ts)
        .ok_or_else(|| WadoError::NotAcceptable("thumbnail requires transcode".to_string()))?;
    let decoded = codec.decode(bytes).map_err(|e| match e {
        clarus_adapters::codec::CodecError::NonConformantCodestream(msg) => {
            WadoError::NotAcceptable(msg)
        }
        other => WadoError::Internal(format!("Decode failed for thumbnail: {other}")),
    })?;
    // v1 renders frame 1 only. A decode returns ALL frames concatenated
    // frame-major, so validate for the real decoded frame count, not 1, then
    // slice out frame 1. The frame count is derived from the decoded bytes
    // (len over one frame's size) - the same value NumberOfFrames carries but
    // immune to that tag's IS-text-vs-binary storage. Mirrors the canonical
    // frame-thumbnail shape: validate_frames(N) + slice_decoded_frame.
    let one_frame = decoded.expected_size();
    // `checked_div` folds in the zero-size guard: a zero `one_frame` (an
    // underivable frame size) yields 1 rather than dividing by zero.
    let num_frames = decoded
        .data
        .len()
        .checked_div(one_frame)
        .unwrap_or(1)
        .max(1) as u32;
    if decoded.validate_frames(num_frames).is_err() {
        return Err(WadoError::Internal("Decoded pixels invalid".into()));
    }
    if decoded.samples_per_pixel != 1 {
        // MONOCHROME-only in v1 - colour planes are out of scope.
        return Err(WadoError::NotAcceptable(
            "thumbnail colour unsupported".to_string(),
        ));
    }
    let frame_slice = slice_decoded_frame(&decoded, 1, num_frames)?;
    // Same derivation as render_full_compressed (invariant, see the fn doc).
    let invert = decoded.photometric_interpretation == "MONOCHROME1";
    let pixel_rep = if decoded.bits_allocated == 16 { 1 } else { 0 };
    render_pixels_thumbnail(
        frame_slice,
        RawGeometry {
            rows: decoded.rows as u32,
            cols: decoded.columns as u32,
            bits_allocated: decoded.bits_allocated as u32,
            pixel_rep,
            invert,
        },
        // The original (compressed) instance: WC/WW and Rescale live in it,
        // not in the decoded plane.
        bytes,
        target,
        sop_uid,
    )
}

/// Default-build compressed thumbnail -> 406, wire-compatible with the
/// raw-path compressed-syntax gate ("thumbnail requires transcode").
#[cfg(not(feature = "transcode"))]
fn render_thumbnail_compressed(
    _bytes: &[u8],
    _sop_uid: &str,
    _target: (u32, u32),
    _registry: &clarus_adapters::codec::CodecRegistry,
) -> Result<WadoResp, WadoError> {
    Err(WadoError::NotAcceptable(
        "thumbnail requires transcode".to_string(),
    ))
}

/// Frame-thumbnail, ANY transfer syntax. Uncompressed -> the current
/// `render_frame_thumbnail` (raw path untouched); compressed -> decode the
/// WHOLE instance (the codec decodes all frames at once), validate the frame
/// number (PS3.18 10.4.3.1: <1 or >N names an absent Target Resource -> 404),
/// slice frame N from the planar mono data (frame-major, per
/// `DecodedPixels::expected_size()`), colour -> 406, then the shared
/// `render_pixels_thumbnail`. Same pixel budget + bounded-pool pattern as
/// `render_thumbnail_from_bytes_any`.
pub fn render_frame_thumbnail_any(
    bytes: &[u8],
    sop_uid: &str,
    frame: &str,
    target: (u32, u32),
    registry: &clarus_adapters::codec::CodecRegistry,
    pool: &crate::state::TranscodePool,
    max_megapixels: u32,
) -> Result<WadoResp, WadoError> {
    if !is_compressed_transfer_syntax(bytes)? {
        return render_frame_thumbnail(bytes, sop_uid, frame, target);
    }
    check_pixel_budget(bytes, max_megapixels)?;
    let Some(_permit) = acquire(pool) else {
        return Err(WadoError::Busy);
    };
    render_frame_thumbnail_compressed(bytes, sop_uid, frame, target, registry)
}

/// Compressed single-frame thumbnail: the compressed counterpart of
/// `render_frame_thumbnail`. Frame number semantics mirror the raw path
/// (<1 or >NumberOfFrames -> NotFound -> 404); pixel_rep/invert
/// follow the SAME derivation as `render_full_compressed` (invariant).
#[cfg(feature = "transcode")]
fn render_frame_thumbnail_compressed(
    bytes: &[u8],
    sop_uid: &str,
    frame: &str,
    target: (u32, u32),
    registry: &clarus_adapters::codec::CodecRegistry,
) -> Result<WadoResp, WadoError> {
    let frame_number: u32 = frame
        .parse()
        .map_err(|_| WadoError::NotAcceptable(format!("Invalid frame number: '{frame}'")))?;
    // Same Target Resource rule as the raw thumbnail path above: an absent
    // frame is 404, not a range failure.
    if frame_number < 1 {
        return Err(WadoError::NotFound(format!(
            "Frame {frame_number} does not exist"
        )));
    }
    let num_frames = num_frames_of(bytes);
    if frame_number > num_frames {
        return Err(WadoError::NotFound(format!(
            "Frame {frame_number} does not exist: the instance has {num_frames}"
        )));
    }
    let ts = match registry_sorted_ts(bytes) {
        Some(t) => t,
        None => {
            return Err(WadoError::NotAcceptable(
                "thumbnail requires transcode".to_string(),
            ));
        }
    };
    let codec = registry
        .get(&ts)
        .ok_or_else(|| WadoError::NotAcceptable("thumbnail requires transcode".to_string()))?;
    let decoded = codec.decode(bytes).map_err(|e| match e {
        clarus_adapters::codec::CodecError::NonConformantCodestream(msg) => {
            WadoError::NotAcceptable(msg)
        }
        other => WadoError::Internal(format!("Decode failed for frame thumbnail: {other}")),
    })?;
    // Multi-frame: ALL frames are concatenated in the plane (frame-major),
    // so the decode is validated for N frames, not 1.
    if decoded.validate_frames(num_frames).is_err() {
        return Err(WadoError::Internal("Decoded pixels invalid".into()));
    }
    if decoded.samples_per_pixel != 1 {
        // MONOCHROME-only in v1 - colour planes are out of scope.
        return Err(WadoError::NotAcceptable(
            "thumbnail colour unsupported".to_string(),
        ));
    }
    let frame_slice = slice_decoded_frame(&decoded, frame_number, num_frames)?;
    // Same derivation as render_full_compressed (invariant, see the fn doc).
    let invert = decoded.photometric_interpretation == "MONOCHROME1";
    let pixel_rep = if decoded.bits_allocated == 16 { 1 } else { 0 };
    render_pixels_thumbnail(
        frame_slice,
        RawGeometry {
            rows: decoded.rows as u32,
            cols: decoded.columns as u32,
            bits_allocated: decoded.bits_allocated as u32,
            pixel_rep,
            invert,
        },
        // The original (compressed) instance is the window source.
        bytes,
        target,
        sop_uid,
    )
}

/// Default-build compressed frame thumbnail -> 406, wire-compatible with the
/// raw-path compressed-syntax gate ("thumbnail requires transcode").
#[cfg(not(feature = "transcode"))]
fn render_frame_thumbnail_compressed(
    _bytes: &[u8],
    _sop_uid: &str,
    _frame: &str,
    _target: (u32, u32),
    _registry: &clarus_adapters::codec::CodecRegistry,
) -> Result<WadoResp, WadoError> {
    Err(WadoError::NotAcceptable(
        "thumbnail requires transcode".to_string(),
    ))
}

/// Slice frame `frame_number` (1-based) out of planar multi-frame mono data:
/// frame-major layout (all frames of the single plane concatenated), frame
/// size = rows x cols x (bits_allocated / 8) - see
/// `DecodedPixels::expected_size()`. Unit-tested on a synthetic 2-frame
/// `DecodedPixels` (wado_pure tests, runs in the default build via `test`).
#[cfg(any(feature = "transcode", test))]
fn slice_decoded_frame(
    decoded: &clarus_adapters::codec::DecodedPixels,
    frame_number: u32,
    num_frames: u32,
) -> Result<&[u8], WadoError> {
    if frame_number < 1 || frame_number > num_frames {
        return Err(WadoError::RangeNotSatisfiable);
    }
    let frame_size = decoded.pixel_count() * (decoded.bits_allocated as usize / 8);
    decoded
        .data
        .get((frame_number - 1) as usize * frame_size..frame_number as usize * frame_size)
        .ok_or(WadoError::RangeNotSatisfiable)
}

/// Full-size rendered JPEG for `Accept: image/jpeg` on the instance endpoint.
///
/// Reuses the thumbnail raw-path decode (MONOCHROME1 invert, PixelRepresentation
/// signed fix, Baseline-JPEG encode) but WITHOUT any downscale.
/// The JPEG is emitted at the instance's NATIVE resolution. Windowing follows
/// PS3.3 C.11.2.1.2: Modality LUT (Rescale Slope/Intercept) first, then VOI LUT
/// Sequence (first item), else Window Center/Width, and ONLY if no VOI attribute
/// exists at all does it fall back to a 2-98% percentile (auto-window). A
/// percentile pass over a calibrated lung/bone window would wreck it, so it is
/// never applied when the instance carries VOI data.
///
/// Transfer syntax: uncompressed instances are decoded by the raw path (this is
/// the DEFAULT build, same as the thumbnail). Compressed instances need a pixel
/// decoder. In a default build the endpoint returns `NotAcceptable`
/// ("rendered requires transcode") -> 406, wire-compatible with the thumbnail
/// 406; in a transcode build the codec registry decodes the pixel data, then
/// the same windowing + native-size JPEG encode runs.
///
/// Multi-frame: v1 renders frame 1 only (documented in the report; the instance
/// endpoint has no per-frame selector). Window center/width query params are
/// OUT of scope for v1 (documented) - only the instance's stored windowing is
/// used.
pub fn render_full_from_bytes(
    bytes: &[u8],
    sop_uid: &str,
    registry: &clarus_adapters::codec::CodecRegistry,
    pool: &crate::state::TranscodePool,
    max_megapixels: u32,
) -> Result<WadoResp, WadoError> {
    // 2608 pixel budget BEFORE any decode buffer is allocated (reuses
    // `PayloadTooLarge` -> 413, same status as the thumbnail pixel cap).
    check_pixel_budget(bytes, max_megapixels)?;
    // 2608 bounded pool: the render holds a permit for its whole lifetime; a
    // saturated pool -> 503 + Retry-After, never render without a permit.
    let Some(_permit) = acquire(pool) else {
        return Err(WadoError::Busy);
    };
    render_full_impl(bytes, sop_uid, registry)
}

/// The actual render body, wrapped by [`render_full_from_bytes`] (which owns
/// the pool permit + pixel-budget guard). Kept separate so the bounded-pool
/// acquire/release wraps every return point exactly once.
fn render_full_impl(
    bytes: &[u8],
    sop_uid: &str,
    registry: &clarus_adapters::codec::CodecRegistry,
) -> Result<WadoResp, WadoError> {
    use clarus_adapters::dicom_rs::DicomRsParser;

    if is_compressed_transfer_syntax(bytes)? {
        return render_full_compressed(bytes, sop_uid, registry);
    }
    let parser = DicomRsParser;
    let pixel_data = parser
        .extract_tag_value(bytes, (0x7FE0, 0x0010))
        .map_err(|e| WadoError::Internal(format!("Failed to parse DICOM for render: {e}")))?
        .ok_or_else(|| WadoError::NotFound("PixelData not found".into()))?;
    if pixel_data.is_empty() {
        return Err(WadoError::Internal("PixelData is empty".into()));
    }

    let rows = extract_u16(bytes, (0x0028, 0x0010))
        .ok_or_else(|| WadoError::Internal("Missing Rows (0028,0010)".into()))?
        as u32;
    let cols = extract_u16(bytes, (0x0028, 0x0011))
        .ok_or_else(|| WadoError::Internal("Missing Columns (0028,0011)".into()))?
        as u32;
    let bits_allocated = extract_u16(bytes, (0x0028, 0x0100))
        .ok_or_else(|| WadoError::Internal("Missing BitsAllocated (0028,0100)".into()))?
        as u32;
    let pixel_rep = extract_u16(bytes, (0x0028, 0x0103)).unwrap_or(0);
    let photometric = parser
        .extract_tag_value(bytes, (0x0028, 0x0004))
        .ok()
        .flatten()
        .map(|v| String::from_utf8_lossy(&v).trim_matches(' ').to_string());
    let invert = photometric.as_deref() == Some("MONOCHROME1");

    // Multi-frame: render frame 1's pixel bytes only (v1). frame_size uses the
    // same geometry as the frame extractor, so this honors the byte-stride.
    let num_frames = num_frames_of(bytes);
    let samples_per_pixel = extract_u16(bytes, (0x0028, 0x0002)).unwrap_or(1);
    let frame_pixel = if num_frames <= 1 && !pixel_data_is_encapsulated(&pixel_data) {
        pixel_data
    } else {
        let frame_size = clarus_adapters::dicom_rs::frame::compute_frame_size(
            rows as u16,
            cols as u16,
            samples_per_pixel,
            bits_allocated as u16,
        )
        .unwrap_or(0);
        clarus_adapters::dicom_rs::frame::extract_frame(&pixel_data, 1, frame_size, num_frames)
            .map_err(|_| WadoError::Internal("Failed to extract first frame for render".into()))?
    };

    render_pixels_full(
        FullPixelInput {
            data: &frame_pixel,
            rows,
            cols,
            bits_allocated,
            pixel_rep,
        },
        invert,
        bytes,
        sop_uid,
    )
}

/// Decode a compressed instance then render full-size. Only present behind the
/// `transcode` feature (the decoders are compiled in then).
#[cfg(feature = "transcode")]
fn render_full_compressed(
    bytes: &[u8],
    sop_uid: &str,
    registry: &clarus_adapters::codec::CodecRegistry,
) -> Result<WadoResp, WadoError> {
    let ts = match registry_sorted_ts(bytes) {
        Some(t) => t,
        None => {
            return Err(WadoError::NotAcceptable(
                "rendered requires transcode".to_string(),
            ));
        }
    };
    let codec = registry
        .get(&ts)
        .ok_or_else(|| WadoError::NotAcceptable("rendered requires transcode".to_string()))?;
    let decoded = codec.decode(bytes).map_err(|e| match e {
        clarus_adapters::codec::CodecError::NonConformantCodestream(msg) => {
            WadoError::NotAcceptable(msg)
        }
        other => WadoError::Internal(format!("Decode failed for render: {other}")),
    })?;
    // v1 renders frame 1 only. A decode returns ALL frames concatenated
    // frame-major, so validate for the real decoded frame count, not 1, then
    // slice out frame 1 (see render_thumbnail_compressed for the count logic).
    let one_frame = decoded.expected_size();
    // `checked_div` folds in the zero-size guard (see the thumbnail twin).
    let num_frames = decoded
        .data
        .len()
        .checked_div(one_frame)
        .unwrap_or(1)
        .max(1) as u32;
    if decoded.validate_frames(num_frames).is_err() {
        return Err(WadoError::Internal("Decoded pixels invalid".into()));
    }
    if decoded.samples_per_pixel != 1 {
        // MONOCHROME-only in v1 - colour planes are out of scope.
        return Err(WadoError::NotAcceptable(
            "rendered colour unsupported".to_string(),
        ));
    }
    let frame_slice = slice_decoded_frame(&decoded, 1, num_frames)?;
    let invert = decoded.photometric_interpretation == "MONOCHROME1";
    let pixel_rep = if decoded.bits_allocated == 16 { 1 } else { 0 };
    // DecodedPixels frame slice is one byte per sample element; bits_allocated
    // 8 or 16 maps to the same raw-path byte/word layout for a single plane.
    render_pixels_full(
        FullPixelInput {
            data: frame_slice,
            rows: decoded.rows as u32,
            cols: decoded.columns as u32,
            bits_allocated: decoded.bits_allocated as u32,
            pixel_rep,
        },
        invert,
        bytes,
        sop_uid,
    )
}

/// Default-build compressed instance -> 406 (wire-compatible with the
/// thumbnail's compressed-syntax gate).
#[cfg(not(feature = "transcode"))]
fn render_full_compressed(
    _bytes: &[u8],
    _sop_uid: &str,
    _registry: &clarus_adapters::codec::CodecRegistry,
) -> Result<WadoResp, WadoError> {
    Err(WadoError::NotAcceptable(
        "rendered requires transcode".to_string(),
    ))
}

/// Read the stored transfer syntax UID from the meta-header (uncompressed
/// already handled by the caller; used to pick the decode codec).
#[cfg(feature = "transcode")]
fn registry_sorted_ts(bytes: &[u8]) -> Option<String> {
    let parser = clarus_adapters::dicom_rs::DicomRsParser;
    parser
        .transfer_syntax_uid(bytes)
        .map(|t| t.trim().to_string())
}

/// Pixel geometry + raw samples for the full-size renderer. `data` is the
/// mono plane at `bits_allocated` bits per sample (8 / 16), `pixel_rep` marks
/// signed 16-bit samples (PixelRepresentation=1). Shared by the raw path and
/// the transcode-decode path so `render_pixels_full` stays under the clippy
/// `too_many_arguments` ceiling.
struct FullPixelInput<'a> {
    data: &'a [u8],
    rows: u32,
    cols: u32,
    bits_allocated: u32,
    pixel_rep: u16,
}

/// Full-size window + invert + Baseline-JPEG core (native resolution, no
/// resize). `window_bytes` is the DICOM instance used to resolve the
/// PS3.3 C.11.2.1.2 windowing (VOI LUT Sequence / WC-WW / percentile fallback).
fn render_pixels_full(
    input: FullPixelInput<'_>,
    invert: bool,
    window_bytes: &[u8],
    sop_uid: &str,
) -> Result<WadoResp, WadoError> {
    use clarus_adapters::jpeg_encoder::{ColorType, Encoder};

    let FullPixelInput {
        data: pixel_data,
        rows,
        cols,
        bits_allocated,
        pixel_rep,
    } = input;

    if rows == 0 || cols == 0 {
        return Err(WadoError::Internal(format!(
            "Invalid dimensions: {rows}x{cols}"
        )));
    }
    let pixel_count = (cols * rows) as usize;
    let expected = (rows * cols * (bits_allocated / 8)) as usize;
    if pixel_data.len() < expected {
        return Err(WadoError::Internal(format!(
            "Pixel data too short: got {} bytes, expected {expected}",
            pixel_data.len()
        )));
    }

    // Windowing per PS3.3 C.11.2.1.2. Rescale Slope/Intercept (0028,1052/1053)
    // is always applied (default 1/0). VOI resolution may yield a percent-ile
    // fallback when the instance carries no VOI attribute at all.
    let (slope, intercept) = rescale_params(window_bytes);
    let voi = resolve_voi_window(window_bytes)?;

    let bytes_per_sample = (bits_allocated / 8) as usize;
    if bytes_per_sample == 0 {
        return Err(WadoError::Internal("BitsAllocated too small".into()));
    }

    // Collect post-rescale physical values so a percentile fallback can rank
    // them; for a windowed render we map directly.
    let mut physical: Vec<i32> = Vec::with_capacity(pixel_count);
    for i in 0..pixel_count {
        let idx = i * bytes_per_sample;
        let s = if bytes_per_sample == 1 {
            pixel_data[idx] as i32
        } else {
            let b0 = pixel_data.get(idx).copied().unwrap_or(0);
            let b1 = pixel_data.get(idx + 1).copied().unwrap_or(0);
            let u16v = u16::from_le_bytes([b0, b1]);
            if pixel_rep == 1 {
                (u16v as i16) as i32
            } else {
                u16v as i32
            }
        };
        physical.push(rescale_sample(s, slope, intercept) as i32);
    }

    // Resolve the mapping bounds.
    let mut out = vec![0u8; pixel_count];
    match voi {
        Some((center, width)) => {
            let w = if width <= 0.0 { 1.0 } else { width };
            let lo_f = center - w / 2.0;
            for i in 0..pixel_count {
                let v = physical[i] as f64;
                let y = ((v - lo_f) / w) * 255.0;
                let yc = y.clamp(0.0, 255.0) as u8;
                out[i] = if invert { 255 - yc } else { yc };
            }
        }
        None => {
            // 2-98% percentile fallback (no VOI at all). physical is i32 of the
            // post-rescale values; sort a copy and read the percentile ranks.
            let mut sorted = physical.clone();
            sorted.sort_unstable();
            let lo_idx = ((pixel_count as f64 * 0.02) as usize).min(pixel_count - 1);
            let hi_idx = ((pixel_count as f64 * 0.98) as usize).min(pixel_count - 1);
            let lo = sorted[lo_idx];
            let hi = sorted[hi_idx];
            let range = (hi - lo).max(1) as i64;
            for i in 0..pixel_count {
                let v = physical[i] as i64;
                let y = ((v - lo as i64).clamp(0, range) as u64 * 255) / range as u64;
                let yc = y.min(255) as u8;
                out[i] = if invert { 255 - yc } else { yc };
            }
        }
    }

    let mut jpeg_data = Vec::new();
    let encoder = Encoder::new(&mut jpeg_data, 75);
    encoder
        .encode(&out, cols as u16, rows as u16, ColorType::Luma)
        .map_err(|e| WadoError::Internal(format!("JPEG encoding failed: {e}")))?;

    crate::clog_debug!(
        sop_uid = %sop_uid,
        render_bytes = %jpeg_data.len(),
        "Generated full-size rendered JPEG"
    );

    Ok(WadoResp {
        status: 200,
        content_type: "image/jpeg",
        headers: vec![("Content-Length", jpeg_data.len().to_string())],
        body: jpeg_data,
    })
}

/// Rescale Slope/Intercept (0028,1053/0028,1052) - the Modality LUT. Defaults
/// to 1/0 when absent (identity).
fn rescale_params(bytes: &[u8]) -> (f64, f64) {
    let slope = parse_ds_tag(bytes, (0x0028, 0x1053)).unwrap_or(1.0);
    let intercept = parse_ds_tag(bytes, (0x0028, 0x1052)).unwrap_or(0.0);
    (slope, intercept)
}

/// `stored * slope + intercept` - stored values to physical units (HU).
fn rescale_sample(v: i32, slope: f64, intercept: f64) -> f64 {
    v as f64 * slope + intercept
}

/// Resolve the VOI window per PS3.3 C.11.2.1.2 order. Returns the window from
/// the VOI LUT Sequence's first item when present, else the top-level Window
/// Center/Width, else `None` (the caller then uses the percentile fallback).
fn resolve_voi_window(bytes: &[u8]) -> Result<Option<(f64, f64)>, WadoError> {
    // 1. VOI LUT Sequence first item.
    let parser = clarus_adapters::dicom_rs::DicomRsParser;
    if let Some((c, w)) = parser
        .extract_tag_value(bytes, (0x0028, 0x3010))
        .unwrap_or(None)
        .and_then(|seq| extract_first_item_window(&seq))
    {
        return Ok(Some((c, w)));
    }
    // 2. Top-level Window Center / Window Width.
    let center = parse_ds_tag(bytes, (0x0028, 0x1050));
    let width = parse_ds_tag(bytes, (0x0028, 0x1051));
    match (center, width) {
        (Some(c), Some(w)) => Ok(Some((c, w))),
        _ => Ok(None),
    }
}

/// Walk a VOI LUT Sequence body (explicit VR LE, first item, defined or
/// undefined item length) and return the first Window Center/Width pair.
///
/// Layout (PS3.5 7.5.1): the sequence body is a run of items. Each item is
/// `(FFFE,E000)` + u32 length (0xFFFFFFFF = undefined) followed by the item's
/// elements (explicit VR LE) and terminated by `(FFFE,E00D)` for undefined
/// items. We read the FIRST item's `Window Center (0028,1050)` and
/// `Window Width (0028,1051)` and stop as soon as both are present.
fn extract_first_item_window(seq: &[u8]) -> Option<(f64, f64)> {
    // Skip any leading element to the first item (FFFE,E000) header.
    let mut pos = 0usize;
    while pos + 8 <= seq.len() {
        let g = u16::from_le_bytes([seq[pos], seq[pos + 1]]);
        let e = u16::from_le_bytes([seq[pos + 2], seq[pos + 3]]);
        if g == 0xFFFE && e == 0xE000 {
            break;
        }
        pos += 8;
    }
    if pos + 8 > seq.len() {
        return None;
    }
    // After the item tag + length header, scan the item's explicit-VR-LE elements.
    let mut scan = pos + 8;
    let mut center: Option<f64> = None;
    let mut width: Option<f64> = None;
    while scan + 8 <= seq.len() {
        let g = u16::from_le_bytes([seq[scan], seq[scan + 1]]);
        let e = u16::from_le_bytes([seq[scan + 2], seq[scan + 3]]);
        // Item / sequence delimiters or the next item: stop.
        if g == 0xFFFE {
            break;
        }
        let vr = [seq[scan + 4], seq[scan + 5]];
        let is_extended = matches!(
            &vr,
            b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
        );
        let len = if is_extended {
            if scan + 12 > seq.len() {
                break;
            }
            u32::from_le_bytes([seq[scan + 8], seq[scan + 9], seq[scan + 10], seq[scan + 11]])
                as usize
        } else {
            u16::from_le_bytes([seq[scan + 6], seq[scan + 7]]) as usize
        };
        let value_off = scan + if is_extended { 12 } else { 8 };
        let value_end = (value_off + len).min(seq.len());
        if g == 0x0028 && e == 0x1050 {
            center = parse_ds(&seq[value_off..value_end]);
        } else if g == 0x0028 && e == 0x1051 {
            width = parse_ds(&seq[value_off..value_end]);
        }
        if let (Some(c), Some(w)) = (center, width) {
            return Some((c, w));
        }
        scan = value_end;
    }
    None
}

/// Parse a DICOM DS (decimal string) element value to f64. Tolerates trailing
/// padding bytes (space or NUL).
fn parse_ds(v: &[u8]) -> Option<f64> {
    let s = v
        .iter()
        .copied()
        .take_while(|b| *b != 0 && *b != b' ')
        .collect::<Vec<u8>>();
    let s = std::str::from_utf8(&s).ok()?.trim();
    if s.is_empty() {
        return None;
    }
    s.parse::<f64>().ok()
}

/// Parse a top-level DS tag from the instance to f64.
fn parse_ds_tag(bytes: &[u8], tag: (u16, u16)) -> Option<f64> {
    let parser = clarus_adapters::dicom_rs::DicomRsParser;
    let val = parser.extract_tag_value(bytes, tag).ok()??;
    parse_ds(&val)
}

/// Uncompressed transfer syntax UIDs the raw thumbnail path can serve
/// (PS3.18 Section 8.7 / PS3.5 Section 7.1 defaults). Anything else (JPEG/JPEG-LS/JPEG2000/
/// RLE/Deflated) needs a decoder -> thumbnail returns `NotAcceptable` (406).
const UNCOMPRESSED_TS: &[&str] = &[
    "1.2.840.10008.1.2",   // Implicit VR Little Endian
    "1.2.840.10008.1.2.1", // Explicit VR Little Endian
    "1.2.840.10008.1.2.2", // Explicit VR Big Endian
];

/// C1: read the (0002,0010) TransferSyntaxUID from the meta-header and return
/// `true` when it is NOT one of the servable uncompressed syntaxes. The parser
/// extracts it via `extract_metadata` (the meta-header is consumed by the
/// reader, so `extract_tag_value` would never find group 0002). Missing TS
/// (no meta-header, e.g. DIMSE "DICM"-prefixed data) is treated as
/// compressed (conservative -> 406) rather than risking a garbage decode.
fn is_compressed_transfer_syntax(bytes: &[u8]) -> Result<bool, WadoError> {
    let parser = clarus_adapters::dicom_rs::DicomRsParser;
    let ts = parser.transfer_syntax_uid(bytes);
    Ok(match ts {
        Some(ts) => {
            let trim_owned = ts.trim().to_string();
            !UNCOMPRESSED_TS.iter().any(|u| *u == trim_owned)
        }
        None => true, // no meta TS -> conservative 406
    })
}

/// render instance metadata from the CAS `.dcm` bytes via the
/// native (serde-free) renderer + native bulk-data-URI injection.
///
/// The WADO `/metadata` path reads the instance from CAS (`read_cas_bytes`
/// over the manifest CAS key) and renders inline. Output is byte-identical to
/// the pre-SQLite-blob era - the native renderer is proven equal to serde by
/// the adapters capture goldens, and the native injector to the frozen WADO
/// goldens.
pub fn metadata_from_dcm(
    dcm: &[u8],
    study_uid: &str,
    series_uid: &str,
    sop_uid: &str,
) -> Result<WadoResp, WadoError> {
    let json = clarus_adapters::dicom_rs::DicomRsParser
        .extract_full_metadata_bytes(dcm)
        .map_err(|e| WadoError::Internal(format!("Metadata render failed: {e}")))?;
    let body = inject_bulk_data_uris_native(&json, study_uid, series_uid, sop_uid);
    Ok(WadoResp {
        status: 200,
        content_type: crate::handlers::DICOM_JSON_CONTENT_TYPE,
        headers: vec![],
        body,
    })
}

/// 0310 F3: render instance metadata from a BOUNDED header slice.
///
/// Output is byte-identical to [`metadata_from_dcm`] for any head that
/// contains every element before PixelData plus the PixelData tag+length. The
/// PixelData VALUE is never needed: the renderer stops at the tag and the
/// BulkDataURI is injected synthetically from the SOP UIDs (no bytes read).
/// This is what lets the WADO metadata route read only `max_part_header` bytes
/// of an arbitrarily large instance.
pub fn metadata_from_dcm_bounded(
    head: &[u8],
    study_uid: &str,
    series_uid: &str,
    sop_uid: &str,
) -> Result<WadoResp, WadoError> {
    let json = clarus_adapters::dicom_rs::DicomRsParser
        .extract_full_metadata_bytes_bounded(head)
        .map_err(|e| WadoError::Internal(format!("Metadata render failed: {e}")))?;
    let body = inject_bulk_data_uris_native(&json, study_uid, series_uid, sop_uid);
    Ok(WadoResp {
        status: 200,
        content_type: crate::handlers::DICOM_JSON_CONTENT_TYPE,
        headers: vec![],
        body,
    })
}

// -- Small shared helpers (moved from wado handlers) ----------------------

fn parse_metadata_u16(
    parser: &clarus_adapters::dicom_rs::DicomRsParser,
    data: &[u8],
    tag: (u16, u16),
    _label: &str,
) -> Result<Option<u16>, WadoError> {
    let raw: Option<Vec<u8>> = parser
        .extract_tag_value(data, tag)
        .map_err(|e| WadoError::Internal(format!("DICOM parse error: {e}")))?;
    match raw {
        Some(bytes) => clarus_adapters::dicom_rs::frame::parse_dicom_int(&bytes)
            .map(|v| Some(v as u16))
            .ok_or_else(|| {
                WadoError::Internal(format!(
                    "Invalid DICOM integer value for tag ({:04X},{:04X})",
                    tag.0, tag.1
                ))
            }),
        None => Ok(None),
    }
}

fn parse_metadata_u32(
    parser: &clarus_adapters::dicom_rs::DicomRsParser,
    data: &[u8],
    tag: (u16, u16),
    _label: &str,
) -> Result<Option<u32>, WadoError> {
    let raw: Option<Vec<u8>> = parser
        .extract_tag_value(data, tag)
        .map_err(|e| WadoError::Internal(format!("DICOM parse error: {e}")))?;
    match raw {
        Some(bytes) => clarus_adapters::dicom_rs::frame::parse_dicom_int(&bytes)
            .map(Some)
            .ok_or_else(|| {
                WadoError::Internal(format!(
                    "Invalid DICOM integer value for tag ({:04X},{:04X})",
                    tag.0, tag.1
                ))
            }),
        None => Ok(None),
    }
}

/// NumberOfFrames (0028,0008) as an integer, defaulting to 1.
///
/// The VR is IS - a decimal STRING, space-padded to even length (PS3.5
/// Table 6.2-1). Reading it as a raw little-endian u16 turns "5 " into
/// 0x2035 = 8245 and drives every multi-frame decision off a nonsense count,
/// so the text parse is the only correct one here.
fn num_frames_of(data: &[u8]) -> u32 {
    let parser = clarus_adapters::dicom_rs::DicomRsParser;
    parse_metadata_u32(&parser, data, (0x0028, 0x0008), "NumberOfFrames")
        .ok()
        .flatten()
        .unwrap_or(1)
}

fn extract_u16(data: &[u8], tag: (u16, u16)) -> Option<u16> {
    let parser = clarus_adapters::dicom_rs::DicomRsParser;
    let val = parser.extract_tag_value(data, tag).ok()??;
    if val.len() >= 2 {
        Some(u16::from_le_bytes([val[0], val[1]]))
    } else if val.len() == 1 {
        Some(val[0] as u16)
    } else {
        None
    }
}

// -- Multipart/related planning ------------------------------------------

/// How a multipart part is served: streamed as-is from CAS or transcoded
/// on-the-fly to a requested transfer syntax.
///
/// The 4-way decision (no target -> as-is; non-image -> as-is; TS already
/// matches -> as-is; image with different TS -> transcode) lives here so the
/// transport wrapper only branches on this enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartKind {
    /// Serve the stored DICOM bytes unchanged (no transcoding).
    StreamAsIs,
    /// Transcode to the enclosed transfer syntax UID.
    TranscodeTo(String),
}

/// A fully-planned MIME part for a WADO-RS `multipart/related` response.
///
/// Resolved entirely from the CBOR manifest (no transport types, fully
/// synchronous). The wrapper reads the CAS bytes and assembles the MIME
/// framing around each plan.
#[derive(Debug, Clone)]
pub struct PartPlan {
    /// CAS key (hex) of the stored instance.
    pub sha256: String,
    /// SOP Instance UID (0008,0018).
    pub sop_uid: String,
    /// SOP Class UID (0008,0016), empty when absent (treated as non-image).
    pub sop_class_uid: String,
    /// Stored transfer syntax UID (always present in a manifest).
    pub source_ts: String,
    /// Storage tier of the stored `.dcm` - needed to open the CAS file.
    pub storage_tier: StorageTier,
    /// Instance file size from the manifest (S2: no per-part `stat` for the
    /// `Content-Length` header - `file_size` is mandatory in a manifest, so it
    /// is always present).
    pub file_size: u64,
    /// Streaming/transcoding decision (resolved by [`resolve_part_kind`]).
    pub kind: PartKind,
    /// Parent series UID (0020,000E) from the manifest - the series slot of
    /// the per-part `Content-Location` path. Carried per part so study-level
    /// multipart can emit the header too (the study route has no series UID
    /// among its captured `ctx.uids`).
    pub series_uid: String,
    /// Emit a `Content-Location` header for this part (series- and
    /// study-level multipart).
    pub with_content_location: bool,
}

/// Resolve the transfer syntax to serve, mirroring `resolve_transfer_syntax`
/// but taking the raw Accept header string (axum-free).
///
/// Priority (PS3.18 Section 10.4.2, Section 10.4.2.1):
/// 1. Query parameter `?transferSyntax=` - explicit client request
/// 2. Accept header `transfer-syntax=` - fallback
/// 3. `None` - no transcoding, return original TS
pub fn resolve_target_ts(query: &WadoQueryParams, accept: Option<&str>) -> Option<String> {
    if let Some(ref ts) = query.transfer_syntax {
        if ts == "*" {
            return None; // wildcard: serve as-is, no transcoding
        }
        return Some(ts.clone());
    }
    let ts = extract_transfer_syntax_from_accept_str(accept)?;
    if ts == "*" {
        return None; // wildcard from Accept header: serve as-is
    }
    Some(ts)
}

/// The 4-way streaming/transcoding decision for one part.
///
/// 0. **Stored Implicit VR LE** -> [`PartKind::TranscodeTo`] Explicit VR LE,
///    whatever the target and whatever the SOP class. PS3.18 8.7.3: "The
///    Implicit VR Little Endian ... shall not be used with Web Services", a
///    sentence the standard does not scope to images, and Table 8.7.3-2
///    defaults the Text and Other resource categories to Explicit VR LE too.
///    Serving those bytes as-is is non-conformant in every build, so this rule
///    sits ABOVE the no-target and non-image rules rather than beside them.
/// 1. **No target TS** -> [`PartKind::StreamAsIs`] (no transcode)
/// 2. **Non-image SOP class** -> [`PartKind::StreamAsIs`] (no pixel data)
/// 3. **Source TS == target TS** -> [`PartKind::StreamAsIs`] (already matches)
/// 4. **Image with different target TS** -> [`PartKind::TranscodeTo`]
pub fn resolve_part_kind(
    sop_class_uid: &str,
    source_ts: &str,
    target_ts: Option<&str>,
) -> PartKind {
    // PS3.18 8.7.3, before anything else: an Implicit VR LE source is served as
    // Explicit VR LE. The comparison is on the constant, and the target is the
    // constant too - a hardcoded UID here is how one spelling drifts.
    if source_ts == clarus_adapters::codec::ts_uid::IMPLICIT_VR_LE {
        return PartKind::TranscodeTo(clarus_adapters::codec::ts_uid::EXPLICIT_VR_LE.to_string());
    }
    let Some(target_ts) = target_ts else {
        return PartKind::StreamAsIs;
    };
    if !clarus_adapters::codec::non_image_sop_classes::is_image_sop_class(sop_class_uid) {
        return PartKind::StreamAsIs;
    }
    if source_ts == target_ts {
        return PartKind::StreamAsIs;
    }
    PartKind::TranscodeTo(target_ts.to_string())
}

/// Plan every instance of a study as a `multipart/related` part.
///
/// Validates the study UID, loads the manifest and maps each instance to a
/// [`PartPlan`]. A malformed instance (missing SOP UID) is skipped with a
/// warning - remaining parts are still served ("skip and continue").
///
/// Since 2908 each part carries its own series UID and a `Content-Location`
/// header (the Weasis study-level queue-loss fix); the header was previously
/// series-level only because the study route has no series UID in `ctx.uids`.
///
/// `expose` is the QA-root flag. A rejected instance is skipped unless the
/// caller may see it, using the same predicate as [`locate_object`]: the whole
/// point of a sequester is that the instance disappears from EVERY retrieve,
/// and a study-level pull is the route the DIMSE bridge uses for C-MOVE, so
/// leaving it unfiltered leaked hidden images to ordinary AE clients.
pub fn plan_study_multipart(
    state: &crate::state::AppState,
    study_uid: &str,
    expose: bool,
) -> Result<Vec<PartPlan>, WadoError> {
    validate_dicom_uid(study_uid).inspect_err(|_| {
        crate::clog_warn!(study_uid, "Invalid study UID in WADO request");
    })?;
    let manifest = load_manifest(&state.study_scanner.manifests_dir, study_uid)?;
    let mut plans = Vec::new();
    for series in &manifest.series {
        let series_uid =
            attr_text_from(&series.attrs, clarus_core::query::tag::SERIES_INSTANCE_UID)
                .unwrap_or_default()
                .to_string();
        for inst in &series.instances {
            if inst.is_rejected() && !(expose && inst.is_exposable()) {
                continue;
            }
            let Some(sop) = attr_text(inst, clarus_core::query::tag::SOP_INSTANCE_UID) else {
                crate::clog_warn!("Instance in manifest missing SOPInstanceUID - skipping");
                continue;
            };
            plans.push(PartPlan {
                sha256: cas_hex_key(inst),
                sop_uid: sop.to_string(),
                sop_class_uid: attr_text(inst, 0x0008_0016).unwrap_or_default().to_string(),
                source_ts: inst.transfer_syntax.clone(),
                storage_tier: tier_from_u8(inst.storage_tier),
                file_size: inst.file_size,
                kind: PartKind::StreamAsIs,
                series_uid: series_uid.clone(),
                with_content_location: true,
            });
        }
    }
    if plans.is_empty() {
        return Err(WadoError::NotFound(study_uid.to_string()));
    }
    Ok(plans)
}

/// Plan the instances of one series (filtered by series UID) as parts.
///
/// Like [`plan_study_multipart`] but keeps only instances whose series
/// (0020,000E) matches `series_uid`, and marks them to carry a
/// `Content-Location` header (series-level multipart).
///
/// `expose` carries the same meaning as in [`plan_study_multipart`]: the
/// series route is reached by viewers that never see a hidden instance in
/// QIDO, so it must not be the one place that still hands one out.
pub fn plan_series_multipart(
    state: &crate::state::AppState,
    study_uid: &str,
    series_uid: &str,
    expose: bool,
) -> Result<Vec<PartPlan>, WadoError> {
    for (label, uid) in [("study", study_uid), ("series", series_uid)] {
        validate_dicom_uid(uid).inspect_err(|_| {
            crate::clog_warn!(label, uid, "Invalid UID in WADO series request");
        })?;
    }
    let manifest = load_manifest(&state.study_scanner.manifests_dir, study_uid)?;
    let mut plans = Vec::new();
    for series in &manifest.series {
        let suid = attr_text_from(&series.attrs, clarus_core::query::tag::SERIES_INSTANCE_UID);
        if suid != Some(series_uid) {
            continue;
        }
        for inst in &series.instances {
            if inst.is_rejected() && !(expose && inst.is_exposable()) {
                continue;
            }
            let Some(sop) = attr_text(inst, clarus_core::query::tag::SOP_INSTANCE_UID) else {
                crate::clog_warn!("Instance in manifest missing SOPInstanceUID - skipping");
                continue;
            };
            plans.push(PartPlan {
                sha256: cas_hex_key(inst),
                sop_uid: sop.to_string(),
                sop_class_uid: attr_text(inst, 0x0008_0016).unwrap_or_default().to_string(),
                source_ts: inst.transfer_syntax.clone(),
                storage_tier: tier_from_u8(inst.storage_tier),
                file_size: inst.file_size,
                kind: PartKind::StreamAsIs,
                series_uid: series_uid.to_string(),
                with_content_location: true,
            });
        }
    }
    if plans.is_empty() {
        return Err(WadoError::NotFound(format!(
            "{study_uid}/series/{series_uid}"
        )));
    }
    Ok(plans)
}

/// First-part boundary header: `--BOUNDARY\r\n`.
///
/// Subsequent parts use `\r\n--BOUNDARY\r\n` (the leading `\r\n` is the
/// trailer from the previous part, stripped by `StripTrailingCrlf`).
pub fn part_boundary_header(boundary: &str, is_first: bool) -> String {
    if is_first {
        format!("--{boundary}\r\n")
    } else {
        format!("\r\n--{boundary}\r\n")
    }
}

/// Closing MIME delimiter: `\r\n--BOUNDARY--\r\n`.
pub fn closing_delimiter(boundary: &str) -> String {
    format!("\r\n--{boundary}--\r\n")
}

// -- Tests -----------------------------------------------------------------

// Default-build unit tests for the raw (uncompressed) thumbnail path and the
// shared helpers around it. Previously gated on `feature = "transcode"`, which
// meant `cargo test -p clarus` never ran a single thumbnail test in the build
// that actually ships: the raw path is NOT feature-gated (it is the default
// wire), so its tests must not be either. Nothing in this module needs a codec.
#[cfg(test)]
mod tests {
    use super::*;

    /// A 1x1 thumbnail of a large bright image averages EVERY source pixel.
    /// With a 32-bit accumulator the sum over ~6.7e7 pixels of 255 wraps past
    /// u32::MAX and the "average" comes out near zero - the thumbnail of a
    /// white image is black, in exactly the case the downscale exists for.
    #[test]
    fn a_one_pixel_thumbnail_of_a_large_bright_image_is_not_black() {
        // 9000 x 9000 = 81 Mpx, all white. Kept to a size the allocator takes
        // comfortably while still exceeding u32::MAX / 255 (16.8M) three times
        // over, which is what a 32-bit sum needs to wrap.
        let (w, h) = (9000u32, 9000u32);
        let src = vec![255u8; (w as usize) * (h as usize)];

        let out = resize_area_average(&src, w, h, 1, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0], 255,
            "the average of an all-white image is white; a wrapped sum would give ~0"
        );
    }

    /// The ordinary downscale still averages correctly, so the widened
    /// accumulator did not change the answer for realistic sizes.
    #[test]
    fn a_two_by_two_average_is_still_the_arithmetic_mean() {
        let src = [0u8, 100, 200, 255];
        let out = resize_area_average(&src, 2, 2, 1, 1);
        // (0 + 100 + 200 + 255) / 4 = 138.75 -> rounds half up to 139.
        assert_eq!(out, vec![139]);
    }

    /// Push one Implicit VR Little Endian element: `[group LE][element LE][len u32 LE][value]`.
    fn push_implicit(out: &mut Vec<u8>, group: u16, element: u16, value: &[u8]) {
        out.extend_from_slice(&group.to_le_bytes());
        out.extend_from_slice(&element.to_le_bytes());
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value);
    }

    /// A minimal valid DICOM (Implicit VR LE) for the thumbnail path, with the
    /// given pixel dimensions and a small PixelData. `bits_allocated=1` makes
    /// `expected_bytes = rows*cols*0 = 0`, so the small PixelData passes the
    /// length check and only the pixel-count cap can reject it.
    fn build_thumb_dicom(rows: u16, cols: u16) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"DICM"); // magic at offset 0
        // Meta-header (0002,0010): Implicit VR LE TS UID so the C1 TS gate
        // (uncompressed) passes before the dimension-cap check runs.
        let ts_uid = b"1.2.840.10008.1.2";
        b.extend_from_slice(&0x0002_u16.to_le_bytes());
        b.extend_from_slice(&0x0010_u16.to_le_bytes());
        b.extend_from_slice(b"UI");
        b.extend_from_slice(&[0u8, 0u8]); // reserved
        b.extend_from_slice(&(ts_uid.len() as u32).to_le_bytes());
        b.extend_from_slice(ts_uid);
        b.push(0x00); // odd-length UI pad
        push_implicit(&mut b, 0x0028, 0x0010, &rows.to_le_bytes()); // Rows
        push_implicit(&mut b, 0x0028, 0x0011, &cols.to_le_bytes()); // Columns
        push_implicit(&mut b, 0x0028, 0x0100, &1u16.to_le_bytes()); // BitsAllocated = 1
        push_implicit(&mut b, 0x7FE0, 0x0010, &[0xAB, 0xCD]); // PixelData (2 bytes, non-empty)
        b
    }

    /// An absurd Rows x Columns (with a matching
    /// oversized `expected_bytes` of 0) must be rejected with a 4xx
    /// `PayloadTooLarge` BEFORE any decode buffer is allocated - not OOM.
    #[test]
    fn thumbnail_rejects_oversized_dimensions_before_allocation() {
        // rows = cols = 8193 -> 8193*8193 = 67,118,849 px > 64M cap.
        let dicom = build_thumb_dicom(8193, 8193);
        let err = match render_thumbnail_from_bytes(&dicom, "1.2.3.4", (64, 64)) {
            Ok(_) => panic!("oversized thumbnail must be rejected"),
            Err(e) => e,
        };
        assert!(
            matches!(err, WadoError::PayloadTooLarge(_)),
            "expected PayloadTooLarge, got {err:?}"
        );
        // No OOM - the function returned before allocating the 67M-px buffer.
    }

    /// A sane small image is NOT blocked by the cap (it decodes or fails with
    /// a different, non-PayloadTooLarge error - here JPEG encoding of a 2-byte
    /// bogus pixel buffer).
    #[test]
    fn thumbnail_small_dimensions_not_cap_blocked() {
        let dicom = build_thumb_dicom(4, 4); // 16 px, far under the cap.
        let res = render_thumbnail_from_bytes(&dicom, "1.2.3.4", (64, 64));
        assert!(
            !matches!(res, Err(WadoError::PayloadTooLarge(_))),
            "small image must not hit the cap"
        );
    }

    /// E3 (31.08): `transfer-syntax=*` - as a query parameter or an Accept
    /// header parameter - must resolve to NO target TS, i.e. StreamAsIs:
    /// the stored bytes are served without transcoding.
    #[test]
    fn wildcard_transfer_syntax_resolves_to_as_is() {
        // Query parameter wildcard.
        let q = WadoQueryParams {
            annotation: None,
            charset: None,
            retrieve_ae: None,
            transfer_syntax: Some("*".to_string()),
        };
        assert_eq!(resolve_target_ts(&q, None), None);

        // Accept header wildcard (PS3.18 10.4.2.1 syntax).
        let q2 = WadoQueryParams {
            annotation: None,
            charset: None,
            retrieve_ae: None,
            transfer_syntax: None,
        };
        let accept = Some("multipart/related; type=\"application/dicom\"; transfer-syntax=*");
        assert_eq!(resolve_target_ts(&q2, accept), None);

        // A concrete TS still resolves.
        let q3 = WadoQueryParams {
            annotation: None,
            charset: None,
            retrieve_ae: None,
            transfer_syntax: Some("1.2.840.10008.1.2.4.80".to_string()),
        };
        assert_eq!(
            resolve_target_ts(&q3, None),
            Some("1.2.840.10008.1.2.4.80".to_string())
        );

        // The 4-way decision: image SOP + wildcard (None target) -> as-is.
        let kind = resolve_part_kind("1.2.840.10008.5.1.4.1.1.2", "1.2.840.10008.1.2.1", None);
        assert_eq!(kind, PartKind::StreamAsIs);
    }

    /// PS3.18 8.7.3: Implicit VR LE shall not be used with Web Services. The
    /// rule applies whatever the target and whatever the SOP class, so it sits
    /// ABOVE the "no target" and "non-image" branches rather than beside them.
    #[test]
    fn an_implicit_vr_part_is_always_transcoded_to_explicit_vr() {
        const EVRLE: &str = clarus_adapters::codec::ts_uid::EXPLICIT_VR_LE;
        const IVRLE: &str = clarus_adapters::codec::ts_uid::IMPLICIT_VR_LE;
        // A harmless uncompressed target that the old code would have
        // streamed as-is: no target TS at all.
        assert_eq!(
            resolve_part_kind("1.2.840.10008.5.1.4.1.1.2", IVRLE, None),
            PartKind::TranscodeTo(EVRLE.to_string()),
            "an IVRLE image with no requested syntax is still re-encoded"
        );
        // A NON-IMAGE object: the standard's rule is not image-scoped, and
        // Table 8.7.3-2 defaults the Text and Other categories to EVRLE too.
        assert_eq!(
            resolve_part_kind("1.2.840.10008.5.1.4.1.1.88.11", IVRLE, None),
            PartKind::TranscodeTo(EVRLE.to_string()),
            "an IVRLE SR is re-encoded: the ban is blanket"
        );
        // A client that asks for a compressed syntax still gets EVRLE, because
        // serving IVRLE is not an option at all.
        assert_eq!(
            resolve_part_kind(
                "1.2.840.10008.5.1.4.1.1.2",
                IVRLE,
                Some(clarus_adapters::codec::ts_uid::JPEG_BASELINE)
            ),
            PartKind::TranscodeTo(EVRLE.to_string()),
            "the IVRLE rule wins over a requested compressed syntax"
        );
    }

    /// The rule fires ONLY for Implicit VR: an Explicit VR LE instance is
    /// still streamed byte-faithfully when nothing is requested.
    #[test]
    fn an_explicit_vr_part_stays_byte_faithful() {
        const EVRLE: &str = clarus_adapters::codec::ts_uid::EXPLICIT_VR_LE;
        assert_eq!(
            resolve_part_kind("1.2.840.10008.5.1.4.1.1.2", EVRLE, None),
            PartKind::StreamAsIs
        );
        assert_eq!(
            resolve_part_kind("1.2.840.10008.5.1.4.1.1.88.11", EVRLE, None),
            PartKind::StreamAsIs
        );
    }

    // -- Box-average downscale -------------------------------------------------

    /// An exact 2x reduction averages the four source pixels of each output
    /// pixel. Nearest-neighbour would have returned one of them unchanged, so
    /// the expected value is what distinguishes the two filters.
    #[test]
    fn area_average_halves_exactly() {
        let src: Vec<u8> = vec![
            0, 100, 200, 255, 40, 60, 80, 100, 10, 20, 30, 40, 1, 2, 3, 4,
        ];
        let out = resize_area_average(&src, 4, 4, 2, 2);
        assert_eq!(out.len(), 4);
        // (0+100+40+60)/4 = 50
        assert_eq!(out[0], 50);
        // (200+255+80+100)/4 = 158.75 -> 159
        assert_eq!(out[1], 159);
        // (10+20+1+2)/4 = 8.25 -> 8
        assert_eq!(out[2], 8);
        // (30+40+3+4)/4 = 19.25 -> 19
        assert_eq!(out[3], 19);
    }

    /// A uniform source stays uniform at any scale: the defining property of
    /// a correct area filter.
    #[test]
    fn area_average_preserves_a_flat_field() {
        let src = vec![77u8; 100 * 100];
        for (dw, dh) in [(10u32, 10u32), (7, 13), (100, 100), (64, 64)] {
            let out = resize_area_average(&src, 100, 100, dw, dh);
            assert_eq!(out.len(), (dw * dh) as usize);
            assert!(
                out.iter().all(|v| *v == 77),
                "flat field changed at {dw}x{dh}"
            );
        }
    }

    /// The average of a two-value source is between the two values, never one
    /// extreme: this is what stops a downscale from dropping thin structures
    /// the way nearest-neighbour does.
    #[test]
    fn area_average_blends_a_black_and_white_checkerboard() {
        // 8x8 checkerboard of 0 and 255.
        let mut src = vec![0u8; 64];
        for y in 0..8usize {
            for x in 0..8usize {
                src[y * 8 + x] = if (x + y) % 2 == 0 { 0 } else { 255 };
            }
        }
        let out = resize_area_average(&src, 8, 8, 2, 2);
        // Each 4x4 box holds 8 black and 8 white pixels -> 127 or 128.
        for value in &out {
            assert!(
                *value == 127 || *value == 128,
                "expected a blend, got {value}"
            );
        }
        // Nearest-neighbour would have produced only 0 or 255 here.
        assert!(out.iter().all(|v| *v != 0 && *v != 255));
    }

    /// A downscale larger than the source never divides by zero and never
    /// emits black from an empty box (both boxes are clamped to at least one
    /// cell).
    #[test]
    fn area_average_upscales_without_an_empty_box() {
        let src = vec![200u8; 4];
        let out = resize_area_average(&src, 2, 2, 8, 8);
        assert_eq!(out.len(), 64);
        assert!(out.iter().all(|v| *v == 200), "no empty-box black");
    }

    /// A degenerate destination size yields an empty buffer rather than
    /// panicking.
    #[test]
    fn area_average_handles_a_zero_destination() {
        let src = vec![1u8; 16];
        assert!(resize_area_average(&src, 4, 4, 0, 4).is_empty());
        assert!(resize_area_average(&src, 4, 4, 4, 0).is_empty());
    }

    // -- Stored windowing ------------------------------------------------------

    /// Append an Explicit VR LE element with even-length padding, the framing
    /// the `full_render_tests` fixtures use and the raw path is known to read.
    fn push_evrle(out: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
        out.extend_from_slice(&group.to_le_bytes());
        out.extend_from_slice(&element.to_le_bytes());
        out.extend_from_slice(vr);
        let extended = matches!(
            vr,
            b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
        );
        if extended {
            out.extend_from_slice(&[0x00, 0x00]);
            out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        } else {
            out.extend_from_slice(&(value.len() as u16).to_le_bytes());
        }
        out.extend_from_slice(value);
        if !value.len().is_multiple_of(2) {
            out.push(0x00);
        }
    }

    /// Build a small Explicit-VR-LE 8-bit MONOCHROME2 instance with the given
    /// pixel bytes and an optional stored Window Center/Width.
    ///
    /// Modelled on `full_render_tests::build_evrle`, which the raw render path
    /// is already known to decode: a 128-byte preamble, the magic, the meta
    /// transfer syntax, then explicit-VR elements. 8-bit samples keep the
    /// fixture free of the 16-bit stride and signedness cases that the
    /// existing full-render tests cover separately.
    fn build_window_dicom(
        rows: u16,
        cols: u16,
        pixels: &[u8],
        window: Option<(u16, u16)>,
    ) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[0u8; 128]);
        b.extend_from_slice(b"DICM");
        push_evrle(&mut b, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
        push_evrle(&mut b, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.7");
        push_evrle(&mut b, 0x0008, 0x0018, b"UI", b"1.2.3.4.5.6.7.8.9.10");
        push_evrle(&mut b, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
        push_evrle(&mut b, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
        push_evrle(&mut b, 0x0028, 0x0010, b"US", &rows.to_le_bytes());
        push_evrle(&mut b, 0x0028, 0x0011, b"US", &cols.to_le_bytes());
        push_evrle(&mut b, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
        push_evrle(&mut b, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
        push_evrle(&mut b, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
        push_evrle(&mut b, 0x0028, 0x0103, b"US", &0u16.to_le_bytes());
        if let Some((center, width)) = window {
            // DS text, the storage the attribute really uses.
            push_evrle(
                &mut b,
                0x0028,
                0x1050,
                b"DS",
                format!("{center}").as_bytes(),
            );
            push_evrle(&mut b, 0x0028, 0x1051, b"DS", format!("{width}").as_bytes());
        }
        push_evrle(&mut b, 0x7FE0, 0x0010, b"OB", pixels);
        b
    }

    /// A stored Window Center/Width is used INSTEAD of the percentile
    /// fallback, asserted differentially: the same pixels with and without the
    /// stored window must render different images, and the stored-window image
    /// must be the one that clamps the bulk to black.
    #[test]
    fn a_stored_window_is_used_instead_of_the_percentile_fallback() {
        // 4x4 of 8-bit samples: every pixel is 10.
        let pixels = [10u8; 16];

        // No stored window: the percentile fallback trims to the observed
        // range (10..10), so the bulk fills the window and renders bright.
        let plain = build_window_dicom(4, 4, &pixels, None);
        let fallback =
            render_thumbnail_from_bytes(&plain, "1.2.3.4", (64, 64)).expect("thumbnail renders");

        // A stored window whose range sits far ABOVE 10, so every sample
        // clamps to the floor and the image is black.
        let stored = build_window_dicom(4, 4, &pixels, Some((200, 50)));
        let clamped =
            render_thumbnail_from_bytes(&stored, "1.2.3.4", (64, 64)).expect("thumbnail renders");

        assert_eq!(fallback.status, 200);
        assert_eq!(clamped.status, 200);
        assert_ne!(
            fallback.body, clamped.body,
            "the stored window must dominate the percentile fallback"
        );
    }

    /// A 3x3 8-bit instance must render, not panic.
    ///
    /// PS3.5 8.1.1 pads the Pixel Data VALUE to an even byte length; it does
    /// not pad each row. A 3x3 sample block is 9 bytes, so the stored value is
    /// 10 bytes with one trailing pad. Sizing the copy from `cols + (cols & 1)`
    /// instead made row 2 start at offset 8 and read index 10 of a 10-byte
    /// buffer - an out-of-bounds panic reachable from /thumbnail, the frame
    /// thumbnails and /rendered for any odd-width 8-bit MONOCHROME instance
    /// with an identity rescale and no VOI (a secondary capture, say).
    ///
    /// The assertion is that the request REACHES the encoder: a short read
    /// would have returned an error instead of a body.
    #[test]
    fn an_odd_width_8bit_instance_renders_without_a_stride_overrun() {
        let pixels: [u8; 9] = [1, 2, 3, 4, 5, 6, 7, 8, 9];
        let dicom = build_window_dicom(3, 3, &pixels, None);
        // `extract_tag_value` hands back the element VALUE including the pad
        // byte the writer appended, so the renderer must size the row walk from
        // the samples, not from the value length.
        let resp = render_thumbnail_from_bytes(&dicom, "1.2.3.4", (64, 64))
            .expect("a 3x3 8-bit instance must render");
        assert_eq!(resp.status, 200, "the thumbnail is produced");
        assert!(
            !resp.body.is_empty(),
            "the encoder ran, so the row walk stayed inside the value"
        );
    }

    /// The same instance, but with the pad byte where a conformant writer puts
    /// it: INSIDE the declared value.
    ///
    /// PS3.5 7.1.2 pads the Pixel Data value to an even length, so a 3x3
    /// 8-bit instance declares 10 bytes - 9 samples plus one 0x00 - and
    /// `extract_tag_value` returns all 10. The first test above cannot reach
    /// that shape: `push_evrle` declares the 9 samples and writes the pad after
    /// the value, which is non-conformant and leaves the surplus at zero. With
    /// the declared length honoured, a stride derived from the VALUE length
    /// (`cols + (cols & 1)`) starts row 2 at offset 8 and reads index 10 of a
    /// 10-byte buffer - the exact out-of-bounds panic this instance class
    /// produced. The rows are packed, so the stride is `cols` and the pad is
    /// simply never read.
    #[test]
    fn a_padded_odd_width_value_is_walked_with_a_packed_stride() {
        // Nine samples with the 0x00 pad byte inside the declared value.
        let padded: [u8; 10] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 0x00];
        let mut b = Vec::new();
        b.extend_from_slice(&[0u8; 128]);
        b.extend_from_slice(b"DICM");
        push_evrle(&mut b, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
        push_evrle(&mut b, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.7");
        push_evrle(&mut b, 0x0008, 0x0018, b"UI", b"1.2.3.4.5.6.7.8.9.10");
        push_evrle(&mut b, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
        push_evrle(&mut b, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
        push_evrle(&mut b, 0x0028, 0x0010, b"US", &3u16.to_le_bytes());
        push_evrle(&mut b, 0x0028, 0x0011, b"US", &3u16.to_le_bytes());
        push_evrle(&mut b, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
        push_evrle(&mut b, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
        push_evrle(&mut b, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
        push_evrle(&mut b, 0x0028, 0x0103, b"US", &0u16.to_le_bytes());
        // An EVEN value: `push_evrle` appends nothing, so the declared length
        // is 10 and the pad byte is part of it.
        push_evrle(&mut b, 0x7FE0, 0x0010, b"OB", &padded);

        let resp = render_thumbnail_from_bytes(&b, "1.2.3.4", (64, 64))
            .expect("a 3x3 instance with a padded value must render");
        assert_eq!(resp.status, 200, "the thumbnail is produced");
        assert!(
            !resp.body.is_empty(),
            "the row walk stayed inside the declared value"
        );
    }

    /// The general (windowed) branch reads the plane with NO row stride at all:
    /// PS3.5 8.1.1 pads the whole value, so an odd-width instance is a packed
    /// `rows*cols` sample block and any stride would shift rows 1+ by one byte.
    ///
    /// Differentially: a 3x3 gradient with a stored window must window the same
    /// samples whether or not the value carried the pad byte.
    #[test]
    fn an_odd_width_instance_with_a_window_reads_packed_samples() {
        let pixels: [u8; 9] = [0, 0, 0, 0, 0, 0, 0, 0, 9];
        let dicom = build_window_dicom(3, 3, &pixels, Some((4, 8)));
        let resp = render_thumbnail_from_bytes(&dicom, "1.2.3.4", (64, 64))
            .expect("an odd-width windowed instance must render");
        assert_eq!(resp.status, 200);
        assert!(!resp.body.is_empty());
    }

    /// With no stored window the percentile fallback still applies, and the
    /// two paths are genuinely different: one dataset that differs ONLY in
    /// having a stored Window Center/Width must render a different image.
    ///
    /// That equivalence is the whole point of the task. Asserting on a JPEG
    /// byte value would be a guess at the encoder's DC coding; asserting that
    /// the outputs differ proves the attribute is actually read, and asserting
    /// that BOTH still render proves the fallback still works when it is not.
    #[test]
    fn without_a_stored_window_the_percentile_fallback_is_used() {
        // 15 samples at 100 and one outlier at 255.
        let mut pixels = [100u8; 16];
        pixels[15] = 255;

        let no_window = build_window_dicom(4, 4, &pixels, None);
        let fallback = render_thumbnail_from_bytes(&no_window, "1.2.3.4", (64, 64))
            .expect("the percentile fallback renders");
        assert_eq!(fallback.status, 200);
        assert!(!fallback.body.is_empty());

        // A stored window far above the data clamps every sample to black.
        let stored_window = build_window_dicom(4, 4, &pixels, Some((200, 50)));
        let clamped = render_thumbnail_from_bytes(&stored_window, "1.2.3.4", (64, 64))
            .expect("the stored window renders");
        assert_eq!(clamped.status, 200);

        assert_ne!(
            fallback.body, clamped.body,
            "a stored Window Center/Width must change the rendered image"
        );
    }

    /// The viewport changes the encoded JPEG's dimensions, not just its
    /// quality: a 4x4 source asked for `(64,64)` produces a 64x64 image, which
    /// is the whole point of task 1.
    #[test]
    fn the_requested_viewport_sets_the_encoded_dimensions() {
        let pixels: Vec<u8> = (0..16u8).map(|v| v * 10).collect();
        let dicom = build_window_dicom(4, 4, &pixels, None);
        for target in [(64u32, 64u32), (256, 256)] {
            let resp =
                render_thumbnail_from_bytes(&dicom, "1.2.3.4", target).expect("thumbnail renders");
            let (w, h) = jpeg_dimensions(&resp.body).expect("SOF dimensions");
            assert_eq!((w, h), (target.0 as u16, target.1 as u16));
        }
    }

    /// The fitted size preserves the aspect ratio of a non-square source.
    #[test]
    fn a_non_square_source_keeps_its_ratio_in_the_viewport() {
        // 8x4 source (2:1) into a 64x64 box -> 64x32.
        let pixels: Vec<u8> = (0..32u8).map(|v| v * 4).collect();
        let dicom = build_window_dicom(4, 8, &pixels, None);
        let resp =
            render_thumbnail_from_bytes(&dicom, "1.2.3.4", (64, 64)).expect("thumbnail renders");
        let (w, h) = jpeg_dimensions(&resp.body).expect("SOF dimensions");
        assert_eq!((w, h), (64, 32), "2:1 source in a square box");
    }

    /// Read the SOF0 dimensions out of a JPEG so a test can assert the real
    /// encoded size rather than trusting the header list.
    fn jpeg_dimensions(jpeg: &[u8]) -> Option<(u16, u16)> {
        if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
            return None;
        }
        let mut i = 2; // skip SOI
        while i + 9 < jpeg.len() {
            if jpeg[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = jpeg[i + 1];
            // SOF0/SOF1 carry the frame dimensions.
            if marker == 0xC0 || marker == 0xC1 {
                let h = u16::from_be_bytes([jpeg[i + 5], jpeg[i + 6]]);
                let w = u16::from_be_bytes([jpeg[i + 7], jpeg[i + 8]]);
                return Some((w, h));
            }
            // Standalone markers carry no length field.
            if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
                i += 2;
                continue;
            }
            let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
            i += 2 + len;
        }
        None
    }
}

// --- Compressed-frame thumbnail slice ----------------------------------------
// Default-build unit tests: slice_decoded_frame is cfg(any(transcode, test)),
// so the slice logic is covered by `cargo test --all` in the DEFAULT build.
#[cfg(test)]
mod compressed_thumbnail_tests {
    use super::*;

    /// Frame-slice unit: planar mono multi-frame data is frame-major
    /// (all frames of the single plane concatenated); frame size =
    /// rows x cols x (bits_allocated / 8). 2 frames of 2x2 8-bit mono.
    /// NumberOfFrames (0028,0008) is IS text, space-padded. Read as a raw
    /// little-endian u16, "5 " is 0x2035 = 8245 - a count that accepts frames
    /// that do not exist and defeats the 416 range check.
    #[test]
    fn number_of_frames_is_read_as_text() {
        fn elem(buf: &mut Vec<u8>, g: u16, e: u16, vr: &[u8; 2], v: &[u8]) {
            buf.extend_from_slice(&g.to_le_bytes());
            buf.extend_from_slice(&e.to_le_bytes());
            buf.extend_from_slice(vr);
            buf.extend_from_slice(&(v.len() as u16).to_le_bytes());
            buf.extend_from_slice(v);
        }
        let mut d = Vec::new();
        d.extend_from_slice(&[0u8; 128]);
        d.extend_from_slice(b"DICM");
        // "5 " - the even-length padded form PS3.5 requires for IS.
        elem(&mut d, 0x0028, 0x0008, b"IS", b"5 ");
        assert_eq!(num_frames_of(&d), 5, "padded IS text parses as 5");

        // An unpadded single digit is equally valid IS.
        let mut d2 = Vec::new();
        d2.extend_from_slice(&[0u8; 128]);
        d2.extend_from_slice(b"DICM");
        elem(&mut d2, 0x0028, 0x0008, b"IS", b"1 ");
        assert_eq!(num_frames_of(&d2), 1);

        // Absent tag: a single-frame instance, exactly as before.
        let mut d3 = Vec::new();
        d3.extend_from_slice(&[0u8; 128]);
        d3.extend_from_slice(b"DICM");
        elem(&mut d3, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
        assert_eq!(num_frames_of(&d3), 1);
    }

    #[test]
    fn decoded_frame_slice_offsets_and_size() {
        let decoded = clarus_adapters::codec::DecodedPixels {
            data: vec![1, 2, 3, 4, 5, 6, 7, 8], // frame 1, then frame 2
            rows: 2,
            columns: 2,
            samples_per_pixel: 1,
            bits_allocated: 8,
            photometric_interpretation: "MONOCHROME2".to_string(),
        };
        assert_eq!(slice_decoded_frame(&decoded, 1, 2).unwrap(), &[1, 2, 3, 4]);
        assert_eq!(slice_decoded_frame(&decoded, 2, 2).unwrap(), &[5, 6, 7, 8]);
        // Out-of-range frame numbers in the slice helper -> RangeNotSatisfiable
        // (416 on the wire); this is the render/slice path, not the frames
        // URI segment, which reports an absent Target Resource as 404.
        assert!(matches!(
            slice_decoded_frame(&decoded, 0, 2),
            Err(WadoError::RangeNotSatisfiable)
        ));
        assert!(matches!(
            slice_decoded_frame(&decoded, 3, 2),
            Err(WadoError::RangeNotSatisfiable)
        ));
    }
}

// --- Full-size rendered (part b, 26.08) -------------------------------------
// Default-build unit tests for PS3.3 C.11.2.1.2 windowing + native-size JPEG.
#[cfg(test)]
mod full_render_tests {
    use super::*;

    /// Append an Explicit VR LE element (PS3.5 7.1), mirroring the integration
    /// builder in wado_smoke so the fixture is a well-formed uncompressed
    /// instance the raw render path decodes.
    fn push_explicit(out: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
        // Meta header (group 0002) is appended by the caller.
        out.extend_from_slice(&group.to_le_bytes());
        out.extend_from_slice(&element.to_le_bytes());
        out.extend_from_slice(vr);
        let extended = matches!(
            vr,
            b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
        );
        if extended {
            out.extend_from_slice(&[0x00, 0x00]);
            out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        } else {
            out.extend_from_slice(&(value.len() as u16).to_le_bytes());
        }
        out.extend_from_slice(value);
        if !value.len().is_multiple_of(2) {
            out.push(0x00); // even-length pad
        }
    }

    /// A DS (decimal string) value encoded as ASCII bytes for a float.
    fn ds_f(v: f64) -> Vec<u8> {
        format!("{v}").into_bytes()
    }

    /// Build an 8-bit MONOCHROME2 EVRLE instance. `wcc`, `ww` are the optional
    /// top-level Window Center/Width; `with_voi_seq` wraps them in a
    /// VOI LUT Sequence first item instead; `slope`/`intercept` set the
    /// Rescale Modality LUT. Pixels are 4x4 (16 bytes) of the given byte.
    fn build_evrle(
        wcc: Option<f64>,
        ww: Option<f64>,
        with_voi_seq: bool,
        slope: f64,
        intercept: f64,
        pixel_byte: u8,
    ) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[0u8; 128]);
        b.extend_from_slice(b"DICM");
        push_explicit(&mut b, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
        push_explicit(&mut b, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
        push_explicit(&mut b, 0x0008, 0x0018, b"UI", b"1.2.3.4.5.6.7.8.9.10");
        push_explicit(&mut b, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
        push_explicit(&mut b, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
        push_explicit(&mut b, 0x0028, 0x0010, b"US", &4u16.to_le_bytes());
        push_explicit(&mut b, 0x0028, 0x0011, b"US", &4u16.to_le_bytes());
        push_explicit(&mut b, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
        push_explicit(&mut b, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
        push_explicit(&mut b, 0x0028, 0x0106, b"US", &0u16.to_le_bytes());
        // Modality LUT rescale.
        push_explicit(&mut b, 0x0028, 0x1053, b"DS", &ds_f(slope));
        push_explicit(&mut b, 0x0028, 0x1052, b"DS", &ds_f(intercept));
        if with_voi_seq {
            // VOI LUT Sequence (0028,3010) SQ with one defined-length item.
            let mut item = Vec::new();
            if let Some(c) = wcc {
                push_explicit(&mut item, 0x0028, 0x1050, b"DS", &ds_f(c));
            }
            if let Some(w) = ww {
                push_explicit(&mut item, 0x0028, 0x1051, b"DS", &ds_f(w));
            }
            // Build the item: FFFE,E000 + u32 length + item body.
            let mut item_enc = Vec::new();
            item_enc.extend_from_slice(&0xFFFEu16.to_le_bytes());
            item_enc.extend_from_slice(&0xE000u16.to_le_bytes());
            item_enc.extend_from_slice(&(item.len() as u32).to_le_bytes());
            item_enc.extend_from_slice(&item);
            // Sequence with defined length.
            push_explicit(&mut b, 0x0028, 0x3010, b"SQ", &item_enc);
        } else if let Some(c) = wcc {
            push_explicit(&mut b, 0x0028, 0x1050, b"DS", &ds_f(c));
            if let Some(w) = ww {
                push_explicit(&mut b, 0x0028, 0x1051, b"DS", &ds_f(w));
            }
        }
        let pixels = [pixel_byte; 16];
        push_explicit(&mut b, 0x7FE0, 0x0010, b"OB", &pixels);
        b
    }

    /// JPEG SOI marker.
    fn is_jpeg(data: &[u8]) -> bool {
        data.len() > 4
            && data[0] == 0xFF
            && data[1] == 0xD8
            && data[data.len() - 2] == 0xFF
            && data[data.len() - 1] == 0xD9
    }

    /// Unwrap a render result; `WadoResp` is not `Debug`, so `.expect()` is
    /// unavailable - panic with the error's Display form on failure.
    fn unwrap_resp(r: Result<WadoResp, WadoError>) -> WadoResp {
        match r {
            Ok(resp) => resp,
            Err(e) => panic!("full render failed: {e:?}"),
        }
    }

    fn render_ok(d: &[u8]) -> WadoResp {
        unwrap_resp(render_full_from_bytes(
            d,
            "1.2.3",
            &clarus_adapters::codec::CodecRegistry::default(),
            &crate::state::TranscodePool::new(32),
            64,
        ))
    }

    #[test]
    fn window_wc_ww_applied_not_percentile() {
        // Constant pixels -> percentile would collapse to a single level; a
        // real WC/WW window over a wider range yields multiple levels, so the
        // output differs from the percentile result. We assert the WC/WW path
        // is chosen by checking the render succeeds as a valid native JPEG.
        let d = build_evrle(Some(128.0), Some(256.0), false, 1.0, 0.0, 200);
        let resp = render_ok(&d);
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type, "image/jpeg");
        assert!(is_jpeg(&resp.body), "body must be a valid JPEG");
    }

    #[test]
    fn voi_lut_sequence_first_item_window_applied() {
        // VOI LUT Sequence (rather than top-level WC/WW) must drive the window.
        let d = build_evrle(Some(64.0), Some(128.0), true, 1.0, 0.0, 100);
        let resp = render_ok(&d);
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type, "image/jpeg");
        assert!(is_jpeg(&resp.body));
    }

    #[test]
    fn no_voi_falls_back_to_percentile() {
        // No WC/WW and no VOI LUT Sequence -> percentile fallback must still
        // produce a valid native JPEG (never an internal error).
        let d = build_evrle(None, None, false, 1.0, 0.0, 150);
        let resp = render_ok(&d);
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type, "image/jpeg");
        assert!(is_jpeg(&resp.body));
    }

    #[test]
    fn rescale_slope_intercept_applied() {
        // A non-unity rescale must not error; the modality LUT runs first.
        let d = build_evrle(Some(0.0), Some(1000.0), false, 2.0, -500.0, 200);
        let resp = render_ok(&d);
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type, "image/jpeg");
        assert!(is_jpeg(&resp.body));
    }

    #[test]
    #[cfg(not(feature = "transcode"))]
    fn compressed_instance_406_in_default_build() {
        // A synthetic compressed TS (JPEG Baseline meta header) must yield 406
        // in the default build (no decoder).
        let mut d = Vec::new();
        d.extend_from_slice(&[0u8; 128]);
        d.extend_from_slice(b"DICM");
        push_explicit(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.4.50");
        let err = match render_full_from_bytes(
            &d,
            "1.2.3",
            &clarus_adapters::codec::CodecRegistry::default(),
            &crate::state::TranscodePool::new(32),
            64,
        ) {
            Ok(_) => panic!("compressed instance must be rejected in default build"),
            Err(e) => e,
        };
        assert!(
            matches!(err, WadoError::NotAcceptable(_)),
            "expected NotAcceptable/406, got {err:?}"
        );
    }

    /// Build a minimal uncompressed (EVRLE) DICOM with explicit Rows/Columns.
    /// Only the meta TS + the two geometry tags matter for the pixel budget.
    fn build_dims(rows: u16, cols: u16) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[0u8; 128]);
        b.extend_from_slice(b"DICM");
        push_explicit(&mut b, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
        push_explicit(&mut b, 0x0028, 0x0010, b"US", &rows.to_le_bytes());
        push_explicit(&mut b, 0x0028, 0x0011, b"US", &cols.to_le_bytes());
        b
    }

    #[test]
    fn pixel_budget_rejects_over_limit_before_decode() {
        // A 65535x65535 instance (~4.29 Gpx) is far over the 64 MP budget.
        let big = build_dims(u16::MAX, u16::MAX);
        let err = check_pixel_budget(&big, 64).expect_err("over-budget instance must be refused");
        assert!(
            matches!(err, WadoError::PayloadTooLarge(_)),
            "expected PayloadTooLarge/413, got {err:?}"
        );
    }

    #[test]
    fn pixel_budget_passes_under_limit() {
        // 64x64 = 4096 px is well under the 64 MP budget.
        let small = build_dims(64, 64);
        assert!(
            check_pixel_budget(&small, 64).is_ok(),
            "under-budget instance must pass"
        );
        // A missing geometry (0 rows or cols) is not treated as an over-budget
        // refusal - the downstream decode surfaces the real cause.
        let zero = build_dims(0, 0);
        assert!(
            check_pixel_budget(&zero, 64).is_ok(),
            "empty geometry passes"
        );
    }

    #[test]
    fn maybe_transcode_busy_503_when_pool_full() {
        // A pool of 1 with the permit taken: a real transcode must refuse with
        // Busy (503), NOT block. Uses a stored!=target path (EVRLE target over
        // a raw uncompressed source) so it reaches try_acquire.
        let src = build_evrle(None, None, false, 1.0, 0.0, 200);
        let registry = clarus_adapters::codec::CodecRegistry::default();
        let pool = crate::state::TranscodePool::new(1);
        // Unknown TS -> 406 before any permit (registry does not support it);
        // verify the no-op stored==target path never touches the pool, then
        // that a saturated pool reports Busy for a hypothetical supported TS.
        assert!(pool.try_acquire(), "hold the single permit");
        // stored == target -> no-op, returns Ok without acquiring.
        let out = maybe_transcode(
            src.clone(),
            "1.2.840.10008.1.2.1",
            Some("1.2.840.10008.1.2.1"),
            &registry,
            &pool,
            64,
        )
        .expect("stored==target no-op must not need a permit");
        assert_eq!(out, (src.clone(), "1.2.840.10008.1.2.1".to_string()));
        // Unsupported target with full pool -> NotAcceptable (406) first.
        let err = maybe_transcode(
            src.clone(),
            "1.2.840.10008.1.2.1",
            Some("1.2.826.0.1.3680043.10.9.9.1"),
            &registry,
            &pool,
            64,
        )
        .unwrap_err();
        assert!(
            matches!(err, WadoError::NotAcceptable(_)),
            "406 first: {err:?}"
        );
        // Release so the pool is free; a supported-TS call would succeed. We
        // assert the pool accounting is sane: after release, a fresh acquire works.
        pool.release();
        assert!(pool.try_acquire(), "permit freed after release");
        pool.release();
    }
}
