// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Shared helpers for WADO-RS handlers.
//!
//! Provides [`inject_bulk_data_uris_native`] (the serde-free byte-level
//! injector that replaces large binary DICOM tag values with `BulkDataURI`
//! references in metadata responses) plus the CBOR-manifest conversion helpers
//! (`bytes_to_hex`, `tier_from_u8`, `cas_hex_key`) and the manifest lookup
//! helpers that back WADO: resolves and serves
//! instances straight from the per-study CBOR manifest, with no SQLite lookup
//! in the hot path.

#![forbid(unsafe_code)]

use std::path::Path;
use std::sync::Arc;

use clarus_core::manifest::{InstanceRecord, Manifest, ManifestReadError};
use clarus_ports::StorageTier;

use super::error::WadoError;

/// VR values that indicate binary (bulk) data, per PS3.5 Table 6.2-1.
pub const BULK_DATA_VRS: &[&str] = &["OB", "OW", "OF", "OD", "OL", "OV", "UN"];

/// Tag group+element for PixelData (7FE0,0010).
pub const PIXEL_DATA_TAG: &str = "7FE00010";

/// Maximum inline value length in characters before BulkDataURI is used.
/// Values longer than this from bulk-data VRs are replaced with a URI.
pub const BULK_DATA_THRESHOLD: usize = 64;

/// Lowercase hex: 32 bytes -> 64 chars.
///
/// CAS internally validates: len == 64 && all ASCII hex digits. Both
/// BLAKE3 (32 bytes) and SHA-256 (32 bytes) produce 64 hex chars, so
/// either hash yields a valid CAS key regardless of path resolution
/// (BLAKE3 2-level first, legacy SHA-256 fallback).
pub fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX_CHARS[(b >> 4) as usize] as char);
        s.push(HEX_CHARS[(b & 0x0f) as usize] as char);
    }
    s
}

/// u8 storage tier from `InstanceRecord` -> `StorageTier` enum.
///
/// Tiers follow the manifest constants: 1 = Hot, 2 = Cold, 3 = S3. Any
/// out-of-range byte (corrupt manifest) defaults to `Hot`, the safest target -
/// a wrong tier only triggers the cross-tier fallback chain, never a 500.
pub fn tier_from_u8(t: u8) -> StorageTier {
    match t {
        1 => StorageTier::Hot,
        2 => StorageTier::Cold,
        3 => StorageTier::S3,
        _ => StorageTier::Hot,
    }
}

/// InstanceRecord -> 64-char hex CAS key (BLAKE3).
///
/// All instances are BLAKE3-addressed (legacy SHA-256 excised,
/// data re-uploaded), so the key is always the 32-byte BLAKE3 hash rendered
/// to the 64-char hex CAS requires.
pub fn cas_hex_key(inst: &InstanceRecord) -> String {
    bytes_to_hex(&inst.blake3)
}

/// Open and decode the study manifest for `study_uid` through the decoded-
/// manifest cache: a hit clones the cached `Arc` (0 alloc,
/// pinned by the ap101_manifest_cache gate), a miss decodes from disk and
/// inserts only if no writer committed during the decode.
///
/// Maps `ManifestReadError` onto the WADO error contract:
/// `Io` (file missing) becomes 404 - a legitimate "no such study";
/// `Decode` (corrupt manifest) becomes 500 - lazy recovery should have
/// repaired it, and surfacing it as internal is honest rather than hiding a
/// corrupt file.
/// The branch stays on the MISS path (the cache stores only successful
/// decodes), so the corruption signal is never masked.
pub fn load_manifest(manifests_dir: &Path, study_uid: &str) -> Result<Arc<Manifest>, WadoError> {
    match clarus_core::manifest_cache::get_or_load(manifests_dir, study_uid) {
        Ok(m) => Ok(m),
        Err(ManifestReadError::Io(_)) => Err(WadoError::NotFound(study_uid.to_string())),
        Err(ManifestReadError::Decode(e)) => {
            // manifest-degradation (Etap 2): a corrupt manifest must be
            // surfaced through the live /health + /metrics signal, not only the
            // per-request ACCESS log. Request behaviour is unchanged (still an
            // error); the hot path is only touched on the error branch. The
            // WARN is rate-limited per study file so a badly-corrupt manifest
            // does not spam the log on every WADO hit.
            crate::metrics::MANIFEST_CORRUPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            crate::metrics::MANIFEST_DEGRADED.store(1, std::sync::atomic::Ordering::Relaxed);
            let manifest_path = manifests_dir.join(format!("{study_uid}.manifest"));
            if crate::clog::rate_limited_allow(study_uid.as_bytes()) {
                crate::clog_warn!(
                    study_uid,
                    file = %manifest_path.display(),
                    error = %e,
                    "Corrupt B-tree manifest during read"
                );
            }
            Err(WadoError::Internal(format!(
                "Corrupt study manifest for {study_uid}"
            )))
        }
    }
}

