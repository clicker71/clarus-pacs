#!/usr/bin/env python3
"""Threshold masker: the reference segmentation producer.

Pixels above a threshold become one binary mask per frame, packed one bit
per pixel. This is the smallest possible segmentation processor and it is
deliberately not a model: no weights, no GPU, no learned parameters. Its
job is to prove the runtime is model-agnostic - a filter and a network
reach the same `seg` schema key with no transport difference.

The mask is packed with numpy.packbits, which pads each row to a byte
boundary. That is exactly the layout PS3.3 C.8.20.2 requires for a
Bits Allocated 1 segmentation, so clinfer copies the bytes into Pixel Data
without touching them.

Run by hand from the work directory, which is also how clinfer runs it:

    python SEG-threshold.py /path/to/workdir

See ../abi/README.md for the contract.
"""

import json
import pathlib
import sys

import numpy as np

import params

# The binarization threshold, in the frame's own units. A site tunes this in
# its own copy: it is a display-neutral number, not a clinical one. The
# requester may override it through the workitem's parameters.
THRESHOLD = 128

# The segment this producer declares. Both the category and the type are
# required by the ABI, and the standard's own vocabulary is used rather
# than a site-invented one. M-01000 is "Morphologically Altered Structure"
# in the SRT coding scheme (PS3.16).
CATEGORY = {
    "code": "M-01000",
    "scheme": "SRT",
    "meaning": "Morphologically Altered Structure",
}
TYPE = {
    "code": "M-01000",
    "scheme": "SRT",
    "meaning": "Morphologically Altered Structure",
}
# Recommended display colour in the DICOM CIELab encoding (PS3.3 C.10.7.1.1,
# L* x 655.35, a*/b* shifted by 0x8080 and scaled by 257). This is the encoding
# a SEG reader decodes - the plain RGB triple [255,0,0] would decode as nearly
# black. Same conversion the chest producer uses.
COLOR = [round(53.24 * 655.35), round(80.09 * 257 + 0x8080), round(67.20 * 257 + 0x8080)]


def process_batch(workdir):
    workdir = pathlib.Path(workdir)
    manifest = json.loads((workdir / "manifest.json").read_text())

    threshold = params.number(manifest, "THRESHOLD", THRESHOLD)

    files = []
    for index, frame in enumerate(manifest["frames"]):
        raw = (workdir / "frames" / frame["file"]).read_bytes()

        # The dtype is the frame's own bit depth, declared in the manifest.
        dtype = np.uint8 if frame["bits"] == 8 else np.uint16
        image = np.frombuffer(raw, dtype=dtype).reshape(
            frame["height"], frame["width"]
        )

        # One mask, one bit per pixel, packed along each row.
        mask = (image > threshold).astype(np.uint8)
        packed = np.packbits(mask, axis=1)

        # The mask is a new file, never the frame itself: a derived object
        # may not overwrite the input the run was given.
        name = f"mask-{index:04d}.raw"
        (workdir / name).write_bytes(packed.tobytes())
        files.append({"file": name, "frame": index + 1})

    return {
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {
            "script": "SEG-threshold.py",
            "script_version": "1.0.0",
            "model": "threshold",
            "model_version": f"t{THRESHOLD}",
        },
        "seg": {
            "segments": [
                {
                    "label": "THRESHOLD",
                    "category": CATEGORY,
                    "type": TYPE,
                    "color": COLOR,
                }
            ],
            "files": files,
        },
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
