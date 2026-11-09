#!/usr/bin/env python3
"""Generate the minimal PR (GSPS) fixture used by the viewer overlay test.

Deterministic: no timestamps are taken from the clock, every UID is a fixed
literal, and the file is written with an explicit transfer syntax. Re-running
this script must reproduce `fixtures/pr_minimal.dcm` byte-for-byte, so the
committed binary and this script can never drift apart.

Documented values (asserted by the Rust test, so the two stay in sync):

    Referenced series UID   1.2.826.0.1.3680043.10.1609.3.1
    Referenced SOP UID      1.2.826.0.1.3680043.10.1609.3.1.1
    Referenced frame        2
    RECTANGLE GraphicData   10\\20\\110\\60     (x1, y1, x2, y2 in pixels)
    TEXT    UnformattedText "PROBE-A" on the frame-2 rectangle
    RECTANGLE (frame-less)  400\\120\\520\\200, text "PROBE-ALL"
    TEXT    GraphicData 300\\260, text "PROBE-TXT"
    POINT   5\\5                                (must be ignored)

The second rectangle carries no ReferencedFrameNumber, so it belongs to the
whole instance and must render on every frame: a viewer that only handled the
frame-scoped case would drop it. The POINT object exists to prove that an
unhandled graphic type is skipped without disturbing the supported ones.

The GraphicData values are written as explicit-VR FL backslash strings exactly
as the standard requires for a multi-valued numeric attribute; packing them as
raw little-endian floats is a common shortcut that real producers avoid.

Usage:  python tests/make_pr_fixture.py [output.dcm]
"""

from __future__ import annotations

import os
import sys

from pydicom.dataset import Dataset, FileDataset, FileMetaDataset
from pydicom.uid import ExplicitVRLittleEndian, generate_uid

# -- Fixed identifiers ---------------------------------------------------

SERIES_UID = "1.2.826.0.1.3680043.10.1609.3.1"
INSTANCE_UID = "1.2.826.0.1.3680043.10.1609.3.1.1"
SOP_CLASS_GSPS = "1.2.840.10008.5.1.4.1.1.11.1"
SOP_CLASS_CT = "1.2.840.10008.5.1.4.1.1.2"

# -- The two annotations -------------------------------------------------

RECT_FRAME2 = (10.0, 20.0, 110.0, 60.0)  # frame 2 only
RECT_INSTANCE = (400.0, 120.0, 520.0, 200.0)  # every frame
TEXT_FRAME2 = "PROBE-A"

# GraphicData is a multi-valued FL attribute. pydicom packs a sequence of
# real floats into the contiguous little-endian form the standard specifies,
# which is what the viewer's parser reads back.
def _fl(values: tuple[float, ...]) -> list[float]:
    return [float(v) for v in values]


