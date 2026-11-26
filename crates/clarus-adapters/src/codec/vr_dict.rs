// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DICOM Data Dictionary - VR lookup.
//!
//! Used by the transcoding pipeline to convert Implicit VR LE datasets
//! to Explicit VR LE (required for compressed transfer syntaxes like RLE).
//!
//! The lookup is the generated PS3.6 table (`vr_dict_std`), with one
//! exception: the File Meta Information group (0002) is defined by PS3.10 7.1
//! and absent from PS3.6, so its VRs are stated here. Unknown tags default to
//! UN, whose extended-length encoding (2 reserved + 4 length, PS3.5 6.2) is
//! safe for a value of any length.

use super::vr_dict_std::standard_vr;

/// Look up the standard VR for a DICOM tag. Returns "UN" if unknown.
pub fn vr_for_tag(group: u16, element: u16) -> &'static str {
    // PS3.5 7.2 Table 7.2-1: the first element of every group is that group's
    // length and its VR is UL. PS3.6 does not list group-length elements, so
    // without this arm (0028,0000) would answer UN - and an UN value takes the
    // extended 12-byte header, which would then be written back as garbage by
    // an Implicit->Explicit rewrite.
    if element == 0x0000 {
        return "UL";
    }
    match overridden_vr(group, element) {
        Some(vr) => vr,
        None => standard_vr(group, element).unwrap_or("UN"),
    }
}

/// Whether `(group, element)` is a Private Creator element.
///
/// PS3.5 7.8 gives the element range 0010-00FF of every ODD group to the
/// Private Creator, which holds the vendor string that claims the block
/// below it. The VR is `LO` in every such group, so the answer follows from
/// the tag alone and needs no vendor dictionary
/// (`standard/ps3.5/vr-width-classes-6.2-1.json` records the odd-group rule).
///
/// The block it claims, `(gggg,1000-10FF)`, has a VR the creator names and is
/// deliberately NOT covered here: (0029,1010) must still answer `UN`, because
/// asserting `LO` for a block whose contents are a vendor's own choice would
/// mislabel 16-bit pixel data as text as soon as a private block happened to
/// land there.
#[inline]
fn is_private_creator(group: u16, element: u16) -> bool {
    group % 2 == 1 && (0x0010..=0x00FF).contains(&element)
}