/// Resolve a single instance from the manifest by SOP Instance UID.
///
/// A missing instance yields a 404, mirroring the old SQLite `NotFound`.
pub fn find_instance<'a>(
    manifest: &'a Manifest,
    sop_uid: &str,
) -> Result<&'a InstanceRecord, WadoError> {
    manifest
        .find_instance(sop_uid)
        .ok_or_else(|| WadoError::NotFound(sop_uid.to_string()))
}

/// The text value of a DICOM attribute list, by tag.
///
/// Returns `None` when the tag is absent or its value is not text - callers
/// fall back to a default (an empty SOP Class is treated as "unknown", which
/// matches the legacy SQLite `Option` handling).
pub fn attr_text_from(attrs: &[(u32, clarus_core::manifest::AttrValue)], tag: u32) -> Option<&str> {
    attrs.iter().find_map(|(t, v)| {
        if *t == tag
            && let clarus_core::manifest::AttrValue::Text(s) = v
        {
            return Some(s.as_str());
        }
        None
    })
}

/// The text value of a DICOM attribute on an instance, by tag.
pub fn attr_text(inst: &InstanceRecord, tag: u32) -> Option<&str> {
    attr_text_from(&inst.attrs, tag)
}

// -- native (serde-free) bulk-data-URI injection ----------
//
// Byte-level transform over the *native renderer's* metadata JSON (the output
// of `DicomRsParser::extract_full_metadata_bytes`). That JSON has a fixed,
// fully-known grammar - `{"TAG":{"Value":<arr>,"vr":"VR"}}` entries in
// ascending hex-tag order - so no JSON DOM is required. The transform is
// byte-identical to the frozen WADO goldens and the injector unit tests.