def build() -> FileDataset:
    file_meta = FileMetaDataset()
    file_meta.MediaStorageSOPClassUID = SOP_CLASS_GSPS
    file_meta.MediaStorageSOPInstanceUID = INSTANCE_UID
    file_meta.TransferSyntaxUID = ExplicitVRLittleEndian
    file_meta.ImplementationClassUID = "1.2.826.0.1.3680043.10.1609.99"

    ds = FileDataset("pr_minimal.dcm", {}, file_meta=file_meta, preamble=b"\0" * 128)

    ds.SOPClassUID = SOP_CLASS_GSPS
    ds.SOPInstanceUID = INSTANCE_UID
    ds.SpecificCharacterSet = "ISO_IR 192"
    ds.PatientName = "Fixture^PR"
    ds.PatientID = "PR-FIXTURE"
    ds.StudyInstanceUID = "1.2.826.0.1.3680043.10.1609.3"
    ds.SeriesInstanceUID = "1.2.826.0.1.3680043.10.1609.3.2"
    ds.Modality = "PR"
    ds.SeriesNumber = 900
    ds.ContentLabel = "PRFIXTURE"
    ds.ContentDescription = "Viewer overlay fixture"
    ds.ContentCreatorName = "Clarus test fixture"

    # Instance-level reference to the series the annotations apply to.
    ref_series = Dataset()
    ref_series.SeriesInstanceUID = SERIES_UID

    # -- Annotation 1: RECTANGLE scoped to frame 2 of one instance --------
    ref_image = Dataset()
    ref_image.ReferencedSOPClassUID = SOP_CLASS_CT
    ref_image.ReferencedSOPInstanceUID = "1.2.826.0.1.3680043.10.1609.3.1.1"
    ref_image.ReferencedFrameNumber = 2

    text1 = Dataset()
    text1.RelationshipType = "CONTAINS"
    text1.ReferencedSOPSequence = [ref_image]
    text1.UnformattedTextValue = TEXT_FRAME2

    gobj1 = Dataset()
    gobj1.GraphicAnnotationUnits = "PIXEL"
    gobj1.GraphicDimensions = 2
    gobj1.NumberOfGraphicPoints = 4
    gobj1.GraphicData = _fl(RECT_FRAME2)
    gobj1.GraphicType = "RECTANGLE"
    gobj1.GraphicFilled = "N"

    anno1 = Dataset()
    anno1.TextObjectSequence = [text1]
    anno1.GraphicObjectSequence = [gobj1]

    # -- Annotation 2: RECTANGLE with no frame scope (all frames) ---------
    text2 = Dataset()
    text2.RelationshipType = "CONTAINS"
    text2.UnformattedTextValue = "PROBE-ALL"

    gobj2 = Dataset()
    gobj2.GraphicAnnotationUnits = "PIXEL"
    gobj2.GraphicDimensions = 2
    gobj2.NumberOfGraphicPoints = 4
    gobj2.GraphicData = _fl(RECT_INSTANCE)
    gobj2.GraphicType = "RECTANGLE"
    gobj2.GraphicFilled = "N"
    gobj2.GraphicLayer = "ALL"

    anno2 = Dataset()
    anno2.TextObjectSequence = [text2]
    anno2.GraphicObjectSequence = [gobj2]

    # -- Annotation 3: a real TEXT object (label at a pixel anchor) --------
    text3 = Dataset()
    text3.RelationshipType = "CONTAINS"
    text3.UnformattedTextValue = "PROBE-TXT"

    gobj3 = Dataset()
    gobj3.GraphicAnnotationUnits = "PIXEL"
    gobj3.GraphicDimensions = 2
    gobj3.NumberOfGraphicPoints = 1
    gobj3.GraphicData = [300.0, 260.0]
    gobj3.GraphicType = "TEXT"

    anno3 = Dataset()
    anno3.TextObjectSequence = [text3]
    anno3.GraphicObjectSequence = [gobj3]

    # -- Annotation 4: a type the viewer must ignore silently -------------
    gobj4 = Dataset()
    gobj4.GraphicAnnotationUnits = "PIXEL"
    gobj4.GraphicDimensions = 2
    gobj4.NumberOfGraphicPoints = 2
    gobj4.GraphicData = [5.0, 5.0]
    gobj4.GraphicType = "POINT"

    anno4 = Dataset()
    anno4.GraphicObjectSequence = [gobj4]

    ds.GraphicAnnotationSequence = [anno1, anno2, anno3, anno4]
    ds.ReferencedSeriesSequence = [ref_series]

    # -- Encoding shape: undefined lengths everywhere ---------------------
    # The live serializer (clinfer) writes every sequence AND every item
    # with an undefined length (the frozen form). The committed fixture
    # must match it, or the walker is tested against a shape the field
    # never produces. pydicom 3.x spells these two flags differently:
    # the sequence flag lives on the DataElement, the item flag on the
    # item Dataset.
    ds.data_element("ReferencedSeriesSequence").is_undefined_length = True
    ds.data_element("GraphicAnnotationSequence").is_undefined_length = True
    for item in ds.ReferencedSeriesSequence:
        item.is_undefined_length_sequence_item = True
    for anno in ds.GraphicAnnotationSequence:
        anno.is_undefined_length_sequence_item = True
        for name in ("TextObjectSequence", "GraphicObjectSequence"):
            if name in anno:
                anno.data_element(name).is_undefined_length = True
                for item in getattr(anno, name):
                    item.is_undefined_length_sequence_item = True
        # The reference inside a text object is its own nested sequence.
        for text_item in getattr(anno, "TextObjectSequence", []):
            if "ReferencedSOPSequence" in text_item:
                text_item.data_element("ReferencedSOPSequence").is_undefined_length = True
                for item in text_item.ReferencedSOPSequence:
                    item.is_undefined_length_sequence_item = True
    return ds


def main() -> int:
    out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "fixtures", "pr_minimal.dcm"
    )
    os.makedirs(os.path.dirname(out), exist_ok=True)
    ds = build()
    # Write with the explicit little-endian syntax so the bytes are stable and
    # the viewer's parser (which only reads explicit and implicit LE) accepts it.
    ds.save_as(out, enforce_file_format=True)
    print("wrote %s (%d bytes)" % (out, os.path.getsize(out)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
