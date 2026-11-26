// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! VR width classes and the Explicit VR element header rules.
//!
//! PS3.5 6.2 Table 6.2-1 splits the VRs into two header shapes: the
//! twenty-seven short-form VRs carry a 2-byte Value Length straight after the
//! VR, and the rest carry two reserved bytes and a 4-byte Value Length. Which
//! table a VR belongs to is a property of the VR alone - it does not depend on
//! the byte order, the transfer syntax, or the tag.
//!
//! This module exists because that one fact was restated as a hand-written
//! `matches!` in more than twenty places across `codec`, and the lists drifted:
//! several were missing `OV` (and some `SV`), so a dataset containing an Other
//! 64-bit Very Long or a Signed 64-bit Very Long element desynchronised the
//! walk at that element and every attribute after it was read from the wrong
//! offset. A walker that knows the class gets the length from the right place
//! by construction.
//!
//! The byte order is a parameter, never a constant: PS3.5 7.1.2 says the tag
//! and every length field follow the dataset's transfer syntax, while the VR
//! letters themselves are a pair of ASCII bytes in both orders.

#![forbid(unsafe_code)]

/// PS3.5 6.2 Table 6.2-1: the VRs whose header is
/// `VR(2) + reserved(2) + ValueLength(4)`.
///
/// `is_long_form_vr` classifies by the SHORT set, not by this one, because an
/// unrecognized VR is long form by default; this list is the counterpart that
/// states which VRs the standard names, and the partition test holds it to the
/// same table element for element.
#[cfg_attr(not(test), allow(dead_code))]
const LONG_FORM_VRS: [&str; 13] = [
    "OB", "OD", "OF", "OL", "OV", "OW", "SQ", "SV", "UC", "UN", "UR", "UT", "UV",
];

/// PS3.5 6.2 Table 6.2-1, the VRs with a 2-byte Value Length.
///
/// Kept beside the long-form list so both classes are stated in one place. It
/// is also the set `is_long_form_vr` tests against, which is why a VR the
/// standard adds later cannot silently join the short form: an unrecognized
/// pair is long form by default (`standard/ps3.5/unrecognized-vr-uses-long-header-6.2.json`).
#[cfg_attr(not(test), allow(dead_code))]
const SHORT_FORM_VRS: [&str; 21] = [
    "AE", "AS", "AT", "CS", "DA", "DS", "DT", "FD", "FL", "IS", "LO", "LT", "PN", "SH", "SL", "SS",
    "ST", "TM", "UI", "UL", "US",
];

/// Whether `vr` uses the long-form element header (PS3.5 6.2 Table 6.2-1).
///
/// The test is membership in the SHORT-form set, not in the long-form set:
/// PS3.5 6.2 states that "all new VRs defined in future versions of DICOM
/// shall be of the same Data Element Structure as defined in Section 7.1.2
/// with reserved bytes after the VR and a 32-bit unsigned integer VL", so a
/// well-formed VR pair outside Table 6.2-1 must be read with the long header.
/// Testing for a known-long VR instead sent every future VR down the short
/// branch, where its two reserved bytes became a 16-bit Value Length and the
/// walk desynchronised at that element.
///
/// A pair that is not two alphabetic ASCII characters is not a VR at all - it
/// is either a truncated header or an Implicit VR value being read as one. Those
/// stay short form so the caller's length read stays inside the four bytes it
/// has already validated; treating them as long would ask for eight.
pub(crate) fn is_long_form_vr(vr: &[u8]) -> bool {
    let Ok(vr) = std::str::from_utf8(vr) else {
        return false;
    };
    // Exactly two characters: a single byte is a truncated header, and the
    // length test comes before the character-class test so `"U"` does not slip
    // through as a one-letter VR.
    if vr.len() != 2 || !vr.bytes().all(|b| b.is_ascii_uppercase()) {
        return false;
    }
    !SHORT_FORM_VRS.contains(&vr)
}

