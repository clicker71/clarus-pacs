// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Query parameter types for WADO-RS and WADO-URI endpoints.
//!
//! the query-string parsers are hand-rolled (no `serde_urlencoded`),
//! eliminating the serde dependency from the WADO-URI path. The wire
//! semantics match the previous `serde_urlencoded` behavior: `+` decodes to a
//! space, `%XX` decodes to a byte, and unknown keys are ignored.

#![forbid(unsafe_code)]

/// URL-decode a percent-encoded form value (`%XX`, and `+` -> space).
///
/// This is the application/x-www-form-urlencoded rule, which is what the
/// WADO-URI and WADO-RS query parsers here use. It is deliberately NOT
/// `json_common::form_decode_query_str`: that one keeps `+` literal, because
/// PS3.18 8.3.1 lists `+` in `qp-special` for DICOMweb query values. The two
/// contracts differ, so the two functions stay separate.
///
/// A `%` starts an escape ONLY when the next two bytes are both ASCII hex
/// digits; otherwise it is a literal `%`. The old code consumed the next two
/// bytes unconditionally and pushed NOTHING when they were not hex, so
/// `1.2%GG3` decoded to `1.23` - a different, perfectly valid UID that selected
/// a different instance. Leaving `%GG` intact lets UID validation reject the
/// value instead of silently reinterpreting it.
pub fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let (hi, lo) = (bytes[i + 1], bytes[i + 2]);
                if hi.is_ascii_hexdigit() && lo.is_ascii_hexdigit() {
                    out.push((hex_val(hi) << 4) | hex_val(lo));
                    i += 3;
                } else {
                    // Not an escape: keep the '%' and advance by ONE, so the
                    // bytes after it are revisited as ordinary input.
                    out.push(b'%');
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    // UTF-8 multi-byte sequences decode into BYTES, not Latin-1 scalars:
    // %D0%BC is "м", not "Ð¼".
    String::from_utf8_lossy(&out).into_owned()
}

/// Value of one ASCII hex digit (0-15). The caller has already checked it is
/// Value of one ASCII hex digit (0-15). The caller has already checked it is
/// hex, so this cannot fail.
fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        _ => b - b'A' + 10,
    }
}

/// Decode a query string into `(key, value)` pairs (both URL-decoded).
///
/// Splits on `&`, each pair on the first `=`. A key with no value decodes to
/// an empty value. Empty keys are dropped (matching form-urlencoding norms).
pub fn parse_query_string(qs: &str) -> Vec<(String, String)> {
    qs.split('&')
        .filter_map(|pair| {
            if pair.is_empty() {
                return None;
            }
            let mut it = pair.splitn(2, '=');
            let k = url_decode(it.next().unwrap_or(""));
            let v = url_decode(it.next().unwrap_or(""));
            if k.is_empty() {
                return None;
            }
            Some((k, v))
        })
        .collect()
}

/// First decoded value for `key`, if present.
fn get<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// WADO-RS query parameters for study/series retrieval (R-05, R-06, R-10).
#[derive(Debug)]
pub struct WadoQueryParams {
    /// Annotation (patient/technique) - not supported (R-05).
    pub annotation: Option<String>,
    /// Character set for text encoding - only utf-8 supported (R-06).
    pub charset: Option<String>,
    /// Retrieve AE Title for DIMSE C-MOVE - not supported (R-10).
    pub retrieve_ae: Option<String>,
    /// Requested transfer syntax for transcoding (G-05).
    pub transfer_syntax: Option<String>,
}

impl WadoQueryParams {
    /// Parse from a query string; all fields are optional (form-urlencoding
    /// default), so any subset is valid.
    pub fn parse(qs: &str) -> Self {
        let pairs = parse_query_string(qs);
        WadoQueryParams {
            annotation: get(&pairs, "annotation").map(str::to_string),
            charset: get(&pairs, "charset").map(str::to_string),
            // (0008,0054) RetrieveAETitle - the form-urlencoding key is the
            // tag itself (`00080054`).
            retrieve_ae: get(&pairs, "00080054").map(str::to_string),
            // `transferSyntax` is the DICOMweb query key for requested TS.
            transfer_syntax: get(&pairs, "transferSyntax").map(str::to_string),
        }
    }
}

