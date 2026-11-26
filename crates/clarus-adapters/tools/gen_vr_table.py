#!/usr/bin/env python3
"""Generate the VR table for clarus-adapters/src/codec/vr_dict_std.rs from PS3.6.

Regenerate from the repository root:

    python3 crates/clarus-adapters/tools/gen_vr_table.py

Prerequisite: `pydicom` must be importable (`pip install pydicom`). It is a
contributor-side code-generation dependency only - it is not in Cargo.toml, not
linked into the crate, and nothing at build or run time imports it.

The table is the frozen fact set from the DICOM data dictionary (PS3.6 6), so it
is generated once and checked in rather than rebuilt at every compile.

The dictionary is the source of the VRs, but it is NOT the authority: the facts
this repository cites are the ones in `standard/`. The two standard/ files that
bear on this table are read here and enforced, so a regeneration that disagrees
with them fails loudly instead of quietly rewriting the table:

* `standard/ps3.5/vr-width-classes-6.2-1.json` - every VR emitted must be one of
  the 34 VRs PS3.5 6.2 Table 6.2-1 defines, and the two width classes must
  partition that set.
* `standard/ps3.6/overlay-repeating-groups-6.json` - the overlay repeating group
  (PS3.5 7.6) is expanded over the even groups 6000-601E, which the dictionary
  does not do on its own because it keys those elements by the literal group
  "60xx".
"""

import json
import os
import sys

import pydicom

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(
    os.path.abspath(__file__)))))
STANDARD = os.path.join(REPO_ROOT, "standard")
OUT_PATH = os.path.join(REPO_ROOT, "crates", "clarus-adapters", "src", "codec",
                        "vr_dict_std.rs")

# PS3.5 6.2 Table 6.2-1, long form: VR(2) + reserved(2) + ValueLength(4).
LONG_FORM = ["OB", "OD", "OF", "OL", "OV", "OW", "SQ", "SV", "UC", "UN", "UR",
             "UT", "UV"]
# PS3.5 6.2 Table 6.2-1, short form: VR(2) + ValueLength(2).
SHORT_FORM = ["AE", "AS", "AT", "CS", "DA", "DS", "DT", "FD", "FL", "IS", "LO",
              "LT", "PN", "SH", "SL", "SS", "ST", "TM", "UI", "UL", "US"]
KNOWN_VRS = set(LONG_FORM) | set(SHORT_FORM)

# PS3.5 7.6: repeating groups are the EVEN group numbers 6000-601E. The odd
# groups 6001-601F are private and carry no repeating semantics, so they are
# deliberately not expanded - giving a vendor's private block the standard
# overlay VRs would assert a dictionary fact about code that may not follow it.
OVERLAY_GROUPS = list(range(0x6000, 0x601F, 2))


def load_standard_facts():
    """Read the two standard/ facts this generator must agree with."""
    with open(os.path.join(STANDARD, "ps3.5",
                           "vr-width-classes-6.2-1.json"), encoding="utf-8") as fh:
        widths = json.load(fh)
    # That file is keyed by class rather than being a list of facts; the VR
    # lists live in the `fact` string of each class entry.
    long_json = widths["longFormVrs"]["fact"]
    short_json = widths["shortFormVrs"]["fact"]
    for vr in LONG_FORM:
        if vr not in long_json:
            sys.exit("standard/ps3.5/vr-width-classes-6.2-1.json is missing "
                     "long-form VR %s" % vr)
    for vr in SHORT_FORM:
        if vr not in short_json:
            sys.exit("standard/ps3.5/vr-width-classes-6.2-1.json is missing "
                     "short-form VR %s" % vr)

    with open(os.path.join(STANDARD, "ps3.6",
                           "overlay-repeating-groups-6.json"), encoding="utf-8") as fh:
        overlays = json.load(fh)
    if not any("(60xx,0010)" in fact["fact"] for fact in overlays):
        sys.exit("standard/ps3.6/overlay-repeating-groups-6.json does not "
                 "describe (60xx,0010)")


load_standard_facts()

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
    if group == 0x0002 or element == 0x0000:
        # File meta is always Explicit VR Little Endian, and item tags are
        # structural rather than data elements.
        continue
    entry = dictionary[tag]
    vr = AMBIGUOUS.get(entry[0], entry[0])
    if vr == "NONE" or len(vr) != 2 or not vr.isalpha():
        vr = "UN"
    by_group.setdefault(group, []).append((element, vr))

