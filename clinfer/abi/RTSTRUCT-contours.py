#!/usr/bin/env python3
"""Threshold contours -> result.kind rtstruct: a license-clean demo producer.

No model, no weights, no network: this is the contour half of the synthetic
RTSTRUCT demo, reshaped as a clinfer ABI processor. It reads the workdir's
RAW pixel frames, thresholds them (bone and teeth windows), traces the
outer contours, and emits an `rtstruct` result block whose points are
[column, row] pairs keyed by the manifest's 1-based frame index.

clinfer makes no clinical claim: this is a contouring demo, not a
segmentation model, and no diagnostic value is implied.

Usage: python RTSTRUCT-contours.py <workdir>

See ../abi/README.md for the contract.

WHAT CAN GO WRONG, AND WHAT TO DO ABOUT IT
------------------------------------------

1. HU CALIBRATION. The thresholds below (BONE 300-1100, TEETH >= 1100) assume
   the stored values ARE Hounsfield units, i.e. RescaleSlope 1 and
   RescaleIntercept 0. The ABI deliberately does not carry the calibration -
   clinfer hands over the raw pixel values, exactly as the origin returned
   them - so on data with a different rescale the windows select the wrong
   tissue, or nothing at all. Fix it by editing the constants in YOUR copy of
   this script. There is nothing to configure: the script is the place.

2. NOT CT, OR NOT AXIAL. This expects an axial CT stack; cone-beam CT
   usually works because its units are HU-like. On MR or any modality with
   different units the thresholds are meaningless, so no ROI is found, no
   `rtstruct` block is emitted, and the workitem is CANCELLED with a reason
   in clinfer's log (the workdir is kept, so you can rerun by hand with your
   own thresholds). An empty result is not a failure of the pipeline.

3. THE DICOM DS BYTE CEILING (65534). ContourData is a DS element and its
   length field is 16 bits, so ONE polygon's encoded text may not exceed
   65534 bytes. On the live CT this was measured against, a point costs about
   17.7 bytes, so a polygon is capped at MAX_POINTS points below and
   decimated if the trace produced more. clinfer's encoder REFUSES an
   over-long contour rather than truncating it (a wrapped length produces a
   file that parses as garbage, and an RS that looks complete while carrying
   a mangled contour is worse than none). If you raise MAX_POINTS or add
   decimals to the coordinates, keep the byte ceiling in mind: it is
   points x bytes-per-point, not a point count on its own.

4. EMPTY (AIR) SLICES. A slice with no tissue in either window simply has no
   contour, so it does not appear in the structure set at all. That is
   normal, not an error: an RS references the frames it has contours on.

5. A SPARSE STACK (for example a demo that keeps every 12th slice) is also
   normal. The structure set references only the frames this script produced
   contours for, which is what an RS is for.
"""

import json
import pathlib
import sys

import params

# HU windows for the demo fixture (raw values are already HU there).
BONE_MIN = 300
BONE_MAX = 1100
TEETH_MIN = 1100

# Polygon discipline: drop dust, keep the biggest components, and keep every
# polygon inside the byte ceiling of the DICOM element it becomes.
MIN_AREA_PX = 100
MAX_COMPONENTS = 24

# ContourData is a DS element, and a DS element's length field is 16 bits, so
# ONE polygon's encoded text may not exceed 65534 bytes. clinfer writes three
# text values per point (patient x, y, z in millimetres) separated by
# backslashes, so the ceiling is on BYTES, not on points:
#
#   * on the live CT this was measured against, a point cost 17.9 bytes
#     (3874 points produced 69342 bytes), because a coordinate like -342.8
#     renders as seven characters and z repeats per point;
#   * 65534 / 17.9 is about 3660 points, and the budget below is set well
#     under that so a case with wider coordinates - more decimals, a larger
#     position - still fits.
#
# The previous revision capped POINTS at 4096, which looked like a byte cap
# and was not: the live contour of 3874 points produced 69342 bytes, the
# encoder's 16-bit length field wrapped to 3806, and the stored object was
# rejected by the origin with "Invalid VR in undefined-length element". The
# encoder now REFUSES such a contour instead of truncating it, so a polygon
# kept under this budget is what makes the whole chain succeed rather than
# cancel.
#
# Decimating does not contradict the standard: how many points a ContourData
# carries is the producer's choice (PS3.3 sets no minimum), and the polygon
# is compressed rather than dropped. If you raise this in your own copy,
# keep the byte ceiling in mind - 2000 points is not a magic number, it is a
# margin under 65534 bytes divided by the measured width of one point.
MAX_POINTS = 2000

# ROI Display Color (3006,002A) is three integers 0..255.
BONE_COLOR = [0, 255, 102]
TEETH_COLOR = [255, 204, 0]


def raw_frame(workdir, frame):
    """One frame's stored pixels, row-major, in the sign the manifest declares.

    PS3.3 C.7.6.3.1.2: PixelRepresentation=1 means two's complement, so a signed
    CT stores air as about -1000, not 64536. Reading a signed frame as unsigned
    put air ABOVE every Hounsfield window and it came out classified as tissue -
    the contour landed on the background instead of the anatomy.
    """
    import numpy as np

    raw = (workdir / "frames" / frame["file"]).read_bytes()
    signed = bool(frame.get("signed"))
    if frame["bits"] == 8:
        dtype = np.int8 if signed else np.uint8
    else:
        dtype = np.int16 if signed else np.uint16
    values = np.frombuffer(raw, dtype=dtype)
    return values.reshape(frame["height"], frame["width"])