/// The largest thumbnail viewport this server will render on either axis.
///
/// PS3.18 Table 10.4.1-5 makes `viewport` optional for Thumbnail Resources
/// and 8.3.5.1.3 requires a 400 for a value that is "not supported", so the
/// supported range has to be a documented decision rather than an implicit
/// one. 512 is chosen because it is the smallest power of two that exceeds
/// every current client's request (OHIF asks 64, Weasis asks 256) while
/// keeping one worst-case render at 512*512 = 256 Ki px - one quarter of a
/// megapixel, so the decode buffers dominate the cost and a large viewport
/// cannot be used to amplify work per request. PS3.18 10.4.5 requires these
/// bounds to appear in the Conformance Statement.
pub const MAX_THUMBNAIL_VIEWPORT: u32 = 512;

/// The size at which a thumbnail is served when the client sends no
/// `viewport`. PS3.18 8.3.5.2 leaves the choice to the origin server ("If no
/// viewport parameter is provided the origin server will determine the size
/// of the Thumbnail"); 64 is kept so OHIF's existing requests are unchanged.
pub const DEFAULT_THUMBNAIL_VIEWPORT: u32 = 64;

/// A parsed thumbnail `viewport`.
///
/// Thumbnails accept ONLY `vw,vh` (PS3.18 8.3.5.1.3: "The syntax of this
/// parameter for a Presentation State Instance or a Thumbnail is
/// `viewport= vw "," vh`"; 8.3.5.2: "The Viewport parameter only has width
/// and height values"). The four-value cropping form that plain Rendered
/// Resources accept is NOT valid here, so its presence is a 400 rather than
/// a silently ignored suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThumbnailViewport {
    pub width: u32,
    pub height: u32,
}

/// Why a `viewport` value was refused. Both variants map to 400 per
/// PS3.18 8.3.5.1.3; they are distinguished so the message can say which
/// rule was broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewportError {
    /// Not two positive integers: non-numeric, zero, negative, or the wrong
    /// number of components (the four-value crop form included).
    IllFormed,
    /// Well-formed, but outside the supported range (larger than
    /// [`MAX_THUMBNAIL_VIEWPORT`], or a size that fits an axis at zero).
    NotSupported,
}

impl ThumbnailViewport {
    /// Parse a `viewport` query value for a Thumbnail Resource.
    ///
    /// `None` means the parameter was absent, which is legal and leaves the
    /// size to this server (see [`DEFAULT_THUMBNAIL_VIEWPORT`]).
    pub fn parse(value: &str) -> Result<Self, ViewportError> {
        let mut parts = value.split(',');
        let (Some(w), Some(h)) = (parts.next(), parts.next()) else {
            return Err(ViewportError::IllFormed);
        };
        // A third component means the caller sent the crop form, which a
        // Thumbnail does not accept.
        if parts.next().is_some() {
            return Err(ViewportError::IllFormed);
        }
        let width = parse_positive(w).ok_or(ViewportError::IllFormed)?;
        let height = parse_positive(h).ok_or(ViewportError::IllFormed)?;
        if width > MAX_THUMBNAIL_VIEWPORT || height > MAX_THUMBNAIL_VIEWPORT {
            return Err(ViewportError::NotSupported);
        }
        Ok(Self { width, height })
    }

    /// The size to render to after fitting the source's aspect ratio.
    ///
    /// The viewport is a bounding box, not a stretch: PS3.18 8.3.5.1.3 says
    /// the content is scaled "maintaining the aspect ratio ... until either
    /// the rendered content width or height is the same as the viewport
    /// width or height, whichever avoids truncation", which is exactly what
    /// `fit_dimensions` computes.
    #[must_use]
    pub fn fit(self, src_w: u32, src_h: u32) -> (u32, u32) {
        let (w, h) = fit_in_box(src_w, src_h, self.width, self.height);
        // Never return a zero axis: a source far wider than the box would
        // otherwise round the short side to 0 and produce an empty image.
        (w.max(1), h.max(1))
    }
}

/// A positive integer, rejecting signs, whitespace and empty strings.
fn parse_positive(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: u32 = text.parse().ok()?;
    if value == 0 { None } else { Some(value) }
}

/// Fit `src_w`x`src_h` inside `box_w`x`box_h`, preserving aspect ratio.
///
/// PS3.18 8.3.5.1.3 scales the content "as large as possible, within the
/// viewport, without overflowing the viewport area and without distorting
/// the image", so this SCALES UP as well as down: a 64x64 source asked for
/// through `viewport=256,256` is rendered at 256x256 rather than passed
/// through at its native size.
///
/// Local to the types module so the viewport rule is testable without the
/// renderer; `wado_pure::fit_dimensions` is the square-box special case that
/// the default thumbnail size uses.
fn fit_in_box(src_w: u32, src_h: u32, box_w: u32, box_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (box_w, box_h);
    }
    let scale = f64::min(box_w as f64 / src_w as f64, box_h as f64 / src_h as f64);
    (
        (src_w as f64 * scale).round() as u32,
        (src_h as f64 * scale).round() as u32,
    )
}

