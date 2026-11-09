#!/usr/bin/env python3
"""Generate the derived-object fixtures the deletion cascade needs.

Two fixtures, both deterministic - fixed UIDs, no clock - so re-running this
script reproduces the committed bytes exactly:

    derived_pr.dcm    Grayscale Softcopy Presentation State (GSPS)
    derived_seg.dcm   Segmentation

Each references a stored study through the sequences the cascade reads:

    GSPS   (0008,1115) ReferencedSeriesSequence -> (0020,000E)
           (0008,1140) ReferencedImageSequence  -> (0008,1155)
    SEG    (0008,2112) SourceImageSequence      -> (0008,1155)
           per segment (0008,1115) ReferencedSeriesSequence -> (0020,000E)

The SEG fixture is hand-built because the product has no segmentation encoder
yet; the cascade must still handle one, so the fixture exists ahead of the
encoder that would produce it in the field.

The study and series the fixtures point at are named here as literals and
asserted by the Rust test, so the fixture and its test cannot drift apart.

Usage:  python tests/make_derived_fixtures.py [output_dir]
"""

from __future__ import annotations

import os
import sys

from pydicom.dataset import Dataset, FileDataset, FileMetaDataset
from pydicom.uid import ExplicitVRLittleEndian

# -- The stored objects the fixtures annotate ----------------------------
#
# A realistic pair: the fixtures point at study A / series A, and the negative
# case in the test points at series B, which must NOT be claimed by a cascade
# scoped to series A.

STUDY_A = "1.2.826.0.1.3680043.10.1709.1"
SERIES_A = "1.2.826.0.1.3680043.10.1709.1.1"
INSTANCE_A = "1.2.826.0.1.3680043.10.1709.1.1.1"

SERIES_B = "1.2.826.0.1.3680043.10.1709.1.2"
INSTANCE_B = "1.2.826.0.1.3680043.10.1709.1.2.1"

# -- The derived objects -------------------------------------------------

SOP_CLASS_GSPS = "1.2.840.10008.5.1.4.1.1.11.1"
SOP_CLASS_SEG = "1.2.840.10008.5.1.4.1.1.66.4"
SOP_CLASS_CT = "1.2.840.10008.5.1.4.1.1.2"

PR_SERIES = "1.2.826.0.1.3680043.10.1709.2.1"
PR_INSTANCE = "1.2.826.0.1.3680043.10.1709.2.1.1"

SEG_SERIES = "1.2.826.0.1.3680043.10.1709.3.1"
SEG_INSTANCE = "1.2.826.0.1.3680043.10.1709.3.1.1"


def _file_meta(sop_class: str, sop_instance: str) -> FileMetaDataset:
    meta = FileMetaDataset()
    meta.MediaStorageSOPClassUID = sop_class
    meta.MediaStorageSOPInstanceUID = sop_instance
    meta.TransferSyntaxUID = ExplicitVRLittleEndian
    meta.ImplementationClassUID = "1.2.826.0.1.3680043.10.1709.99"
    return meta


def build_pr() -> FileDataset:
    """A presentation state that references series A and instance A."""
    ds = FileDataset(
        "derived_pr.dcm", {}, file_meta=_file_meta(SOP_CLASS_GSPS, PR_INSTANCE),
        preamble=b"\0" * 128,
    )
    ds.SOPClassUID = SOP_CLASS_GSPS
    ds.SOPInstanceUID = PR_INSTANCE
    ds.PatientName = "DERIVED^PR"
    ds.PatientID = "DERIVED-PR-1"
    ds.StudyInstanceUID = PR_SERIES
    ds.SeriesInstanceUID = PR_SERIES.rsplit(".", 1)[0] + ".9"
    ds.Modality = "PR"
    ds.ContentDate = "20260917"
    ds.ContentTime = "120000"
    ds.InstanceNumber = 1

    # (0008,1115) - the series this state applies to.
    ref_series = Dataset()
    ref_series.SeriesInstanceUID = SERIES_A
    # (0008,1140) - the individual images it applies to.
    ref_image = Dataset()
    ref_image.ReferencedSOPClassUID = SOP_CLASS_CT
    ref_image.ReferencedSOPInstanceUID = INSTANCE_A
    ref_series.ReferencedImageSequence = [ref_image]
    ds.ReferencedSeriesSequence = [ref_series]

    # One annotation, so the file is a plausible presentation state rather
    # than an empty shell.
    gobj = Dataset()
    gobj.GraphicAnnotationUnits = "PIXEL"
    gobj.GraphicDimensions = 2
    gobj.NumberOfGraphicPoints = 4
    gobj.GraphicData = [10.0, 20.0, 110.0, 60.0]
    gobj.GraphicType = "RECTANGLE"
    anno = Dataset()
    anno.GraphicObjectSequence = [gobj]
    ds.GraphicAnnotationSequence = [anno]
    return ds