/// Return the index just past the closing quote of a JSON string whose opening
/// quote sits at `open` (escape-aware: `\"`, `\\`, and `\uXXXX` are skipped).
fn json_string_end(bytes: &[u8], open: usize) -> usize {
    debug_assert_eq!(bytes.get(open), Some(&b'"'));
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return i + 1,
            b'\\' => {
                // Skip escaped char (and the 4 hex digits of \uXXXX).
                i += 1;
                if bytes.get(i) == Some(&b'u') {
                    i += 5;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    bytes.len()
}

/// Find the index of the `}` that closes the object whose `{` sits at `open`,
/// scanning escape-aware and ignoring braces inside strings. Assumes the
/// value is a single top-level object (the renderer's tag object).
fn json_object_end(bytes: &[u8], open: usize) -> usize {
    debug_assert_eq!(bytes.get(open), Some(&b'{'));
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            b'"' => {
                let end = json_string_end(bytes, i);
                i = end;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

/// Extract `("vr", value_len)` from a native-rendered tag object.
///
/// Two shapes are recognized:
///
/// * binary VRs render as `{"InlineBinary":"<base64>","vr":"VR"}` - the
///   value length is the base64 string length (so the >64 threshold still
///   promotes large binary values to BulkDataURI);
/// * every other VR renders as `{"Value":<arr>,"vr":"VR"}` - the value
///   length is the compact-JSON serialized length of the `Value` array (the
///   metric the serde version derived from `serde_json::to_string(Value)`).
fn tag_object_vr_and_value_len(obj: &[u8]) -> Option<(&str, usize)> {
    // Object must be a lone top-level object (the renderer's tag object).
    let after_value_close = json_object_end(obj, 0);
    if after_value_close + 1 != obj.len() {
        return None;
    }

    // Binary shape: `{"InlineBinary":"<base64>","vr":"VR"}`.
    const INLINE_PREFIX: &[u8] = b"{\"InlineBinary\":";
    if let Some(rest) = obj.strip_prefix(INLINE_PREFIX) {
        if rest.first() != Some(&b'"') {
            return None;
        }
        let str_end = json_string_end(rest, 0);
        // base64 length = the string's characters, i.e. minus its two quotes.
        let value_len = str_end.saturating_sub(2);
        let vr = vr_from_tail(&rest[str_end..]);
        return Some((vr, value_len));
    }

    // Value shape: `{"Value":<arr>,"vr":"VR"}`.
    if !obj.starts_with(b"{\"Value\":") {
        return None;
    }
    let arr_open = 9; // index of '[' after `{"Value":`
    if obj.get(arr_open) != Some(&b'[') {
        return None;
    }
    // Escape-aware find of the matching ']' (the array's closing bracket).
    let arr_close = {
        let mut depth = 0usize;
        let mut i = arr_open;
        while i < obj.len() {
            match obj[i] {
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                b'"' => {
                    let e = json_string_end(obj, i);
                    i = e;
                    continue;
                }
                _ => {}
            }
            i += 1;
        }
        i
    };
    let value_len = arr_close.saturating_sub(arr_open) + 1;
    let vr = vr_from_tail(&obj[arr_close + 1..]);
    Some((vr, value_len))
}

/// Extract the two-character VR from a `,"vr":"XX"` tail.
fn vr_from_tail(tail: &[u8]) -> &str {
    const VR_PREFIX: &[u8] = b",\"vr\":\"";
    let vr_start = find_subslice(tail, VR_PREFIX)
        .map(|idx| idx + VR_PREFIX.len())
        .unwrap_or(tail.len());
    let vr = &tail[vr_start..tail.len().min(vr_start + 2)];
    std::str::from_utf8(vr).unwrap_or("")
}

/// Subslice search (no std `memmem`).
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Serde-free bulk-data-URI injection over the native renderer's metadata
/// bytes. Output is byte-identical to the frozen WADO goldens (the serde-era
/// `inject_bulk_data_uris` + `serde_json::to_string`).
pub fn inject_bulk_data_uris_native(
    meta: &[u8],
    study_uid: &str,
    series_uid: &str,
    sop_uid: &str,
) -> Vec<u8> {
    let body = if meta.len() >= 2 && meta[0] == b'{' && meta[meta.len() - 1] == b'}' {
        &meta[1..meta.len() - 1]
    } else {
        return meta.to_vec();
    };

    let pixel_uri =
        format!("/dicomweb/studies/{study_uid}/series/{series_uid}/instances/{sop_uid}/frames/1");

    let mut out = Vec::with_capacity(meta.len() + 128);
    out.push(b'{');
    let mut i = 0usize;
    let mut first = true;
    // DEV-015: PixelData is always injected. If the renderer already emitted it
    // (defensive - the renderer stops at PixelData so it normally doesn't), we
    // rewrite it in place keeping its VR and skip the final append.
    let mut pixel_present = false;

    while i < body.len() {
        // member key
        if body.get(i) != Some(&b'"') {
            break;
        }
        let key_end = json_string_end(body, i);
        let key = &body[i + 1..key_end - 1];
        // `:`
        let colon = key_end; // points at ':'
        let val_start = colon + 1;
        let val_end = json_object_end(body, val_start);
        let tag_obj = &body[val_start..=val_end];

        let is_pixel = key == PIXEL_DATA_TAG.as_bytes();
        let vr_len = tag_object_vr_and_value_len(tag_obj);
        let replace = if is_pixel {
            // Existing PixelData: keep its VR, replace Value with BulkDataURI.
            pixel_present = true;
            false
        } else if let Some((vr, value_len)) = vr_len {
            BULK_DATA_VRS.contains(&vr) && value_len > BULK_DATA_THRESHOLD
        } else {
            false
        };

        if !first {
            out.push(b',');
        }
        first = false;
        out.push(b'"');
        out.extend_from_slice(key);
        out.push(b'"');
        out.push(b':');

        if is_pixel {
            let vr = vr_len
                .map(|(v, _)| v.to_string())
                .unwrap_or_else(|| "OW".to_string());
            out.extend_from_slice(b"{\"BulkDataURI\":\"");
            out.extend_from_slice(pixel_uri.as_bytes());
            out.extend_from_slice(br#"","vr":""#);
            out.extend_from_slice(vr.as_bytes());
            out.push(b'"');
            out.push(b'}');
        } else if replace {
            let tag = key_to_str(key);
            let bulk_uri = format!(
                "/dicomweb/studies/{study_uid}/series/{series_uid}/instances/{sop_uid}/bulkdata/{tag}"
            );
            let vr = vr_len.map(|(v, _)| v.to_string()).unwrap_or_default();
            out.extend_from_slice(b"{\"BulkDataURI\":\"");
            out.extend_from_slice(bulk_uri.as_bytes());
            out.extend_from_slice(br#"","vr":""#);
            out.extend_from_slice(vr.as_bytes());
            out.push(b'"');
            out.push(b'}');
        } else {
            out.extend_from_slice(tag_obj);
        }

        i = val_end + 1;
        if i < body.len() {
            if body[i] == b',' {
                i += 1;
            } else {
                break;
            }
        }
    }

    // PixelData always injected (DEV-015), sorted last (7FE00010 max tag).
    // Only when it wasn't already present in the input.
    if !pixel_present {
        if !first {
            out.push(b',');
        }
        out.extend_from_slice(b"\"7FE00010\":{\"BulkDataURI\":\"");
        out.extend_from_slice(pixel_uri.as_bytes());
        out.extend_from_slice(br#"","vr":"OW"}"#);
    }
    out.push(b'}');

    out
}

/// Key -> string for a tag (8 hex chars); guarantees a stable tag name for the
/// bulkdata URI even if the key bytes are somehow not UTF-8.
fn key_to_str(key: &[u8]) -> String {
    String::from_utf8_lossy(key).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const STUDY: &str = "1.2.826.0.1.3680043.10.1234.6001";
    const SERIES: &str = "1.2.826.0.1.3680043.10.1234.6101";
    const SOP: &str = "1.2.826.0.1.3680043.10.1234.6201";

    /// Bulk/URI path prefix shared by the frozen goldens (Commit A). The
    /// serde-era injector produced these exact URIs; the native injector must
    /// stay byte-identical.
    fn uri_prefix() -> String {
        format!("/dicomweb/studies/{STUDY}/series/{SERIES}/instances/{SOP}")
    }
    fn pixel_golden() -> String {
        format!("{}/frames/1", uri_prefix())
    }
    fn bulk_golden(tag: &str) -> String {
        format!("{}/bulkdata/{tag}", uri_prefix())
    }

    /// Renderer grammar fixture matching `extract_full_metadata_bytes` output:
    /// ascending hex keys (BTreeMap order), `{"Value":...,"vr":"VR"}` objects.
    fn meta(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut sorted: Vec<(&str, &str)> = entries.to_vec();
        sorted.sort_by_key(|e| e.0);
        let mut s = String::from("{");
        for (i, (k, v)) in sorted.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!("\"{k}\":{v}"));
        }
        s.push('}');
        s.into_bytes()
    }

    #[test]
    fn native_inject_matches_serde_small_instance() {
        // Frozen serde-era golden (Commit A): only PixelData injected.
        let m = meta(&[
            ("00020010", r#"{"Value":["1.2.840.10008.1.2.1"],"vr":"UI"}"#),
            (
                "00100010",
                r#"{"Value":[{"Alphabetic":"WADO^SMOKE"}],"vr":"PN"}"#,
            ),
            ("00280010", r#"{"Value":[8],"vr":"US"}"#),
        ]);
        let native = inject_bulk_data_uris_native(&m, STUDY, SERIES, SOP);
        let expected = format!(
            "{{\"00020010\":{{\"Value\":[\"1.2.840.10008.1.2.1\"],\"vr\":\"UI\"}},\
             \"00100010\":{{\"Value\":[{{\"Alphabetic\":\"WADO^SMOKE\"}}],\"vr\":\"PN\"}},\
             \"00280010\":{{\"Value\":[8],\"vr\":\"US\"}},\
             \"7FE00010\":{{\"BulkDataURI\":\"{}\",\"vr\":\"OW\"}}}}",
            pixel_golden()
        );
        assert_eq!(String::from_utf8_lossy(&native), expected, "small instance");
        assert!(
            native
                .windows(b"BulkDataURI".len())
                .any(|w| w == b"BulkDataURI")
        );
    }

    #[test]
    fn native_inject_matches_serde_large_bulk() {
        // >64-char OW -> BulkDataURI, short OB stays inline (now InlineBinary),
        // PixelData injected. Binary VRs render as InlineBinary, not Value.
        let big: String = "A".repeat(90);
        let m = meta(&[
            (
                "00080016",
                r#"{"Value":["1.2.840.10008.5.1.4.1.1.2"],"vr":"UI"}"#,
            ),
            (
                "0040A010",
                &format!(r#"{{"InlineBinary":"{big}","vr":"OW"}}"#),
            ),
            ("0040A020", r#"{"InlineBinary":"c2hvcnQ=","vr":"OB"}"#),
        ]);
        let native = inject_bulk_data_uris_native(&m, STUDY, SERIES, SOP);
        let expected = format!(
            "{{\"00080016\":{{\"Value\":[\"1.2.840.10008.5.1.4.1.1.2\"],\"vr\":\"UI\"}},\
             \"0040A010\":{{\"BulkDataURI\":\"{}\",\"vr\":\"OW\"}},\
             \"0040A020\":{{\"InlineBinary\":\"c2hvcnQ=\",\"vr\":\"OB\"}},\
             \"7FE00010\":{{\"BulkDataURI\":\"{}\",\"vr\":\"OW\"}}}}",
            bulk_golden("0040A010"),
            pixel_golden()
        );
        assert_eq!(String::from_utf8_lossy(&native), expected, "large bulk");
    }

    #[test]
    fn native_inject_matches_serde_existing_pixeldata() {
        // Frozen serde-era golden (Commit A): if PixelData already present,
        // serde preserves its VR (OB) and does not append a duplicate.
        let m = meta(&[
            (
                "00080016",
                r#"{"Value":["1.2.840.10008.5.1.4.1.1.2"],"vr":"UI"}"#,
            ),
            ("7FE00010", r#"{"Value":["c2hvcnQ="],"vr":"OB"}"#),
        ]);
        let native = inject_bulk_data_uris_native(&m, STUDY, SERIES, SOP);
        let expected = format!(
            "{{\"00080016\":{{\"Value\":[\"1.2.840.10008.5.1.4.1.1.2\"],\"vr\":\"UI\"}},\
             \"7FE00010\":{{\"BulkDataURI\":\"{}\",\"vr\":\"OB\"}}}}",
            pixel_golden()
        );
        assert_eq!(
            String::from_utf8_lossy(&native),
            expected,
            "existing pixeldata"
        );
        assert_eq!(
            String::from_utf8_lossy(&native).matches("7FE00010").count(),
            1
        );
    }

    #[test]
    fn native_inject_matches_serde_strings_with_brackets_braces() {
        // Frozen serde-era golden (Commit A): braces/brackets/escaped-quote in
        // strings must not confuse the top-level object scanner.
        let m = meta(&[
            (
                "00100010",
                r#"{"Value":[{"Alphabetic":"A}][\"B"}],"vr":"PN"}"#,
            ),
            ("00081030", r#"{"Value":["Study {1} [x]"],"vr":"LO"}"#),
        ]);
        let native = inject_bulk_data_uris_native(&m, STUDY, SERIES, SOP);
        let expected = format!(
            "{{\"00081030\":{{\"Value\":[\"Study {{1}} [x]\"],\"vr\":\"LO\"}},\
             \"00100010\":{{\"Value\":[{{\"Alphabetic\":\"A}}][\\\"B\"}}],\"vr\":\"PN\"}},\
             \"7FE00010\":{{\"BulkDataURI\":\"{}\",\"vr\":\"OW\"}}}}",
            pixel_golden()
        );
        assert_eq!(
            String::from_utf8_lossy(&native),
            expected,
            "brackets/braces in strings"
        );
    }

    // -- manifest-degradation (Etap 2) --
    // A corrupt B-tree manifest read must increment MANIFEST_CORRUPT and set
    // MANIFEST_DEGRADED (live /health + /metrics signal), not just the ACCESS
    // log. The hot path is only touched on the error branch.

    #[test]
    fn load_manifest_corrupt_increments_degradation_signal() {
        use tempfile::tempdir;
        // Write a manifest file that is present but undecodable (garbage bytes).
        let dir = tempdir().expect("tempdir");
        let study_uid = "1.2.826.0.1.3680043.10.1234.6001";
        std::fs::write(
            dir.path().join(format!("{study_uid}.manifest")),
            b"\x00\xff\x00\xfe not cbor",
        )
        .expect("write corrupt manifest");

        let before = crate::metrics::MANIFEST_CORRUPT.load(std::sync::atomic::Ordering::Relaxed);
        let r = load_manifest(dir.path(), study_uid);
        assert!(r.is_err(), "corrupt manifest must fail to load");
        let after = crate::metrics::MANIFEST_CORRUPT.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            after > before,
            "MANIFEST_CORRUPT must increment on corrupt read (delta >= 1)"
        );
        assert_eq!(
            crate::metrics::MANIFEST_DEGRADED.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "MANIFEST_DEGRADED must be set to 1 on corrupt read"
        );
    }
}