# PS3.5 7.6 / PS3.6 6: element number 0010 carries the same VR in every even
# group of 6000-601E, so the one dictionary entry expands across the range.
#
# The overlay elements are NOT in `DicomDictionary`: pydicom keys a repeating
# group by the literal pattern "60xx0010" in `RepeatersDictionary`, so a walk of
# DicomDictionary finds no 0x6000 keys at all and emits nothing. That is exactly
# how the checked-in table came to have no overlay entry beyond group 0x6000's
# own lone row, leaving (6002,0010) and (6002,3000) to answer UN.
overlay_elements = []
for pattern in sorted(pydicom.datadict.RepeatersDictionary):
    if not pattern.startswith("60xx"):
        continue
    element = int(pattern[4:], 16)
    if element == 0x0000:
        continue
    entry = pydicom.datadict.RepeatersDictionary[pattern]
    vr = AMBIGUOUS.get(entry[0], entry[0])
    if vr == "NONE" or len(vr) != 2 or not vr.isalpha():
        vr = "UN"
    overlay_elements.append((element, vr))

if not overlay_elements:
    sys.exit("pydicom exposes no 60xx repeating group; the overlay expansion "
             "would silently emit nothing")

for group in OVERLAY_GROUPS:
    by_group[group] = list(overlay_elements)

# Every VR emitted must be one PS3.5 6.2 defines; an unrecognized pair is read
# with the LONG header by codec/vr_widths.rs, so emitting one would change the
# wire shape of that element rather than merely label it.
for group, entries in by_group.items():
    for element, vr in entries:
        if vr not in KNOWN_VRS:
            sys.exit("tag (%04X,%04X) has VR %s, which PS3.5 6.2 does not define"
                     % (group, element, vr))

out = []
out.append("// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.")
out.append("")
out.append("//! Standard VR table for the Implicit VR Little Endian -> Explicit VR")
out.append("//! rewrite, generated from the DICOM data dictionary (PS3.6 6).")
out.append("//!")
out.append("//! A tag that is missing here loses its VR in the rewrite and lands as UN,")
out.append("//! which makes the output unreadable to anything that keys geometry off the")
out.append("//! explicit VR (Image Position/Orientation, Slice Thickness, Rescale).")
out.append("//!")
out.append("//! The overlay repeating groups are expanded over the even groups 6000-601E")
out.append("//! (PS3.5 7.6): element number 0010 carries the same VR in every one of them,")
out.append("//! so (6002,0010) Overlay Rows and (6002,3000) Overlay Data resolve here")
out.append("//! instead of falling to UN. The odd groups 6001-601F are private and are")
out.append("//! deliberately absent.")
out.append("//!")
out.append("//! Regenerate from the repository root:")
out.append("//! `python3 crates/clarus-adapters/tools/gen_vr_table.py` (needs pydicom).")
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
out.append("        // (7777,1010) rather than (7777,0010): element 0010-00FF of an")
out.append("        // odd group is a Private Creator, which PS3.5 7.8 makes LO by")
out.append("        // rule, so that tag is not unknown. 1010 is inside the block the")
out.append("        // creator claims and has no dictionary entry, which is the case")
out.append("        // this test states.")
out.append("        assert_eq!(standard_vr(0x7777, 0x1010), None);")
out.append('        assert_eq!(vr_for_tag(0x7777, 0x1010), "UN");')
out.append("    }")
out.append("")
out.append("    #[test]")
out.append("    fn overlay_repeating_groups_keep_their_explicit_vr() {")
out.append("        // PS3.5 7.6: element number 0010 carries the same VR in every")
out.append("        // even group of 6000-601E. The dictionary keys these by the")
out.append("        // literal group \"60xx\", so a generator that only walked its")
out.append("        // resolved tags emitted 0x6000 and left the rest at UN.")
out.append("        for group in [0x6000u16, 0x6002, 0x6004, 0x601E] {")
out.append("            assert_eq!(standard_vr(group, 0x0010), Some(\"US\"), \"{group:04X},0010\");")
out.append("            assert_eq!(standard_vr(group, 0x3000), Some(\"OB\"), \"{group:04X},3000\");")
out.append("        }")
out.append("        // The range bounds are exact, and the odd groups are private.")
out.append("        assert_eq!(standard_vr(0x601F, 0x0010), None);")
out.append("        assert_eq!(standard_vr(0x6001, 0x0010), None);")
out.append("        assert_eq!(standard_vr(0x6020, 0x0010), None);")
out.append("    }")
out.append("}")
out.append("")

text = "\n".join(out)
with open(OUT_PATH, "w", encoding="utf-8", newline="\n") as fh:
    fh.write(text)
print("rows", sum(len(v) for v in by_group.values()), "groups", len(by_group),
      "overlayGroups", len(OVERLAY_GROUPS), "bytes", len(text))