def build_seg() -> FileDataset:
    """A segmentation that references series A through its source images and
    through a per-segment referenced series."""
    ds = FileDataset(
        "derived_seg.dcm", {}, file_meta=_file_meta(SOP_CLASS_SEG, SEG_INSTANCE),
        preamble=b"\0" * 128,
    )
    ds.SOPClassUID = SOP_CLASS_SEG
    ds.SOPInstanceUID = SEG_INSTANCE
    ds.PatientName = "DERIVED^SEG"
    ds.PatientID = "DERIVED-SEG-1"
    ds.StudyInstanceUID = STUDY_A
    ds.SeriesInstanceUID = SEG_SERIES
    ds.Modality = "SEG"
    ds.ContentDate = "20260917"
    ds.ContentTime = "120000"
    ds.InstanceNumber = 1
    ds.Rows = 4
    ds.Columns = 4
    ds.SamplesPerPixel = 1
    ds.PhotometricInterpretation = "MONOCHROME2"
    ds.BitsAllocated = 8
    ds.BitsStored = 8
    ds.HighBit = 7
    ds.PixelRepresentation = 0
    ds.NumberOfFrames = 1

    # (0008,2112) SourceImageSequence - the images being segmented.
    src = Dataset()
    src.ReferencedSOPClassUID = SOP_CLASS_CT
    src.ReferencedSOPInstanceUID = INSTANCE_A
    ds.SourceImageSequence = [src]

    # A per-segment referenced series, as a real SEG carries.
    seg_series = Dataset()
    seg_series.SeriesInstanceUID = SERIES_A
    seg = Dataset()
    seg.SegmentNumber = 1
    seg.SegmentLabel = "PROBE"
    seg.SegmentAlgorithmType = "MANUAL"
    seg.ReferencedSeriesSequence = [seg_series]
    ds.SegmentSequence = [seg]

    # Minimal pixel data: one frame of 16 bytes, all background.
    ds.PixelData = bytes(16)
    return ds


def build_study_image() -> FileDataset:
    """An ordinary image: the object the derived fixtures annotate.

    Not a derived object itself, so the cascade must never claim it. It exists
    so the wire test has a real stored study for the fixtures to point at,
    with the same study/series/instance UIDs they name.
    """
    ds = FileDataset(
        "derived_study.dcm", {}, file_meta=_file_meta(SOP_CLASS_CT, INSTANCE_A),
        preamble=b"\0" * 128,
    )
    ds.SOPClassUID = SOP_CLASS_CT
    ds.SOPInstanceUID = INSTANCE_A
    ds.PatientName = "DERIVED^STUDY"
    ds.PatientID = "DERIVED-STUDY-1"
    ds.StudyInstanceUID = STUDY_A
    ds.SeriesInstanceUID = SERIES_A
    ds.Modality = "CT"
    ds.ContentDate = "20260917"
    ds.ContentTime = "120000"
    ds.InstanceNumber = 1
    # A second series in the same study, so the series-scoped gate has
    # something to leave alone.
    ds.Rows = 4
    ds.Columns = 4
    ds.SamplesPerPixel = 1
    ds.PhotometricInterpretation = "MONOCHROME2"
    ds.BitsAllocated = 8
    ds.BitsStored = 8
    ds.HighBit = 7
    ds.PixelRepresentation = 0
    ds.PixelData = bytes(16)
    return ds


def main() -> int:
    out_dir = (
        sys.argv[1]
        if len(sys.argv) > 1
        else os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures")
    )
    os.makedirs(out_dir, exist_ok=True)
    for name, ds in (
        ("derived_study.dcm", build_study_image()),
        ("derived_pr.dcm", build_pr()),
        ("derived_seg.dcm", build_seg()),
    ):
        path = os.path.join(out_dir, name)
        ds.save_as(path, enforce_file_format=True)
        print("wrote %s (%d bytes)" % (path, os.path.getsize(path)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