/// Parse `transfer-syntax` parameter from an Accept header string (pure,
/// axum-free). The HeaderMap variant is a thin adapter over this.
///
/// PS3.18 Section 10.4.2.1 defines:
/// ```text
/// Accept: multipart/related; type="application/dicom"; transfer-syntax=1.2.840.10008.1.2.4.80
/// ```
///
/// Returns `None` if no `transfer-syntax` parameter is found or the header
/// is missing/malformed.
///
/// The range's `q` value decides the ORDER, not the presence: PS3.18 10.4.3.1
/// says the origin server "shall" return a representation acceptable to the
/// client, and RFC 9110 12.5.1 makes `q` the client's relative preference. So
/// `transfer-syntax=...4.50;q=0, ...1.2.1;q=1` asks for Explicit VR LE, and
/// returning the JPEG Baseline range because it was listed first ignores the
/// client. `q=0` means "not acceptable" (RFC 9110 12.5.1), so such a range is
/// never selected.
pub fn extract_transfer_syntax_from_accept_str(accept: Option<&str>) -> Option<String> {
    let accept = accept?;
    let mut best: Option<(u16, &str)> = None;
    // RFC 9110 8.3: media ranges are comma-separated and each carries its own
    // parameter list, so split on ',' FIRST - a `transfer-syntax` in one range
    // must not leak across a range boundary. Within a range, parameters are
    // ';'-separated `name=value` pairs; the first segment is the media type
    // itself (no '='), so it is skipped by `split_once`.
    for range in accept.split(',') {
        let mut transfer_syntax: Option<&str> = None;
        let mut weight: u16 = 1000;
        for param in range.split(';') {
            let Some((name, value)) = param.split_once('=') else {
                continue;
            };
            // Parameter names are case-insensitive and may be padded around
            // '=' (RFC 9110 5.6.1); the value may be quoted.
            let name = name.trim();
            let value = value.trim().trim_matches('"').trim();
            if name.eq_ignore_ascii_case("transfer-syntax") {
                transfer_syntax = Some(value);
            } else if name.eq_ignore_ascii_case("q") {
                // An unparsable q makes the range unusable: falling back to
                // 1.0 would silently promote a range the client marked down.
                match crate::handlers::accept::parse_weight_pub(value) {
                    Some(w) => weight = w,
                    None => {
                        transfer_syntax = None;
                        break;
                    }
                }
            }
        }
        let Some(uid) = transfer_syntax else {
            continue;
        };
        // RFC 9110 12.5.1: `q=0` means "not acceptable at all".
        if weight == 0 {
            continue;
        }
        // `>` not `>=`: among equal weights the first range listed wins, which
        // keeps the selection deterministic and matches the client's own order.
        if best.is_none_or(|(w, _)| weight > w) {
            best = Some((weight, uid));
        }
    }
    best.map(|(_, uid)| uid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A malformed escape must be PRESERVED, not deleted.
    ///
    /// `1.2%GG3` used to decode to `1.23`: the decoder consumed `%GG`, failed
    /// to read it as hex, and pushed nothing. The result is a different, valid
    /// UID that selects a different instance, so a typo in the request silently
    /// retrieved someone else's object.
    #[test]
    fn a_malformed_percent_escape_is_preserved() {
        assert_eq!(url_decode("1.2%GG3"), "1.2%GG3", "a bad escape is literal");
        assert_eq!(url_decode("%G1"), "%G1");
        assert_eq!(url_decode("%1G"), "%1G");
        // A trailing or dangling '%' stays.
        assert_eq!(url_decode("abc%"), "abc%");
        assert_eq!(url_decode("%"), "%");
        assert_eq!(url_decode("%2"), "%2");
        // The value must not become a VALID uid by losing bytes.
        assert_ne!(url_decode("1.2%GG3"), "1.23");
    }

    /// Valid escapes still decode, including one at the very end.
    #[test]
    fn a_valid_percent_escape_still_decodes() {
        assert_eq!(url_decode("%2E"), ".");
        assert_eq!(url_decode("1%2E2%2E3"), "1.2.3");
        // Mixed case hex is legal. `%AB` alone is a bare byte that is not valid
        // UTF-8, so the lossy conversion replaces it - what matters here is
        // that the escape was CONSUMED, not that the byte was a character.
        assert_eq!(url_decode("%2e"), ".");
        assert_eq!(url_decode("%AB"), "\u{fffd}", "a lone byte is not UTF-8");
        // A literal '%' followed by a real escape: only the escape is consumed.
        assert_eq!(url_decode("a%25b"), "a%b");
        assert_eq!(url_decode("%25%41"), "%A");
    }

    /// The `transfer-syntax` value ends at the next `;`, because the parameters
    /// are split on it. A trailing `q=` therefore cannot leak into the UID.
    #[test]
    fn a_quality_parameter_does_not_leak_into_the_transfer_syntax() {
        assert_eq!(
            extract_transfer_syntax_from_accept_str(Some(
                "multipart/related; type=\"application/dicom\"; \
                 transfer-syntax=1.2.840.10008.1.2.4.80; q=0.9"
            )),
            Some("1.2.840.10008.1.2.4.80".to_string())
        );
        // Parameter first, no `q=`.
        assert_eq!(
            extract_transfer_syntax_from_accept_str(Some(
                "multipart/related; transfer-syntax=1.2.840.10008.1.2.4.80"
            )),
            Some("1.2.840.10008.1.2.4.80".to_string())
        );
        // Quoted value, spaced parameters.
        assert_eq!(
            extract_transfer_syntax_from_accept_str(Some(
                "multipart/related ; type=application/dicom ; transfer-syntax=\"1.2.840.10008.1.2.1\""
            )),
            Some("1.2.840.10008.1.2.1".to_string())
        );
        assert_eq!(extract_transfer_syntax_from_accept_str(None), None);
        assert_eq!(
            extract_transfer_syntax_from_accept_str(Some("multipart/related")),
            None
        );
    }

    /// A multi-range Accept header: only the FIRST range carrying the
    /// parameter wins, and a later `application/dicom+json` range is ignored.
    #[test]
    fn multi_range_accept_parses_the_transfer_syntax() {
        assert_eq!(
            extract_transfer_syntax_from_accept_str(Some(
                "multipart/related; type=\"application/dicom\"; \
                 transfer-syntax=1.2.840.10008.1.2.4.80, application/dicom+json; q=0.9"
            )),
            Some("1.2.840.10008.1.2.4.80".to_string())
        );
    }

    /// Parameter names are case-insensitive and may be spaced around '='.
    #[test]
    fn transfer_syntax_parameter_is_case_insensitive_and_spaced() {
        assert_eq!(
            extract_transfer_syntax_from_accept_str(Some(
                "multipart/related; Transfer-Syntax = 1.2.840.10008.1.2.1"
            )),
            Some("1.2.840.10008.1.2.1".to_string())
        );
    }

    /// `q` decides which range wins, not the order they are listed in.
    ///
    /// PS3.18 10.4.3.1 requires a representation the client finds acceptable
    /// and RFC 9110 12.5.1 makes `q` that preference. A client asking for
    /// JPEG Baseline first at `q=0` and Explicit VR LE second at `q=1` wants
    /// Explicit VR LE; the previous implementation returned the first range
    /// carrying the parameter, i.e. the one the client refused.
    #[test]
    fn the_highest_weighted_transfer_syntax_wins() {
        assert_eq!(
            extract_transfer_syntax_from_accept_str(Some(
                "multipart/related; type=\"application/dicom\"; \
                 transfer-syntax=1.2.840.10008.1.2.4.50; q=0, \
                 multipart/related; type=\"application/dicom\"; \
                 transfer-syntax=1.2.840.10008.1.2.1; q=1"
            )),
            Some("1.2.840.10008.1.2.1".to_string()),
            "the q=1 range beats the q=0 range it follows"
        );
        // Same header with the weights swapped must select the other UID, so
        // the choice really follows `q` and is not a fixed position.
        assert_eq!(
            extract_transfer_syntax_from_accept_str(Some(
                "multipart/related; type=\"application/dicom\"; \
                 transfer-syntax=1.2.840.10008.1.2.4.50; q=1, \
                 multipart/related; type=\"application/dicom\"; \
                 transfer-syntax=1.2.840.10008.1.2.1; q=0"
            )),
            Some("1.2.840.10008.1.2.4.50".to_string())
        );
    }

    /// `q=0` alone means "not acceptable" (RFC 9110 12.5.1), so no syntax is
    /// selected and the caller falls back to the stored transfer syntax.
    #[test]
    fn a_zero_weighted_transfer_syntax_is_not_selected() {
        assert_eq!(
            extract_transfer_syntax_from_accept_str(Some(
                "multipart/related; type=\"application/dicom\"; \
                 transfer-syntax=1.2.840.10008.1.2.4.80; q=0"
            )),
            None
        );
    }

    /// Equal weights keep the client's own order (the first listed wins), so
    /// the selection is deterministic.
    #[test]
    fn equal_weights_keep_the_listed_order() {
        assert_eq!(
            extract_transfer_syntax_from_accept_str(Some(
                "multipart/related; transfer-syntax=1.2.840.10008.1.2.1; q=0.5, \
                 multipart/related; transfer-syntax=1.2.840.10008.1.2.4.80; q=0.5"
            )),
            Some("1.2.840.10008.1.2.1".to_string())
        );
    }

    /// An unparsable `q` drops that range instead of promoting it to 1.0.
    #[test]
    fn a_malformed_weight_drops_the_range() {
        assert_eq!(
            extract_transfer_syntax_from_accept_str(Some(
                "multipart/related; transfer-syntax=1.2.840.10008.1.2.4.80; q=5, \
                 multipart/related; transfer-syntax=1.2.840.10008.1.2.1; q=0.1"
            )),
            Some("1.2.840.10008.1.2.1".to_string()),
            "the q=5 range is invalid and must not outrank the q=0.1 range"
        );
    }
}

/// Legacy WADO-URI query parameters per PS3.18 Section 9.
///
/// Thin wrapper over `wado_instance` - validates parameters and delegates
/// retrieval.
///
/// # Additional parameters (PS3.18 Section 9.3)
/// - `contentType`: requested media type (default: `application/dicom`)
///   Anything other than `application/dicom` -> 406 (G-03)
/// - `transferSyntax`: requested transfer syntax UID
///   Non-matching -> 406 (G-03, G-05 documented limitation)
/// - `rows`/`columns`/`imageQuality` -> 406 (image rendering not supported) (G-11)
/// - `frameNumber`: 1 for single-frame OK, else 406 (G-11)
/// - `annotation` -> 406 (G-11)
/// - `charset`: validate against instance charset (G-11)
///
/// Returns 400 if `requestType != "WADO"`.
#[derive(Debug)]
pub struct WadoUriQuery {
    pub request_type: String,
    pub study_uid: String,
    pub series_uid: String,
    pub object_uid: String,
    pub content_type: Option<String>,
    pub transfer_syntax: Option<String>,
    pub rows: Option<String>,
    pub columns: Option<String>,
    pub image_quality: Option<String>,
    pub frame_number: Option<String>,
    pub annotation: Option<String>,
    pub charset: Option<String>,
}

impl WadoUriQuery {
    /// Parse from a query string, mirroring the previous `serde_urlencoded`
    /// semantics: the four UID/requestType fields are required (missing ->
    /// `None` renders as a 400 "bad query"); everything else is optional.
    pub fn parse(qs: &str) -> Option<Self> {
        let pairs = parse_query_string(qs);
        let req = |k: &str| get(&pairs, k).map(str::to_string);
        Some(WadoUriQuery {
            request_type: req("requestType")?,
            study_uid: req("studyUID")?,
            series_uid: req("seriesUID")?,
            object_uid: req("objectUID")?,
            content_type: get(&pairs, "contentType").map(str::to_string),
            transfer_syntax: get(&pairs, "transferSyntax").map(str::to_string),
            rows: get(&pairs, "rows").map(str::to_string),
            columns: get(&pairs, "columns").map(str::to_string),
            image_quality: get(&pairs, "imageQuality").map(str::to_string),
            frame_number: get(&pairs, "frameNumber").map(str::to_string),
            annotation: get(&pairs, "annotation").map(str::to_string),
            charset: get(&pairs, "charset").map(str::to_string),
        })
    }
}

#[cfg(test)]
mod viewport_tests {
    use super::{
        DEFAULT_THUMBNAIL_VIEWPORT, MAX_THUMBNAIL_VIEWPORT, ThumbnailViewport, ViewportError,
    };

    /// The two sizes the field actually asks for: OHIF 64, Weasis 256
    /// (PS3.18 8.3.5.1.3: `viewport=vw,vh`).
    #[test]
    fn a_two_integer_viewport_parses() {
        assert_eq!(
            ThumbnailViewport::parse("64,64"),
            Ok(ThumbnailViewport {
                width: 64,
                height: 64
            })
        );
        assert_eq!(
            ThumbnailViewport::parse("256,256"),
            Ok(ThumbnailViewport {
                width: 256,
                height: 256
            })
        );
        // A non-square viewport is legal: it is a bounding box, not a
        // requirement that the image be square.
        assert_eq!(
            ThumbnailViewport::parse("300,200"),
            Ok(ThumbnailViewport {
                width: 300,
                height: 200
            })
        );
    }

    /// PS3.18 8.3.5.1.3: an ill-formed viewport on a Thumbnail is a 400.
    /// Covers non-numeric, zero, negative, empty and missing components.
    #[test]
    fn an_ill_formed_viewport_is_refused() {
        for value in [
            "abc",     // not numeric
            "64",      // only one component
            "64,",     // empty height
            ",64",     // empty width
            "",        // empty value
            "0,0",     // zero is not a positive integer
            "64,0",    // one zero axis is still ill-formed
            "0,64",    //
            "-64,64",  // a negative dimension
            "64.5,64", // a fractional dimension
            " 64,64",  // leading whitespace is not a valid integer token
        ] {
            assert_eq!(
                ThumbnailViewport::parse(value),
                Err(ViewportError::IllFormed),
                "value {value:?} must be ill-formed"
            );
        }
    }

    /// PS3.18 8.3.5.1.3: a Thumbnail accepts ONLY `vw,vh`, so the four-value
    /// cropping form is a 400 rather than a silently ignored suffix.
    #[test]
    fn the_four_value_crop_form_is_refused_on_a_thumbnail() {
        assert_eq!(
            ThumbnailViewport::parse("512,512,0,0,512,512"),
            Err(ViewportError::IllFormed)
        );
        assert_eq!(
            ThumbnailViewport::parse("512,512,0,0"),
            Err(ViewportError::IllFormed)
        );
    }

    /// A well-formed viewport above the supported maximum is "not supported",
    /// which 8.3.5.1.3 also makes a 400 - and it is reported distinctly so the
    /// message can name the bound.
    #[test]
    fn an_oversized_viewport_is_not_supported() {
        let over = MAX_THUMBNAIL_VIEWPORT + 1;
        assert_eq!(
            ThumbnailViewport::parse(&format!("{over},{over}")),
            Err(ViewportError::NotSupported)
        );
        // Exactly the maximum is still supported (the bound is inclusive).
        assert!(
            ThumbnailViewport::parse(&format!(
                "{MAX_THUMBNAIL_VIEWPORT},{MAX_THUMBNAIL_VIEWPORT}"
            ))
            .is_ok()
        );
    }

    /// The documented default is what an absent parameter gets, and it stays
    /// 64 so OHIF's existing requests are unchanged (PS3.18 8.3.5.2 leaves
    /// the choice to the origin server).
    #[test]
    fn the_documented_default_is_sixty_four() {
        assert_eq!(DEFAULT_THUMBNAIL_VIEWPORT, 64);
    }

    /// The viewport is a bounding box: the source keeps its aspect ratio and
    /// the longer axis lands exactly on the box (8.3.5.1.3 "as large as
    /// possible ... without distorting").
    #[test]
    fn fitting_preserves_the_aspect_ratio() {
        let square = ThumbnailViewport {
            width: 256,
            height: 256,
        };
        // A 512x256 source in a square box -> 256x128.
        assert_eq!(square.fit(512, 256), (256, 128));
        // A 256x512 source -> 128x256.
        assert_eq!(square.fit(256, 512), (128, 256));
        // An already-square source fills the box.
        assert_eq!(square.fit(512, 512), (256, 256));
        // Scaling UP is required by the same clause ("as large as possible").
        assert_eq!(square.fit(64, 64), (256, 256));

        let wide = ThumbnailViewport {
            width: 300,
            height: 200,
        };
        // A 600x600 source in a 300x200 box -> 200x200 (height-limited).
        assert_eq!(wide.fit(600, 600), (200, 200));
        // A very wide source never collapses the short axis to zero.
        let (w, h) = wide.fit(10_000, 10);
        assert!(w >= 1 && h >= 1, "got {w}x{h}");
        assert!(w <= 300 && h <= 200, "got {w}x{h} exceeds the box");
    }
}