/// The hand-written overrides.
///
/// Only the File Meta Information group (0002) lives here. PS3.10 7.1 defines
/// those attributes but PS3.6 does not list them among the Data Set elements,
/// so the generated table has no entry and this is the single place the fact is
/// stated.
///
/// Everything else comes from the generated PS3.6 table. Restating a dictionary
/// VR here is not harmless: it shadows the generated value, so a dictionary
/// correction - a VR fixed in a later edition of PS3.6 - never reaches the tags
/// that were copied out by hand. The arms that used to sit below were an exact
/// duplicate of `vr_dict_std` for every one of their tags, including
/// (7FE0,0010), whose stated reason for being here ("the dictionary collapses
/// it to `OB or OW`") was not true of the generated table: it resolves to the
/// concrete `OB`, the same value the override returned.
fn overridden_vr(group: u16, element: u16) -> Option<&'static str> {
    if is_private_creator(group, element) {
        return Some("LO");
    }
    if group != 0x0002 {
        return None;
    }
    // The meta group is always Explicit VR Little Endian (PS3.10 7.1), so these
    // VRs are never needed for an Implicit->Explicit rewrite; they exist so a
    // lookup on a meta tag has an answer instead of `UN`.
    //
    // `standard/ps3.10/file-meta-attribute-vrs-7.1.json` carries the full list.
    // The presentation addresses are `UR` and not `AE` although each one
    // corresponds to an AE title: the value is a URI, and giving it `AE` would
    // both truncate it to 16 characters and put a text width on a URI.
    match element {
        0x0000 => Some("UL"), // File Meta Information Group Length
        0x0001 => Some("OB"), // File Meta Information Version
        0x0002 => Some("UI"), // Media Storage SOP Class UID
        0x0003 => Some("UI"), // Media Storage SOP Instance UID
        0x0010 => Some("UI"), // Transfer Syntax UID
        0x0012 => Some("UI"), // Implementation Class UID
        0x0013 => Some("SH"), // Implementation Version Name
        0x0016 => Some("AE"), // Source Application Entity Title
        0x0017 => Some("AE"), // Sending Application Entity Title
        0x0018 => Some("AE"), // Receiving Application Entity Title
        0x0026 => Some("UR"), // Source Presentation Address
        0x0027 => Some("UR"), // Sending Presentation Address
        0x0028 => Some("UR"), // Receiving Presentation Address
        0x0031 => Some("OB"), // RTV Meta Information Version
        0x0032 => Some("UI"), // RTV Communication SOP Class UID
        0x0033 => Some("UI"), // RTV Communication SOP Instance UID
        0x0035 => Some("OB"), // RTV Source Identifier
        0x0036 => Some("OB"), // RTV Flow Identifier
        0x0037 => Some("UL"), // RTV Flow RTP Sampling Rate
        0x0038 => Some("FD"), // RTV Flow Actual Frame Duration
        0x0100 => Some("UI"), // Private Information Creator UID
        0x0102 => Some("OB"), // Private Information
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_tags_resolve_through_the_generated_table() {
        // These are exactly the tags the hand-written subset did not carry and
        // that degrade to UN under it, which broke MPR/3D on the explicit side.
        assert_eq!(vr_for_tag(0x0020, 0x0032), "DS");
        assert_eq!(vr_for_tag(0x0020, 0x0037), "DS");
        assert_eq!(vr_for_tag(0x0018, 0x0050), "DS");
    }

    #[test]
    fn pixeldata_keeps_a_concrete_vr() {
        // PS3.5 6.2 lets PixelData be OB or OW and the dictionary fixes it to
        // OB; what matters here is that it never comes back as UN, whose
        // extended header would describe the value as one opaque blob.
        assert_eq!(vr_for_tag(0x7FE0, 0x0010), "OB");
        // And the extended offset tables that accompany encapsulated data.
        assert_eq!(vr_for_tag(0x7FE0, 0x0001), "OV");
    }

    #[test]
    fn a_private_or_unknown_tag_still_falls_back_to_un() {
        // The block a creator claims has a VR the creator names, so it is not
        // asserted here: (0029,1010) is a private block value, not a creator.
        assert_eq!(vr_for_tag(0x0029, 0x1010), "UN");
        assert_eq!(vr_for_tag(0x7777, 0x0001), "UN");
    }

    #[test]
    fn a_private_creator_element_resolves_to_lo() {
        // PS3.5 7.8: element 0010-00FF of an ODD group holds the Private
        // Creator, whose VR is LO in every such group. Before this rule the
        // lookup answered UN, so an Explicit VR rewrite turned the vendor
        // string into an opaque blob.
        for group in [0x0009, 0x0029, 0x7777] {
            assert_eq!(vr_for_tag(group, 0x0010), "LO", "{group:04X},0010");
            assert_eq!(vr_for_tag(group, 0x00FF), "LO", "{group:04X},00FF");
        }
        // The range bounds are exact: 000F is below it and 0100 is the first
        // element of the block the creator names, not a creator itself.
        assert_ne!(vr_for_tag(0x0029, 0x000F), "LO");
        assert_ne!(vr_for_tag(0x0029, 0x0100), "LO");
        // An EVEN group has no private creators even in the same element range.
        assert_ne!(vr_for_tag(0x0028, 0x0010), "LO");
    }

    #[test]
    fn the_file_meta_group_carries_its_full_ps310_vr_list() {
        // PS3.10 7.1 Table 7.1-1, via standard/ps3.10/file-meta-attribute-vrs-7.1.json.
        // The hand-written list used to stop at (0002,0016), so this attribute
        // and every later meta attribute resolved to UN.
        assert_eq!(vr_for_tag(0x0002, 0x0017), "AE"); // Sending Application Entity Title
        assert_eq!(vr_for_tag(0x0002, 0x0018), "AE"); // Receiving Application Entity Title
        assert_eq!(vr_for_tag(0x0002, 0x0026), "UR"); // Source Presentation Address
        assert_eq!(vr_for_tag(0x0002, 0x0027), "UR"); // Sending Presentation Address
        assert_eq!(vr_for_tag(0x0002, 0x0028), "UR"); // Receiving Presentation Address
        assert_eq!(vr_for_tag(0x0002, 0x0100), "UI"); // Private Information Creator UID
        assert_eq!(vr_for_tag(0x0002, 0x0102), "OB"); // Private Information
        // A meta element the standard does not define still falls back.
        assert_eq!(vr_for_tag(0x0002, 0x00FF), "UN");
    }
}