def contours_of(values, low, high, limits):
    """Outer contours of the pixels inside [low, high], decimated.

    Returns a list of polygons, each a list of [column, row] pairs.
    """
    import cv2
    import numpy as np

    mask = ((values >= low) & (values <= high)).astype(np.uint8)
    found, _hierarchy = cv2.findContours(mask, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_NONE)
    if not found:
        return []
    found = sorted(found, key=cv2.contourArea, reverse=True)[: limits["max_components"]]
    polygons = []
    for contour in found:
        if cv2.contourArea(contour) < limits["min_area_px"]:
            continue
        points = contour.reshape(-1, 2).tolist()  # (x=column, y=row) from OpenCV
        if len(points) < 3:
            continue
        if len(points) > limits["max_points"]:
            # Decimation, not a drop: the polygon keeps its shape and its
            # first point, and only its resolution changes. The step uses
            # ceiling division so the kept count is always AT OR BELOW
            # max_points - a float step could land one point over and put the
            # polygon back over the 65534-byte ceiling the encoder enforces.
            step = -(-len(points) // limits["max_points"])  # ceiling division
            points = points[::step]
        # ABI points are [column, row], which is EXACTLY what OpenCV hands
        # over: findContours returns (x, y) and x is the column index, y the
        # row index. So the pair is emitted AS-IS - renaming the elements
        # (for (row, col) in points) and re-emitting them swapped is the bug
        # the 24.09 Weasis run found, and this demo is the discipline source
        # every other contour producer copies. The values are PIXEL indices
        # and stay integers: the converter on the other side multiplies them
        # by the pixel spacing, which this script never sees, so the
        # precision of the millimetre value is the converter's to decide.
        # What keeps the encoded size down is the point count above.
        polygons.append([[int(point[0]), int(point[1])] for point in points])
    return polygons


def build(workdir):
    manifest = json.loads((workdir / "manifest.json").read_text())
    # The HU windows are the demo's own classification rule and the polygon
    # limits are the encoder's budget; a requester tunes the second group, so
    # every one of those four reads falls back to the constant above.
    bone_min = params.number(manifest, "BONE_MIN", BONE_MIN)
    bone_max = params.number(manifest, "BONE_MAX", BONE_MAX)
    teeth_min = params.number(manifest, "TEETH_MIN", TEETH_MIN)
    limits = {
        "min_area_px": params.number(manifest, "MIN_AREA_PX", MIN_AREA_PX),
        "max_components": params.count(manifest, "MAX_COMPONENTS", MAX_COMPONENTS),
        "max_points": params.count(manifest, "MAX_POINTS", MAX_POINTS),
    }
    for key in ("max_components", "max_points"):
        # Both are slice lengths and divisors: zero would drop every contour
        # or divide by zero, so an unusable request keeps the default.
        if limits[key] < 1:
            limits[key] = {
                "max_components": MAX_COMPONENTS,
                "max_points": MAX_POINTS,
            }[key]
    rois = []
    bone_contours = []
    teeth_contours = []
    for frame in manifest["frames"]:
        values = raw_frame(workdir, frame)
        for polygon in contours_of(values, bone_min, bone_max, limits):
            bone_contours.append({"frame": frame["index"], "points": polygon})
        for polygon in contours_of(values, teeth_min, 65535, limits):
            teeth_contours.append({"frame": frame["index"], "points": polygon})
    if bone_contours:
        rois.append(
            {
                "name": "BONE",
                "color": BONE_COLOR,
                "interpreted_type": "ORGAN",
                "generation_algorithm": "AUTOMATIC",
                "description": "threshold bone window, demo only",
                "contours": bone_contours,
            }
        )
    if teeth_contours:
        rois.append(
            {
                "name": "TEETH",
                "color": TEETH_COLOR,
                "interpreted_type": "ORGAN",
                "generation_algorithm": "AUTOMATIC",
                "description": "threshold teeth window, demo only",
                "contours": teeth_contours,
            }
        )
    return {
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {
            "script": "RTSTRUCT-contours.py",
            "script_version": "0.1.1-demo",
            "model": "threshold-contours",
        },
        # An rtstruct block with no ROIs is a parse error by design ("a
        # structure set with no ROIs is not a result"), so a slice stack
        # with no bone in any window must not emit the block at all.
        "rtstruct": (
            {"label": "BONE", "name": "Threshold contours", "rois": rois}
            if rois
            else None
        ),
    }


def main(workdir):
    result = build(pathlib.Path(workdir))
    if result["rtstruct"] is None:
        del result["rtstruct"]
    workdir = pathlib.Path(workdir)
    tmp = workdir / "result.json.tmp"
    tmp.write_text(json.dumps(result, indent=2))
    tmp.rename(workdir / "result.json")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <workdir>", file=sys.stderr)
        raise SystemExit(2)
    main(sys.argv[1])
