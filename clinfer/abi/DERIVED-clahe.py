#!/usr/bin/env python3
"""CLAHE: a contrast-limited adaptive histogram equalisation filter.

One derived frame per input frame, the same size as the input. This is the
smallest complete processor: it reads the manifest, transforms every
frame, writes the replacement frames and returns a result that names them.

Run by hand from the work directory, which is also how clinfer runs it:

    python DERIVED-clahe.py /path/to/workdir

See ../abi/README.md for the contract.
"""

import json
import pathlib
import sys

import cv2
import numpy as np

import params

# The clip limit trades local contrast against noise amplification. 2.0 is a
# conventional starting point for radiographs; a site tunes it in its own copy.
# The requester may override either value through the workitem's scheduled
# processing parameters, and the constant below is the default either way.
CLIP_LIMIT = 2.0
TILE_GRID = (8, 8)


def process_batch(workdir):
    workdir = pathlib.Path(workdir)
    manifest = json.loads((workdir / "manifest.json").read_text())

    clip_limit = params.number(manifest, "CLIP_LIMIT", CLIP_LIMIT)
    tile_grid = params.grid(manifest, "TILE_GRID", TILE_GRID)

    clahe = cv2.createCLAHE(clipLimit=clip_limit, tileGridSize=tile_grid)
    files = []

    for frame in manifest["frames"]:
        raw = (workdir / "frames" / frame["file"]).read_bytes()

        # The dtype is the frame's own bit depth, not a guess: the manifest
        # declares bits allocated, and getting this wrong would reinterpret
        # every pixel rather than fail.
        dtype = np.uint8 if frame["bits"] == 8 else np.uint16
        image = np.frombuffer(raw, dtype=dtype).reshape(
            frame["height"], frame["width"]
        )

        # CLAHE works on a normalized image and is scale invariant, so the
        # 16 bit range is preserved rather than converted to 8 bit: the
        # output must stay the same bit depth as its input.
        out = clahe.apply(image)

        # Derived frames are written where the result says they are, and the
        # byte count must equal the source frame's exactly.
        target = workdir / frame["file"]
        target.write_bytes(out.astype(dtype).tobytes())
        assert target.stat().st_size == frame["bytes"], frame["file"]
        # The schema wants one object per file: the name plus any declared
        # geometry. A filter that preserves geometry (this one) declares
        # nothing, so the object carries the name alone.
        files.append({"file": frame["file"]})

    return {
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {
            "script": "DERIVED-clahe.py",
            "script_version": "1.0.0",
            "model": "clahe",
            "model_version": f"clip{clip_limit}",
        },
        "derived": {"files": files},
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
