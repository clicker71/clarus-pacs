"""Generate the VR table for clarus-adapters/src/codec/vr_dict.rs from PS3.6.

The table is the frozen fact set from the DICOM data dictionary (PS3.6 6), so it
is generated once and checked in rather than rebuilt at every compile.
"""

import pydicom

# Ambiguous VRs (PS3.5 Table 6.2-1) collapse to the member whose header width
# is safe for an Implicit -> Explicit rewrite: the value length still decides
# between short and long form for elements whose VR allows both.
AMBIGUOUS = {
    "US or SS": "US",
    "US or SS or OW": "US",
    "US or OW": "US",
    "OB or OW": "OB",
}

dictionary = pydicom.datadict.DicomDictionary

by_group = {}
for tag in sorted(dictionary):
    group = tag >> 16
    element = tag & 0xFFFF
    entry = dictionary[tag]
    if group == 0x0002 or element == 0x0000:
        # File meta is always Explicit VR Little Endian, and item tags are
        # structural rather than data elements.
        continue
    vr = AMBIGUOUS.get(entry[0], entry[0])
    if vr == "NONE" or len(vr) != 2 or not vr.isalpha():
        vr = "UN"
    by_group.setdefault(group, []).append((element, vr))

out = []
out.append("// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.")
out.append("")
out.append("//! Standard VR table for the Implicit VR Little Endian -> Explicit VR")
out.append("//! rewrite, generated from the DICOM data dictionary (PS3.6 6).")
out.append("//!")
out.append("//! A tag that is missing here loses its VR in the rewrite and lands as UN,")
out.append("//! which makes the output unreadable to anything that keys geometry off the")
out.append("//! explicit VR (Image Position/Orientation, Slice Thickness, Rescale).")
out.append("//! Regenerate with `crates/clarus-adapters/tools/gen_vr_table.py`.")
out.append("")
out.append("/// Standard VR for `(group, element)`, or `None` when the tag is not in the")
out.append("/// dictionary.")
out.append("pub fn standard_vr(group: u16, element: u16) -> Option<&'static str> {")
out.append("    let vr = match group {")
for group in sorted(by_group):
    out.append(f"        0x{group:04X} => match element {{")
    for element, vr in by_group[group]:
        out.append(f'            0x{element:04X} => "{vr}",')
    out.append("            _ => return None,")
    out.append("        },")
out.append("        _ => return None,")
out.append("    };")
out.append("    Some(vr)")
out.append("}")
out.append("")
out.append("#[cfg(test)]")
out.append("mod tests {")
out.append("    use super::*;")
out.append("")
out.append("    #[test]")
out.append("    fn geometry_tags_keep_their_explicit_vr() {")
out.append("        // The three that break MPR/3D when they degrade to UN.")
out.append("        assert_eq!(standard_vr(0x0020, 0x0032), Some(\"DS\"));")
out.append("        assert_eq!(standard_vr(0x0020, 0x0037), Some(\"DS\"));")
out.append("        assert_eq!(standard_vr(0x0018, 0x0050), Some(\"DS\"));")
out.append("    }")
out.append("")
out.append("    #[test]")
out.append("    fn rescale_and_pixel_representation_keep_their_explicit_vr() {")
out.append("        assert_eq!(standard_vr(0x0028, 0x1052), Some(\"DS\"));")
out.append("        assert_eq!(standard_vr(0x0028, 0x1053), Some(\"DS\"));")
out.append("        assert_eq!(standard_vr(0x0028, 0x0103), Some(\"US\"));")
out.append("    }")
out.append("")
out.append("    #[test]")
out.append("    fn an_unknown_tag_is_not_invented() {")
out.append("        // The renderer falls back to UN; the dictionary must")
out.append("        // not invent a VR for a tag it does not know.")
out.append("        use super::super::vr_dict::vr_for_tag;")
out.append("")
out.append("        assert_eq!(standard_vr(0x7777, 0x0010), None);")
out.append("        assert_eq!(vr_for_tag(0x7777, 0x0010), \"UN\");")
out.append("    }")
out.append("}")
out.append("")

text = "\n".join(out)
with open("crates/clarus-adapters/src/codec/vr_dict_std.rs", "w", encoding="utf-8", newline="\n") as fh:
    fh.write(text)
print("rows", sum(len(v) for v in by_group.values()), "groups", len(by_group), "bytes", len(text))
