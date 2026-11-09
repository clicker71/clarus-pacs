#!/usr/bin/env python3
"""Super-resolution upscale: the first processor with a DECLARED geometry.

Every other filter in this directory returns frames the same size as its
input, so it declares nothing and clinfer takes the source geometry. This
one makes bigger frames, so it has to SAY so: the result declares the
upscaled width and height per output, and clinfer validates that the file
matches the declaration byte for byte.

That is the relaxed derived rule in one script: DICOM allows any size, so
upscale, downscale and crop are all legal; what is checked is that the file
is the shape it claims, because a mis-sized image is a misregistered one.

The weights are NOT shipped. OpenCV's dnn_superres needs a model file, and
the ready-made ones (EDSR, LapSRN) come from third-party repositories:
check the LICENSE of the .pb file you fetch before using it. OpenCV itself
is Apache-2.0.

Run by hand from the work directory, which is also how clinfer runs it:

    UPSCALE_MODEL=/path/to/EDSR_x2.pb python DERIVED-upscale.py /path/to/workdir

See ../abi/README.md for the contract.
"""

import json
import os
import pathlib
import sys

import cv2
import numpy as np

import params

# The site's super-resolution model file. No default: a path this script
# invented would silently load the wrong file on someone's machine.
MODEL_ENV = "UPSCALE_MODEL"

# The model's own scale factor. EDSR_x2 doubles, EDSR_x4 quadruples; the
# declared geometry must match what the model actually produces, so this is
# a constant a site edits together with the model it fetched.
SCALE = 2

# The algorithm name as OpenCV spells it, for the error message.
ALGORITHM = "edsr"


def load_model(scale, algorithm):
    """Load the site's super-resolution model, or say what to do.

    The failure has to be actionable: an operator who sees this message
    should know which environment variable to set and where the model is
    expected to come from.
    """
    path = os.environ.get(MODEL_ENV)
    if not path:
        raise SystemExit(
            f"{MODEL_ENV} is not set.\n"
            "This script does not ship model weights. Fetch an OpenCV "
            f"dnn_superres model (for example EDSR_x{scale}.pb), check its "
            f"license, then set {MODEL_ENV} to the .pb path."
        )
    if not pathlib.Path(path).is_file():
        raise SystemExit(f"{MODEL_ENV}={path} is not a file")
    return cv2.dnn_superres.DnnSuperResImpl_create(), path


def process_batch(workdir):
    workdir = pathlib.Path(workdir)
    manifest = json.loads((workdir / "manifest.json").read_text())

    scale = params.count(manifest, "SCALE", SCALE)
    algorithm = params.text(manifest, "ALGORITHM", ALGORITHM)
    if scale < 1:
        # A scale of zero would declare a zero-sized frame; the model itself
        # only knows the factors it was trained for, so an unusable value is
        # ignored in favour of the documented default rather than passed on.
        scale = SCALE

    upscaler, model_path = load_model(scale, algorithm)
    upscaler.readModel(model_path)
    upscaler.setModel(algorithm, scale)

    files = []
    for frame in manifest["frames"]:
        raw = (workdir / "frames" / frame["file"]).read_bytes()
        dtype = np.uint8 if frame["bits"] == 8 else np.uint16
        image = np.frombuffer(raw, dtype=dtype).reshape(
            frame["height"], frame["width"]
        )

        # dnn_superres takes and returns 8-bit images through this API, so
        # a 16-bit frame is scaled down for the model and scaled back up
        # afterwards. The declared geometry is what the OUTPUT actually is.
        if dtype == np.uint16:
            scaled = cv2.normalize(image, None, 0, 255, cv2.NORM_MINMAX).astype(
                np.uint8
            )
        else:
            scaled = image

        upscaled = upscaler.upsample(scaled)
        height, width = upscaled.shape[:2]

        if dtype == np.uint16:
            out = (
                upscaled.astype(np.uint16) * (65535 // 255)
            ).astype(np.uint16)
        else:
            out = upscaled.astype(dtype)

        name = f"upscaled-{frame['file']}"
        (workdir / name).write_bytes(out.tobytes())

        # The declaration is what clinfer validates against, so it has to
        # be the size the file really is - read back, not assumed.
        declared = {"file": name, "width": int(width), "height": int(height)}
        if dtype != np.uint8:
            declared["bits"] = int(dtype(0).nbytes * 8)
        files.append(declared)

    return {
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {
            "script": "DERIVED-upscale.py",
            "script_version": "1.0.0",
            "model": algorithm,
            "model_version": f"x{scale}",
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
