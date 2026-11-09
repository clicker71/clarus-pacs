#!/usr/bin/env python3
"""OpenVINO detection: boxes over every frame, as a `pr` block.

Shows the second result kind. A detector does not replace the pixels, so it
returns `pr.boxes` and no `derived` block. The frames are left untouched.

Run by hand from the work directory, which is also how clinfer runs it:

    python PR-openvino-detect.py /path/to/workdir

See ../abi/README.md for the contract.
"""

import json
import pathlib
import sys

import cv2
import numpy as np
import openvino as ov

import params

# A site points these at its own exported IR file and its own labels. The
# paths are absolute because the working directory is the work directory.
MODEL_XML = "/opt/clinfer/models/detector.xml"
LABELS = "/opt/clinfer/models/detector.labels"
INPUT_SIZE = (640, 640)
CONFIDENCE = 0.25


def process_batch(workdir):
    workdir = pathlib.Path(workdir)
    manifest = json.loads((workdir / "manifest.json").read_text())

    # The requester may point the run at another exported IR file and tune the
    # confidence; the constants above are the defaults either way. INPUT_SIZE
    # is NOT a parameter: the network's input geometry is fixed by the model
    # file itself, and a value the caller picked would only make the resize
    # disagree with it.
    model_xml = params.text(manifest, "MODEL_XML", MODEL_XML)
    labels_path = params.text(manifest, "LABELS", LABELS)
    confidence_floor = params.number(manifest, "CONFIDENCE", CONFIDENCE)

    core = ov.Core()
    model = core.compile_model(model_xml, "CPU")
    labels = pathlib.Path(labels_path).read_text().splitlines()
    output = model.output(0)

    boxes = []
    for frame in manifest["frames"]:
        raw = (workdir / "frames" / frame["file"]).read_bytes()
        dtype = np.uint8 if frame["bits"] == 8 else np.uint16
        image = np.frombuffer(raw, dtype=dtype).reshape(
            frame["height"], frame["width"]
        )

        # The network's own input geometry is the model's business; the
        # network's output must be mapped back to the SOURCE frame's pixels,
        # because that is the space the boxes are declared in.
        scale_x = frame["width"] / INPUT_SIZE[0]
        scale_y = frame["height"] / INPUT_SIZE[1]
        resized = np.asarray(cv2.resize(image, INPUT_SIZE), dtype=np.float32)
        tensor = resized[np.newaxis, np.newaxis, :, :]

        detections = model([tensor])[output][0]
        for detection in detections:
            confidence = float(detection[4])
            if confidence < confidence_floor:
                continue
            x1, y1, x2, y2 = (float(value) for value in detection[:4])
            boxes.append(
                {
                    # A box belongs to ONE frame, and the schema requires the
                    # 1-based index (schema.rs: `frame` is a POSITION, and 0 is
                    # refused). Without it the result is rejected as a missing
                    # key, so the helper could never produce a usable answer for
                    # a multi-frame series.
                    "frame": frame["index"],
                    "label": labels[int(detection[5])],
                    "confidence": confidence,
                    "x1": x1 * scale_x,
                    "y1": y1 * scale_y,
                    "x2": x2 * scale_x,
                    "y2": y2 * scale_y,
                }
            )

    return {
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {
            "script": "PR-openvino-detect.py",
            "script_version": "1.0.0",
            "model": pathlib.Path(model_xml).stem,
        },
        "pr": {"boxes": boxes},
    }


def main(workdir):
    result = process_batch(workdir)
    workdir = pathlib.Path(workdir)
    tmp = workdir / "result.json.tmp"
    tmp.write_text(json.dumps(result, indent=2))
    tmp.rename(workdir / "result.json")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <workdir>", file=sys.stderr)
        raise SystemExit(2)
    main(sys.argv[1])