/// [`is_long_form_vr`] for a VR that is already a `&str`.
#[inline]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn is_long_form_vr_str(vr: &str) -> bool {
    is_long_form_vr(vr.as_bytes())
}

/// Read a `u16` in the dataset's byte order.
///
/// Returns `None` rather than indexing when the buffer ends first: every
/// caller is a header walker, and the last element of a truncated file leaves
/// its length field half present. An out-of-bounds index there is a panic on
/// input the decoder is expected to refuse.
#[inline]
pub(crate) fn read_u16_syntax(data: &[u8], at: usize, be: bool) -> Option<u16> {
    let pair: [u8; 2] = data.get(at..at + 2)?.try_into().ok()?;
    Some(if be {
        u16::from_be_bytes(pair)
    } else {
        u16::from_le_bytes(pair)
    })
}

/// Read a `u32` in the dataset's byte order.
///
/// See [`read_u16_syntax`] on why this is fallible.
#[inline]
pub(crate) fn read_u32_syntax(data: &[u8], at: usize, be: bool) -> Option<u32> {
    let quad: [u8; 4] = data.get(at..at + 4)?.try_into().ok()?;
    Some(if be {
        u32::from_be_bytes(quad)
    } else {
        u32::from_le_bytes(quad)
    })
}

/// The location of an Explicit VR element's value inside the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExplicitValue {
    /// Absolute offset of the first value byte, i.e. of `TAG(4) + header`.
    /// Safe to index `data` with directly.
    pub(crate) start: usize,
    /// The Value Length exactly as the element declares it, which PS3.5 6.2
    /// makes an EVEN count: the pad byte is inside this number, not after it
    /// (`standard/ps3.5/vr-width-classes-6.2-1.json`, padIsInsideTheDeclaredLength).
    /// A caller that finds an odd value here is looking at a malformed
    /// element, because the standard does not permit an odd Value Length. The
    /// value is NOT rounded up and the pad is NOT added separately - adding it
    /// again skips one byte into the next element.
    pub(crate) len: usize,
    /// Size in bytes of the element header, measured from the first VR byte:
    /// 8 for `VR(2) + reserved(2) + ValueLength(4)` and 4 for
    /// `VR(2) + ValueLength(2)`. This is a WIDTH and not an offset, so the
    /// element's tag begins at `start - header_len - 4`, and the VR bytes at
    /// `start - header_len`. Reading it as an absolute position addresses the
    /// wrong byte of the buffer.
    pub(crate) header_len: usize,
}

/// Parse the header of the Explicit VR element whose VR starts at `vr_at`.
///
/// `vr_at` points at the two VR letters, i.e. `tag_at + 4`. Returns `None`
/// when the header or the value runs past the buffer, which the callers treat
/// as "the walk cannot continue" - never as a zero length.
pub(crate) fn explicit_value(data: &[u8], vr_at: usize, be: bool) -> Option<ExplicitValue> {
    let vr = data.get(vr_at..vr_at + 2)?;
    if is_long_form_vr(vr) {
        // VR(2) + reserved(2) + ValueLength(4)
        let len_at = vr_at + 4;
        let len = read_u32_syntax(data, len_at, be)? as usize;
        let start = len_at + 4;
        if len != UNDEFINED_LENGTH && len > data.len().saturating_sub(start) {
            return None;
        }
        Some(ExplicitValue {
            start,
            len,
            header_len: 8,
        })
    } else {
        // VR(2) + ValueLength(2). A 16-bit field cannot express the undefined
        // sentinel (PS3.5 7.5.1), so 0xFFFF here is a real value length.
        let len_at = vr_at + 2;
        let len = read_u16_syntax(data, len_at, be)? as usize;
        let start = len_at + 2;
        if len > data.len().saturating_sub(start) {
            return None;
        }
        Some(ExplicitValue {
            start,
            len,
            header_len: 4,
        })
    }
}

