// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DICOM Data Dictionary - VR lookup.
//!
//! Used by the transcoding pipeline to convert Implicit VR LE datasets
//! to Explicit VR LE (required for compressed transfer syntaxes like RLE).
//!
//! The authoritative lookup is the generated PS3.6 table (`vr_dict_std`). The
//! hand-written arms below come first for the few tags where the ambiguous-VR
//! default (PS3.5 Table 6.2-1) is not the shape this crate needs - PixelData
//! above all, which must stay OB/OW rather than the dictionary's `OB or OW`
//! collapse. Unknown tags still default to UN, whose extended-length encoding
//! (2 reserved + 4 length) is safe for any value length.

use super::vr_dict_std::standard_vr;

/// Look up the standard VR for a DICOM tag. Returns "UN" if unknown.
pub fn vr_for_tag(group: u16, element: u16) -> &'static str {
    match overridden_vr(group, element) {
        Some(vr) => vr,
        None => standard_vr(group, element).unwrap_or("UN"),
    }
}

/// The hand-written overrides: tags whose dictionary VR would produce a header
/// this crate cannot round-trip.
fn overridden_vr(group: u16, element: u16) -> Option<&'static str> {
    // Using a match tree organized by group for compactness
    match group {
        0x0002 => match element {
            // Meta-header - but meta-header is always Explicit VR LE, so
            // these should never be needed for Implicit->Explicit conversion.
            0x0000 => Some("UL"),
            0x0001 => Some("OB"),
            0x0002 => Some("UI"),
            0x0003 => Some("UI"),
            0x0010 => Some("UI"),
            0x0012 => Some("UI"),
            0x0013 => Some("SH"),
            0x0016 => Some("AE"),
            _ => None,
        },
        0x0008 => match element {
            0x0005 => Some("CS"),
            0x0008 => Some("CS"),
            0x0012 => Some("DA"),
            0x0013 => Some("TM"),
            0x0016 => Some("UI"),
            0x0018 => Some("UI"),
            0x0020 => Some("DA"),
            0x0021 => Some("DA"),
            0x0022 => Some("DA"),
            0x0023 => Some("DA"),
            0x0030 => Some("TM"),
            0x0031 => Some("TM"),
            0x0032 => Some("TM"),
            0x0033 => Some("TM"),
            0x0050 => Some("SH"),
            0x0060 => Some("CS"),
            0x0070 => Some("LO"),
            0x0080 => Some("LO"),
            0x0081 => Some("ST"),
            0x0090 => Some("PN"),
            0x0100 => Some("SH"),
            0x0102 => Some("SH"),
            0x0104 => Some("LO"),
            0x0105 => Some("CS"),
            0x0106 => Some("DT"),
            0x010F => Some("CS"),
            0x1010 => Some("SH"),
            0x1030 => Some("LO"),
            0x1032 => Some("SQ"),
            0x103E => Some("LO"),
            0x1090 => Some("LO"),
            0x1111 => Some("SQ"),
            0x1140 => Some("SQ"),
            0x1150 => Some("UI"),
            0x1155 => Some("UI"),
            0x2112 => Some("SQ"),
            0x2218 => Some("SQ"),
            _ => None,
        },
        0x0010 => match element {
            0x0010 => Some("PN"),
            0x0020 => Some("LO"),
            0x0030 => Some("DA"),
            0x0032 => Some("TM"),
            0x0040 => Some("CS"),
            0x1010 => Some("AS"),
            _ => None,
        },
        0x0018 => match element {
            0x0010 => Some("LO"),
            0x0015 => Some("CS"),
            0x1000 => Some("LO"),
            0x1020 => Some("LO"),
            0x1164 => Some("DS"),
            0x1405 => Some("IS"),
            0x5101 => Some("CS"),
            _ => None,
        },
        0x0020 => match element {
            0x000D => Some("UI"),
            0x000E => Some("UI"),
            0x0010 => Some("SH"),
            0x0011 => Some("IS"),
            0x0012 => Some("IS"),
            0x0013 => Some("IS"),
            0x0020 => Some("CS"),
            0x1002 => Some("IS"),
            _ => None,
        },
        0x0028 => match element {
            0x0002 => Some("US"),
            0x0004 => Some("CS"),
            0x0006 => Some("US"),
            0x0008 => Some("IS"),
            0x0010 => Some("US"),
            0x0011 => Some("US"),
            0x0030 => Some("DS"),
            0x0034 => Some("IS"),
            0x0100 => Some("US"),
            0x0101 => Some("US"),
            0x0102 => Some("US"),
            0x0103 => Some("US"),
            0x0106 => Some("US"),
            0x0107 => Some("US"),
            0x0301 => Some("CS"),
            0x1050 => Some("DS"),
            0x1051 => Some("DS"),
            0x1052 => Some("DS"),
            0x1053 => Some("DS"),
            0x1054 => Some("LO"),
            0x2110 => Some("CS"),
            0x2112 => Some("DS"),
            0x2114 => Some("CS"),
            0x135A => Some("CS"),
            _ => None,
        },
        // Private groups are not in PS3.6, so they fall through to UN.
        0x0029 => None,
        0x0040 => match element {
            0x0007 => Some("LO"),
            0x0009 => Some("SH"),
            0x0244 => Some("DA"),
            0x0245 => Some("TM"),
            0x0253 => Some("SH"),
            0x0254 => Some("LO"),
            0x0275 => Some("SQ"),
            _ => None,
        },
        0x2050 => match element {
            0x0020 => Some("CS"),
            _ => None,
        },
        0x7FE0 => match element {
            // PixelData carries its own encoding rules and is handled by the
            // caller; leaving it to the dictionary would hand back the
            // ambiguous-VR collapse instead of a concrete OB/OW.
            0x0010 => Some("OB"),
            _ => None,
        },
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
    fn pixeldata_keeps_the_concrete_override() {
        assert_eq!(vr_for_tag(0x7FE0, 0x0010), "OB");
    }

    #[test]
    fn a_private_or_unknown_tag_still_falls_back_to_un() {
        assert_eq!(vr_for_tag(0x0029, 0x1010), "UN");
        assert_eq!(vr_for_tag(0x7777, 0x0001), "UN");
    }
}
