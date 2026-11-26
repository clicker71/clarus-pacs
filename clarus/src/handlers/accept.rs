// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Accept-header negotiation (RFC 9110 12.5.1).
//!
//! One parser for every route that has to choose a representation. The routes
//! used to each carry their own substring test - `accept.contains("image/png")`
//! and friends - and every one of those tests had the same three failures:
//!
//! - **Parameters looked like types.** `application/dicom+json;
//!   example="multipart/related"` matched `multipart/related` because the
//!   parameter VALUE was searched, not the media range
//!   (PS3.18 8.7.3.3.2 puts arbitrary parameters on a media range).
//! - **`q=0` was a match.** `image/png;q=0, image/jpeg;q=1` selects PNG under
//!   a contains-test, though RFC 9110 12.5.1 says `q=0` means "not
//!   acceptable".
//! - **Order was ignored.** The first listed type won, so a client that
//!   ranked JPEG above PNG got PNG whenever PNG appeared earlier.
//!
//! This module answers the only question the routes actually ask: *of the
//! representations I can produce, which one does the client prefer?* It never
//! allocates, because it runs on every retrieve GET (the ap101 gate in
//! `tests/ap101_accept_negotiation.rs` pins `classify_retrieve_accept`).
//!
//! # What this deliberately does not do
//!
//! It does not rank by media-range specificity across families (RFC 9110 12.5.1
//! orders `text/plain` above `text/*` above `*/*` for the SAME offered type).
//! The routes here always choose between a small fixed set of concrete types,
//! and the specificity rule collapses to "a concrete range beats a wildcard",
//! which [`rank`] implements directly.

#![forbid(unsafe_code)]

/// One media range from an Accept header, borrowed from the header value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MediaRange<'a> {
    /// The media type without parameters, trimmed of surrounding whitespace.
    pub media_type: &'a str,
    /// The `type` parameter of a `multipart/related` range, if present (quotes
    /// removed). This is the DICOMweb spelling of "what the parts contain",
    /// so `multipart/related; type="application/dicom+json"` is a request for
    /// the JSON parts, not for multipart in the abstract (PS3.18 8.7.3.3.2).
    pub part_type: Option<&'a str>,
    /// The `q` weight, 0..=1000 in thousandths. Defaults to 1.0 (`1000`).
    pub weight: u16,
}

impl MediaRange<'_> {
    /// Whether this range selects `media_type` (case-insensitive).
    ///
    /// A range's parameters do NOT make it a different media type, so
    /// `application/dicom+json; q=0.5` still selects `application/dicom+json`.
    /// The converse is what used to break: the parameter VALUES must never be
    /// searched for another media type's name.
    ///
    /// The one parameter that names a representation is `type=` on a
    /// `multipart/related` range: PS3.18 8.7.3.3.2 puts the DICOMweb media type
    /// of the PARTS there, so `multipart/related; type="application/dicom+xml"`
    /// is a request for the XML representation and must satisfy a caller
    /// offering `application/dicom+xml`.
    #[must_use]
    pub fn selects(&self, media_type: &str) -> bool {
        if self.media_type.eq_ignore_ascii_case(media_type) {
            return true;
        }
        self.part_type
            .is_some_and(|p| p.eq_ignore_ascii_case(media_type))
    }

    /// Whether this range is a wildcard covering `media_type`.
    #[must_use]
    pub fn covers(&self, media_type: &str) -> bool {
        if self.media_type == "*/*" {
            return true;
        }
        // `type/*` covers every subtype of that type. Compare the type prefix.
        if let Some(type_prefix) = self.media_type.strip_suffix("/*") {
            return media_type
                .split('/')
                .next()
                .is_some_and(|t| t.eq_ignore_ascii_case(type_prefix));
        }
        false
    }
}

/// Parse one `q=` value into thousandths. Returns `None` for a malformed
/// weight, which RFC 9110 12.4.2 says makes the whole range invalid - the
/// caller drops the range rather than treating it as 1.0.
fn parse_weight(raw: &str) -> Option<u16> {
    let raw = raw.trim();
    // The grammar is `0` [ "." 0*3DIGIT ] or `1` [ "." 0*3ZERO ].
    let (int_part, frac_part) = match raw.split_once('.') {
        Some((i, f)) => (i, f),
        None => (raw, ""),
    };
    if frac_part.len() > 3 || !frac_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut thousandths: u16 = match int_part {
        "0" => 0,
        "1" => 1000,
        _ => return None,
    };
    // Only a fractional part of exactly three digits maps to thousandths; pad
    // shorter ones so `.5` means 500, as RFC 9110 12.4.2 specifies.
    if !frac_part.is_empty() {
        if int_part == "1" {
            // `1.x` is only valid when every fractional digit is zero.
            if !frac_part.bytes().all(|b| b == b'0') {
                return None;
            }
        } else {
            let mut acc: u16 = 0;
            for (i, b) in frac_part.bytes().enumerate() {
                let d = u16::from(b - b'0');
                acc += d * 10u16.pow(2 - i as u32);
            }
            thousandths = acc;
        }
    }
    Some(thousandths)
}

