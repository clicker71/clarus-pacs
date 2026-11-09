#!/usr/bin/env python3
"""Opaque-patch analytics: measurements as a Structured Report.

Threshold, find connected regions, measure them, and report the numbers as
Basic Text SR. No learned parameters and no weights: this is arithmetic,
and that is the point - it shows the same pipeline that carries a network's
boxes can carry a pure computation's answer.

The text is MEASUREMENTS, NOT FINDINGS. "3 opaque regions, largest area
412 px" is a measurement; "opacities in the lower lobe" is a clinical
finding, and this script does not make one. clinfer is the transport, not a
diagnostic device, so the wording stays on the mechanical side of that
line. A site that wants clinical vocabulary writes it in its own copy and
owns the claim.

Run by hand from the work directory, which is also how clinfer runs it:

    python SR-opacity.py /path/to/workdir

See ../abi/README.md for the contract.
"""

import json
import pathlib
import sys

import numpy as np
from scipy import ndimage

import params

# The binarization threshold, in the frame's own units. The requester may
# override it through the workitem's scheduled processing parameters.
THRESHOLD = 128


def describe(frames_dir, frame, index, total, threshold):
    """One frame's measurements, as text.

    Every number here is counted from the pixels. Nothing is inferred and
    nothing is named clinically, so the text cannot be mistaken for a
    reading.
    """
    raw = (frames_dir / frame["file"]).read_bytes()
    dtype = np.uint8 if frame["bits"] == 8 else np.uint16
    image = np.frombuffer(raw, dtype=dtype).reshape(frame["height"], frame["width"])

    mask = image > threshold
    # 8-connectivity: two pixels that touch at a corner are one region.
    labels, count = ndimage.label(mask, structure=np.ones((3, 3)))

    header = f"Frame {index + 1} of {total} ({frame['width']}x{frame['height']}):"
    if count == 0:
        return f"{header} no opaque regions above threshold {threshold}."

    areas = ndimage.sum_labels(
        np.ones_like(labels, dtype=np.float64), labels, index=range(1, count + 1)
    )
    centroids = ndimage.center_of_mass(
        np.ones_like(labels, dtype=np.float64), labels, index=range(1, count + 1)
    )
    boxes = ndimage.find_objects(labels)

    largest = int(np.argmax(areas))
    top, left = boxes[largest][0].start, boxes[largest][1].start
    bottom, right = boxes[largest][0].stop, boxes[largest][1].stop
    row, column = centroids[largest]

    return (
        f"{header} {count} opaque region(s) above threshold {threshold}. "
        f"Largest area {int(areas[largest])} px, "
        f"centroid ({row:.1f}, {column:.1f}), "
        f"bounding box ({left},{top})-({right},{bottom}). "
        f"Total opaque area {int(areas.sum())} px."
    )


def process_batch(workdir):
    workdir = pathlib.Path(workdir)
    manifest = json.loads((workdir / "manifest.json").read_text())

    threshold = params.number(manifest, "THRESHOLD", THRESHOLD)

    frames = manifest["frames"]
    frames_dir = workdir / "frames"
    lines = [
        describe(frames_dir, frame, index, len(frames), threshold)
        for index, frame in enumerate(frames)
    ]

    return {
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {
            "script": "SR-opacity.py",
            "script_version": "1.0.0",
            "model": "connected-components",
            "model_version": f"t{threshold}",
        },
        "sr": {"text": "\n".join(lines)},
    }


def main(workdir):
    result = process_batch(workdir)
    workdir = pathlib.Path(workdir)
    # Written under a temporary name and renamed into place: a reader then
    # sees either no result.json or a complete one.
    tmp = workdir / "result.json.tmp"
    tmp.write_text(json.dumps(result, indent=2))
    tmp.rename(workdir / "result.json")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <workdir>", file=sys.stderr)
        raise SystemExit(2)
    main(sys.argv[1])