/// Read a `u16` from the first two bytes of an Explicit VR element's value.
///
/// The VR writers of the next batch read Rows/Columns/PlanarConfiguration
/// through this instead of re-deriving a header length; the codec-core tests
/// exercise it in the meantime.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn explicit_u16(data: &[u8], vr_at: usize, be: bool) -> Option<u16> {
    let v = explicit_value(data, vr_at, be)?;
    if v.len < 2 {
        return None;
    }
    read_u16_syntax(data, v.start, be)
}

/// The undefined-length sentinel (PS3.5 7.5.1), expressible only in the
/// 32-bit length form.
pub(crate) const UNDEFINED_LENGTH: usize = 0xFFFF_FFFF;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_width_classes_partition_ps35_table_6_2_1() {
        // PS3.5 6.2 Table 6.2-1 defines exactly these VRs, written out here
        // rather than read from the two arrays under test. Comparing the arrays
        // against each other and against the classifier that reads those same
        // arrays is circular: replacing "OV" with a second "OB" keeps both
        // lengths at their expected values and the partition still holds, so
        // the omission the test exists to catch goes unnoticed.
        //
        // `standard/ps3.5/vr-width-classes-6.2-1.json` carries this list as a
        // standard fact; it is spelled out here because the test's whole point
        // is to be independent of the implementation's copy.
        const LONG_FORM_TABLE: [&str; 13] = [
            "OB", "OD", "OF", "OL", "OV", "OW", "SQ", "SV", "UC", "UN", "UR", "UT", "UV",
        ];
        const SHORT_FORM_TABLE: [&str; 21] = [
            "AE", "AS", "AT", "CS", "DA", "DS", "DT", "FD", "FL", "IS", "LO", "LT", "PN", "SH",
            "SL", "SS", "ST", "TM", "UI", "UL", "US",
        ];

        // Twenty-one short-form VRs is what PS3.5 6.2 Table 6.2-1 lists, and the
        // module documentation says so; asserting the two class sizes pins the
        // count in both directions.
        assert_eq!(
            SHORT_FORM_TABLE.len(),
            21,
            "PS3.5 6.2 lists 21 short-form VRs"
        );
        assert_eq!(
            LONG_FORM_TABLE.len(),
            13,
            "PS3.5 6.2 lists 13 long-form VRs"
        );

        // The implementation's two arrays must match the independent ones
        // element for element, so a VR swapped for a duplicate is caught even
        // when both sizes and the partition still hold.
        assert_eq!(SHORT_FORM_VRS, SHORT_FORM_TABLE);
        assert_eq!(LONG_FORM_VRS, LONG_FORM_TABLE);

        for vr in LONG_FORM_TABLE {
            assert!(is_long_form_vr_str(vr), "{vr} must be long form");
        }
        for vr in SHORT_FORM_TABLE {
            assert!(!is_long_form_vr_str(vr), "{vr} must be short form");
        }

        // No VR may be in both lists, and the union must be exactly the 34 VRs
        // the standard defines - stated independently so a duplicated entry is
        // caught even when both sizes still add up.
        let mut all: Vec<&str> = LONG_FORM_TABLE
            .iter()
            .chain(SHORT_FORM_TABLE.iter())
            .copied()
            .collect();
        all.sort_unstable();
        let before = all.len();
        all.dedup();
        assert_eq!(before, all.len(), "a VR appears in both width classes");
        assert_eq!(all.len(), 34, "PS3.5 6.2 defines 34 VRs in total");
    }

    #[test]
    fn an_unrecognized_but_well_formed_vr_uses_the_long_header() {
        // PS3.5 6.2: every future VR is defined with reserved bytes and a
        // 32-bit Value Length, so "ZZ" - not in Table 6.2-1 - is long form.
        // Classifying it as short read the two reserved zero bytes as a
        // 16-bit Value Length and put every following element four bytes early.
        assert!(is_long_form_vr(b"ZZ"));
        assert!(is_long_form_vr(b"QQ"));

        // A conforming 12-byte header for it must therefore parse as long: the
        // length lives at vr_at + 4, not at vr_at + 2.
        let mut d = Vec::new();
        d.extend_from_slice(&0x0009u16.to_le_bytes());
        d.extend_from_slice(&0x0001u16.to_le_bytes());
        d.extend_from_slice(b"ZZ");
        d.extend_from_slice(&[0, 0]);
        d.extend_from_slice(&4u32.to_le_bytes());
        d.extend_from_slice(&[1, 2, 3, 4]);
        let v = explicit_value(&d, 4, false).expect("a long-form header");
        assert_eq!(v.header_len, 8);
        assert_eq!(v.len, 4);
        assert_eq!(v.start, 12);
    }

    #[test]
    fn a_pair_that_is_not_a_vr_stays_short_form() {
        // Not two uppercase ASCII letters: a truncated header, or an Implicit
        // VR value read as one. Treating these as long form asks for eight
        // bytes where only four remain, so they must stay short.
        assert!(!is_long_form_vr(b"\0\0"));
        assert!(!is_long_form_vr(b"\x01\x00"));
        assert!(!is_long_form_vr(b"us"));
        assert!(!is_long_form_vr(b"U"));
    }

    #[test]
    fn a_short_form_header_reads_a_two_byte_length() {
        // (0028,0010) US with a 2-byte value.
        let mut d = Vec::new();
        d.extend_from_slice(&0x0028u16.to_le_bytes());
        d.extend_from_slice(&0x0010u16.to_le_bytes());
        d.extend_from_slice(b"US");
        d.extend_from_slice(&2u16.to_le_bytes());
        d.extend_from_slice(&4u16.to_le_bytes());
        let v = explicit_value(&d, 4, false).expect("a valid short-form header");
        assert_eq!(v.header_len, 4);
        assert_eq!(v.len, 2);
        assert_eq!(explicit_u16(&d, 4, false), Some(4));
    }

    #[test]
    fn a_long_form_header_reads_the_four_byte_length_after_the_reserved_bytes() {
        // (7FE0,0001) OV - the VR the drifted lists were missing.
        let mut d = Vec::new();
        d.extend_from_slice(&0x7FE0u16.to_le_bytes());
        d.extend_from_slice(&0x0001u16.to_le_bytes());
        d.extend_from_slice(b"OV");
        d.extend_from_slice(&[0, 0]); // reserved
        d.extend_from_slice(&8u32.to_le_bytes());
        d.extend_from_slice(&[0u8; 8]);
        let v = explicit_value(&d, 4, false).expect("a valid long-form header");
        assert_eq!(v.header_len, 8);
        assert_eq!(v.len, 8);
        // tag(4) + VR(2) + reserved(2) + ValueLength(4).
        assert_eq!(v.start, 12);
    }

    #[test]
    fn a_big_endian_long_form_length_is_read_big_endian() {
        let mut d = Vec::new();
        d.extend_from_slice(&0x7FE0u16.to_be_bytes());
        d.extend_from_slice(&0x0001u16.to_be_bytes());
        d.extend_from_slice(b"OV");
        d.extend_from_slice(&[0, 0]);
        d.extend_from_slice(&8u32.to_be_bytes());
        d.extend_from_slice(&[0u8; 8]);
        let v = explicit_value(&d, 4, true).expect("valid");
        assert_eq!(v.len, 8, "not 0x08000000");
        assert_eq!(v.start, 12);
    }

    #[test]
    fn a_value_that_runs_past_the_buffer_is_not_a_short_read() {
        let mut d = Vec::new();
        d.extend_from_slice(&0x7FE0u16.to_le_bytes());
        d.extend_from_slice(&0x0001u16.to_le_bytes());
        d.extend_from_slice(b"OV");
        d.extend_from_slice(&[0, 0]);
        d.extend_from_slice(&64u32.to_le_bytes());
        d.extend_from_slice(&[0u8; 4]); // only four of the declared 64
        assert_eq!(explicit_value(&d, 4, false), None);
    }
}