/// [`parse_weight`] for callers outside this module that parse their own
/// parameter lists but must apply the same RFC 9110 12.4.2 grammar.
#[must_use]
pub fn parse_weight_pub(raw: &str) -> Option<u16> {
    parse_weight(raw)
}

/// The parsed Accept header: a fixed-capacity list of media ranges.
///
/// Capacity is bounded because a header with more than [`Self::MAX_RANGES`]
/// ranges is answered on its first ranges only - RFC 9110 does not require an
/// origin to consider an unbounded list, and this runs on the hot path.
#[derive(Debug, Clone, Copy)]
pub struct AcceptList<'a> {
    ranges: [Option<MediaRange<'a>>; MAX_RANGES],
    len: usize,
}

/// Most media ranges considered. A real client sends one to four; the cap
/// keeps the value type `Copy` and the parse stack-only.
pub const MAX_RANGES: usize = 16;

impl<'a> AcceptList<'a> {
    /// Most media ranges considered (see [`MAX_RANGES`]).
    pub const MAX: usize = MAX_RANGES;

    /// Parse an Accept header value. An absent or empty header yields an empty
    /// list, which every selector reads as "no preference".
    #[must_use]
    pub fn parse(accept: Option<&'a str>) -> Self {
        let mut list = Self {
            ranges: [None; MAX_RANGES],
            len: 0,
        };
        let Some(accept) = accept else {
            return list;
        };
        for range in accept.split(',') {
            if list.len == MAX_RANGES {
                break;
            }
            if let Some(parsed) = parse_range(range) {
                list.ranges[list.len] = Some(parsed);
                list.len += 1;
            }
        }
        list
    }

    /// Whether the list is empty (absent header or nothing parseable).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The ranges, in header order.
    pub fn iter(&self) -> impl Iterator<Item = MediaRange<'a>> + '_ {
        self.ranges[..self.len].iter().filter_map(|r| *r)
    }

    /// The weight the client assigned to `media_type`, and whether it was named
    /// as a CONCRETE range (as opposed to only covered by a wildcard).
    ///
    /// `q=0` reports `Some(0)`, which callers must treat as "not acceptable" -
    /// returning `None` instead would make an excluded type indistinguishable
    /// from an unmentioned one.
    #[must_use]
    pub fn preference(&self, media_type: &str) -> (Option<u16>, bool) {
        let mut best: Option<(u16, bool)> = None;
        for range in self.iter() {
            let (weight, concrete) = if range.selects(media_type) {
                (range.weight, true)
            } else if range.covers(media_type) {
                // A wildcard names no concrete type, so it only ever supplies
                // a fallback weight at lower specificity.
                (range.weight, false)
            } else {
                continue;
            };
            // A concrete range always outranks a wildcard for the same type
            // (RFC 9110 12.5.1: the more specific range wins). Within the same
            // specificity, the highest weight wins.
            best = Some(match best {
                None => (weight, concrete),
                Some((w, c)) => {
                    if concrete && !c {
                        (weight, true)
                    } else if c && !concrete {
                        (w, c)
                    } else if weight > w {
                        (weight, concrete)
                    } else {
                        (w, c)
                    }
                }
            });
        }
        match best {
            Some((w, concrete)) => (Some(w), concrete),
            None => (None, false),
        }
    }

    /// Whether the client explicitly excluded `media_type` (`q=0`).
    #[must_use]
    pub fn excluded(&self, media_type: &str) -> bool {
        matches!(self.preference(media_type), (Some(0), true))
    }

    /// Whether any range names `media_type` with a nonzero weight.
    #[must_use]
    pub fn accepts(&self, media_type: &str) -> bool {
        matches!(self.preference(media_type), (Some(w), _) if w > 0)
    }

