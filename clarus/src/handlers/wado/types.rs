// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Query parameter types for WADO-RS and WADO-URI endpoints.
//!
//! the query-string parsers are hand-rolled (no `serde_urlencoded`),
//! eliminating the serde dependency from the WADO-URI path. The wire
//! semantics match the previous `serde_urlencoded` behavior: `+` decodes to a
//! space, `%XX` decodes to a byte, and unknown keys are ignored.

#![forbid(unsafe_code)]

/// URL-decode a percent-encoded query value (`%XX` and `+` -> space).
///
/// Precedent: the `url_decode` helper in `qido_pure.rs`.
pub fn url_decode(s: &str) -> String {
    let mut result = Vec::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().unwrap_or(b'0');
            let lo = chars.next().unwrap_or(b'0');
            if let Ok(decoded) =
                u8::from_str_radix(std::str::from_utf8(&[hi, lo]).unwrap_or("00"), 16)
            {
                result.push(decoded);
            }
        } else if b == b'+' {
            result.push(b' ');
        } else {
            result.push(b);
        }
    }
    // UTF-8 multi-byte sequences decode into BYTES, not Latin-1 scalars:
    // %D0%BC is "м", not "Ð¼".
    String::from_utf8_lossy(&result).into_owned()
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
pub fn extract_transfer_syntax_from_accept_str(accept: Option<&str>) -> Option<String> {
    let accept = accept?;
    for part in accept.split(';') {
        let part = part.trim();
        if let Some(ts) = part.strip_prefix("transfer-syntax=") {
            return Some(ts.trim_matches('"').trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
