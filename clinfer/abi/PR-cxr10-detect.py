#!/usr/bin/env python3
"""CXR10 detector -> `pr` boxes, for the DECODED 3-output OpenVINO IR.

Unlike the generic PR-openvino-detect.py example (which reads ONE [N,6]
tensor: x1,y1,x2,y2,conf,label), the shipped cxr10 model is the decoded
torchvision-style export with THREE outputs and a 3-channel 640x640 input:

    boxes   FP32 [-1, 4]   x1,y1,x2,y2 in 640x640 letterbox space
    labels  I64  [-1]      class index into the labels file
    scores  FP32 [-1]      confidence

Input preprocessing mirrors tools/ml/score_one.py (the verified reference):
grayscale frame -> gray replicated into 3 channels -> aspect-preserving
letterbox onto 640x640 (black pad) -> normalize to [0,1] -> NCHW. Boxes are
mapped back into the SOURCE frame's pixel space before being reported, which
is the space the ABI contract (README 4.3) requires.

Run by hand from the work directory, which is also how clinfer runs it:

    python PR-cxr10-detect.py /path/to/workdir

See ../abi/README.md for the contract.
"""

import json
import pathlib
import sys

import cv2
import numpy as np
import openvino as ov

import params

# A site points these at its own exported IR file and labels. Paths are
# absolute because the working directory is the work directory.
MODEL_XML = "/opt/clinfer/models/detector.xml"
LABELS = "/opt/clinfer/models/detector.labels"
INPUT_SIZE = 640
CONFIDENCE = 0.25


def _to_gray8(raw, bits, height, width):
    dtype = np.uint8 if bits == 8 else np.uint16
    image = np.frombuffer(raw, dtype=dtype).reshape(height, width)
    if image.dtype == np.uint8:
        return image
    # Full-range linear scale 16-bit -> 8-bit: the model was trained on
    # 8-bit frames. A site with windowed 16-bit data pre-windows in its own
    # copy instead.
    return np.asarray(image.astype(np.float32) / 257.0, dtype=np.uint8)


def _preprocess(gray):
    h, w = gray.shape
    scale = INPUT_SIZE / max(h, w)
    nw = max(1, int(round(w * scale)))
    nh = max(1, int(round(h * scale)))
    resized = cv2.resize(gray, (nw, nh), interpolation=cv2.INTER_LINEAR)
    canvas = np.zeros((INPUT_SIZE, INPUT_SIZE), dtype=np.uint8)
    top = (INPUT_SIZE - nh) // 2
    left = (INPUT_SIZE - nw) // 2
    canvas[top:top + nh, left:left + nw] = resized
    # Gray replicated into 3 channels, then normalized: exactly what
    # score_one.py feeds (.convert("RGB") then / 255.0).
    rgb = np.stack([canvas, canvas, canvas], axis=-1).astype(np.float32) / 255.0
    tensor = np.transpose(rgb, (2, 0, 1))[np.newaxis, ...]
    # The EFFECTIVE per-axis ratios: the resize dimensions are rounded, so
    # mapping boxes back through the pre-rounding scale would drift by a
    # fraction of a pixel on frames whose aspect ratio does not divide 640.
    return tensor, nw / w, nh / h, top, left


def process_batch(workdir):
    workdir = pathlib.Path(workdir)
    manifest = json.loads((workdir / "manifest.json").read_text())

    model_xml = params.text(manifest, "MODEL_XML", MODEL_XML)
    labels_path = params.text(manifest, "LABELS", LABELS)
    confidence_floor = params.number(manifest, "CONFIDENCE", CONFIDENCE)

    # Audit line: names the effective knobs, so a parameter-passing run is
    # verifiable from the log (default vs requester-supplied values).
    print(f"cxr10 detect: model={model_xml} confidence={confidence_floor}")

    core = ov.Core()
    compiled = core.compile_model(model_xml, "CPU")
    infer = compiled.create_infer_request()
    labels = pathlib.Path(labels_path).read_text().splitlines()

    boxes = []
    for frame in manifest["frames"]:
        raw = (workdir / "frames" / frame["file"]).read_bytes()
        gray = _to_gray8(
            raw, int(frame["bits"]), int(frame["height"]), int(frame["width"])
        )
        tensor, scale_x, scale_y, top, left = _preprocess(gray)

        infer.set_input_tensor(ov.Tensor(tensor))
        infer.infer()

        det_boxes = infer.get_tensor("boxes").data
        det_labels = infer.get_tensor("labels").data
        det_scores = infer.get_tensor("scores").data

        for box, label_idx, score in zip(det_boxes, det_labels, det_scores):
            confidence = float(score)
            if confidence < confidence_floor:
                continue
            x1, y1, x2, y2 = (float(v) for v in box)
            boxes.append(
                {
                    # A box belongs to ONE frame, 1-based (schema.rs refuses
                    # 0 or a fractional index).
                    "frame": int(frame["index"]),
                    "label": labels[int(label_idx)],
                    "confidence": confidence,
                    # 640x640 letterbox space -> source frame pixel space.
                    "x1": (x1 - left) / scale_x,
                    "y1": (y1 - top) / scale_y,
                    "x2": (x2 - left) / scale_x,
                    "y2": (y2 - top) / scale_y,
                }
            )

    return {
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {
            "script": "PR-cxr10-detect.py",
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