    /// The highest nonzero weight among the wildcard ranges (`*/*`,
    /// `type/*`), or `None` when the header carries no wildcard.
    ///
    /// Callers use this for the "client will take whatever you have" case
    /// (RFC 9110 12.5.1): a wildcard is not a request for a named type, but
    /// it is still a positive statement of preference.
    #[must_use]
    pub fn wildcard_weight(&self) -> Option<u16> {
        let mut best = None;
        for range in self.iter() {
            let wildcard = range.media_type == "*/*" || range.media_type.ends_with("/*");
            if wildcard && range.weight > 0 && best.is_none_or(|w| range.weight > w) {
                best = Some(range.weight);
            }
        }
        best
    }
}

/// Parse one comma-separated range: `type/subtype *( ";" param )`.
fn parse_range(raw: &str) -> Option<MediaRange<'_>> {
    let mut parts = raw.split(';');
    let media_type = parts.next()?.trim();
    if media_type.is_empty() {
        return None;
    }
    let mut part_type = None;
    let mut weight: u16 = 1000;
    for param in parts {
        // A parameter is `name=value`; a bare token is malformed and RFC 9110
        // 12.5.1 does not define it, so the range is dropped rather than
        // silently treated as having no parameters.
        let (name, value) = param.split_once('=')?;
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("q") {
            weight = parse_weight(value)?;
        } else if name.eq_ignore_ascii_case("type") {
            // Quoted per PS3.18 8.7.3.3.2 (`type="application/dicom"`), but an
            // unquoted spelling is widely sent, so both are accepted.
            part_type = Some(value.trim_matches('"').trim());
        }
    }
    Some(MediaRange {
        media_type,
        part_type,
        weight,
    })
}

/// Pick the client's preferred type from `offered`, in order of the client's
/// preference and then of `offered`.
///
/// Returns `None` when the client excluded every offered type, which the
/// caller answers with 406 (RFC 9110 15.5.7, PS3.18 10.4.3.1). An absent
/// header selects `offered[0]`, the resource's default.
#[must_use]
pub fn select<'a>(accept: Option<&str>, offered: &[&'a str]) -> Option<&'a str> {
    let list = AcceptList::parse(accept);
    let mut best: Option<(&'a str, u16)> = None;
    if list.is_empty() {
        return offered.first().copied();
    }
    for candidate in offered {
        let (weight, _concrete) = list.preference(candidate);
        let Some(weight) = weight else { continue };
        if weight == 0 {
            continue;
        }
        // `>` not `>=`: on an equal weight the EARLIER offered type wins, so
        // the caller's preference order is the tiebreak and the answer is
        // deterministic.
        if best.is_none_or(|(_, w)| weight > w) {
            best = Some((candidate, weight));
        }
    }
    best.map(|(m, _)| m)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFERED: [&str; 3] = ["image/png", "image/jpeg", "image/gif"];

    /// `q=0` means "not acceptable", not "acceptable".
    ///
    /// A contains/substring test (what the routes used) saw PNG first and
    /// selected it, so a client that explicitly excluded PNG got PNG.
    #[test]
    fn a_zero_weight_excludes_a_type() {
        assert_eq!(
            select(Some("image/png;q=0, image/jpeg;q=1"), &OFFERED),
            Some("image/jpeg"),
            "PNG is excluded by q=0"
        );
        assert_eq!(
            select(Some("image/gif;q=0"), &OFFERED),
            None,
            "every named type is excluded"
        );
    }

    /// The highest weight wins regardless of header order.
    #[test]
    fn the_highest_weight_wins_not_the_first_listed() {
        assert_eq!(
            select(Some("image/png;q=0.2, image/jpeg;q=0.9"), &OFFERED),
            Some("image/jpeg")
        );
        // Equal weights: the earlier OFFERED type wins, so the choice is
        // deterministic and not a function of header order.
        assert_eq!(
            select(Some("image/gif;q=0.5, image/png;q=0.5"), &OFFERED),
            Some("image/png")
        );
    }

    /// A media PARAMETER is not a media type.
    ///
    /// `application/dicom+json; example="multipart/related"` asked for JSON
    /// with a parameter that merely spells another type; a substring search
    /// found `multipart/related` inside the parameter value and chose
    /// multipart.
    #[test]
    fn a_parameter_value_is_not_a_media_type() {
        let accept = r#"application/dicom+json; example="multipart/related""#;
        let list = AcceptList::parse(Some(accept));
        assert!(list.accepts("application/dicom+json"), "the range is JSON");
        assert!(
            !list.accepts("multipart/related"),
            "the parameter value must not be read as a media range"
        );
        let offered = ["application/dicom+json", "multipart/related"];
        assert_eq!(
            select(Some(accept), &offered),
            Some("application/dicom+json")
        );
    }

    /// A concrete range outranks a wildcard FOR THE SAME TYPE.
    ///
    /// RFC 9110 12.5.1 applies the most specific matching range, so
    /// `*/*;q=0.1, image/jpeg;q=1` makes JPEG acceptable at 1.0 even though
    /// the wildcard gives everything else 0.1. The wildcard's weight is NOT
    /// inherited by a type that has its own range.
    #[test]
    fn a_concrete_range_overrides_the_wildcard_for_its_own_type() {
        let list = AcceptList::parse(Some("*/*;q=0.1, image/jpeg;q=1"));
        assert_eq!(
            list.preference("image/jpeg").0,
            Some(1000),
            "the concrete range supplies JPEG's weight"
        );
        assert_eq!(
            list.preference("image/png").0,
            Some(100),
            "an unmentioned type falls back to the wildcard"
        );
        // And the selector follows those weights.
        assert_eq!(
            select(Some("*/*;q=0.1, image/jpeg;q=1"), &OFFERED),
            Some("image/jpeg"),
            "JPEG at 1.0 beats PNG's wildcard 0.1"
        );
    }

    /// A concrete range does NOT outrank a HIGHER-weighted wildcard for OTHER
    /// types: `*/*;q=1, image/jpeg;q=0.1` still prefers PNG at 1.0.
    #[test]
    fn a_lower_weighted_concrete_range_does_not_win_the_whole_offer() {
        assert_eq!(
            select(Some("*/*;q=1, image/jpeg;q=0.1"), &OFFERED),
            Some("image/png"),
            "PNG inherits the wildcard's 1.0 and outranks JPEG's 0.1"
        );
    }

    /// Media types and parameter names are case-insensitive.
    #[test]
    fn case_is_ignored() {
        assert_eq!(
            select(Some("IMAGE/PNG"), &OFFERED),
            Some("image/png"),
            "media types compare case-insensitively"
        );
        assert_eq!(
            select(Some("image/jpeg;Q=0, image/gif"), &OFFERED),
            Some("image/gif"),
            "the q parameter name is case-insensitive"
        );
        let list = AcceptList::parse(Some(r#"multipart/related; TYPE="Application/Dicom""#));
        let range = list.iter().next().expect("one range");
        assert_eq!(range.part_type, Some("Application/Dicom"));
    }

    /// An absent header selects the resource default; an empty one does too.
    #[test]
    fn absent_header_is_no_preference() {
        assert_eq!(select(None, &OFFERED), Some("image/png"));
        assert_eq!(select(Some(""), &OFFERED), Some("image/png"));
        assert!(AcceptList::parse(None).is_empty());
    }

    /// Weight parsing follows RFC 9110 12.4.2.
    #[test]
    fn weights_parse_per_the_grammar() {
        assert_eq!(parse_weight("0"), Some(0));
        assert_eq!(parse_weight("1"), Some(1000));
        assert_eq!(parse_weight("0.5"), Some(500));
        assert_eq!(parse_weight("0.05"), Some(50));
        assert_eq!(parse_weight("0.005"), Some(5));
        assert_eq!(parse_weight("1.000"), Some(1000));
        // Malformed: more than three decimals, `1.x` with a nonzero x, a
        // leading digit other than 0/1, a bare '.'.
        assert_eq!(parse_weight("0.0001"), None);
        assert_eq!(parse_weight("1.5"), None);
        assert_eq!(parse_weight("2"), None);
        assert_eq!(parse_weight("."), None);
    }

    /// A malformed range is dropped, not silently accepted at full weight.
    #[test]
    fn a_malformed_range_is_dropped() {
        // A bare token with no '=' is not a parameter.
        let list = AcceptList::parse(Some("image/png; garbage"));
        assert!(list.is_empty(), "the malformed range must be dropped");
        // A malformed q drops the range rather than defaulting to 1.0.
        let list = AcceptList::parse(Some("image/png;q=5"));
        assert!(list.is_empty());
    }

    /// `q=0` on a wildcard excludes the whole family.
    #[test]
    fn an_excluding_wildcard_excludes_everything_it_covers() {
        assert_eq!(select(Some("*/*;q=0"), &OFFERED), None);
        assert_eq!(select(Some("image/*;q=0"), &OFFERED), None);
    }

    /// The range cap is enforced and the parser stays stack-only.
    #[test]
    fn the_range_cap_is_enforced() {
        let many = "image/png,".repeat(64);
        let list = AcceptList::parse(Some(&many));
        assert_eq!(list.len, MAX_RANGES);
    }
}
