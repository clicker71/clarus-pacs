#!/usr/bin/env python3
"""Chest X-ray segmentation: a DEMO-ONLY clinical model producer.

DISCLAIMER (from the model card of ianpan/chest-x-ray-basic, verbatim):

    for demonstration and research purposes only, NOT approved by any
    regulatory agency for clinical use; the user assumes any and all
    responsibility

clinfer makes no clinical claim either: it is the transport, not a
diagnostic device. This script exists to show that a real network reaches
the same `seg` schema key as the threshold filter, through the same
transport, with no clinfer-side difference.

WEIGHTS ARE NOT SHIPPED and are not redistributed by this repository. The
site downloads the model itself, into its own venv, as an operator-trusted
artifact. The model card states no license and its training masks come from
CheXmask (PhysioNet, non-commercial); the weights therefore likely inherit
non-commercial restrictions. That affects REDISTRIBUTION only, which this
repository does not do - but check the license tab yourself before any use
beyond an internal demonstration.

The checkpoint is a safetensors checkpoint shipped with its own
`modeling.py`, which is loaded with `trust_remote_code=True`: that file
is a program, not just data, and is executed when the model loads. Only
ever point this script at a model directory you downloaded yourself from
the model's own repository.

Site setup (see ../abi/README.md):

    1. Site-managed venv with the runtime deps:
       torch (CPU) + transformers + timm + albumentations.
    2. Download ianpan/chest-x-ray-basic INTO THE SITE, not this repo:
       config.json, model.safetensors, modeling.py, configuration.py,
       unet.py must sit side by side in one directory.
    3. Run with CHEST_SEG_MODEL pointing at that DIRECTORY:

    CHEST_SEG_MODEL=/opt/site/models/chest-x-ray-basic \
        python SEG-DX-chest.py /path/to/workdir

See ../abi/README.md for the contract.
"""

import json
import os
import pathlib
import sys

import numpy as np

# Where the site put the model. No default: a path this script invented
# would be a path that silently loads the wrong file on someone's machine.
MODEL_ENV = "CHEST_SEG_MODEL"

# The three classes the ianpan/chest-x-ray-basic model predicts, in channel
# order. argmax over the three channels gives the segment number.
CLASSES = [
    ("RIGHT_LUNG", "right lung"),
    ("LEFT_LUNG", "left lung"),
    ("HEART", "heart"),
]

# SRT codes for the declared segments. The category is anatomical structure
# and the type names the organ; both are required by the ABI.
SRT = "SRT"
# CID 7150: T-D0005 is "Anatomical Structure". T-D0050 is "Tissue", which
# is what this constant used to carry while claiming the other meaning.
CATEGORY = {"code": "T-D0005", "scheme": SRT, "meaning": "Anatomical Structure"}
TYPES = [
    # The DICOM/SNOMED-French scheme (SRT) has no per-lung code that every
    # reader resolves, so the laterality is carried in the label and the
    # type stays the generic lung. A site with its own vocabulary edits
    # this table in its own copy.
    {"code": "T-28000", "scheme": SRT, "meaning": "Lung"},
    {"code": "T-28000", "scheme": SRT, "meaning": "Lung"},
    {"code": "T-32000", "scheme": SRT, "meaning": "Heart"},
]
# CIELab as DICOM encodes it (PS3.3 C.10.7.1.1, US values): L* x 655.35,
# a*/b* shifted by 0x8080 and scaled by 257 - the scaling dcm4che and
# Weasis decode. Plain [255,0,0] would decode as nearly black.
def lab(L, a, b):
    return [round(L * 655.35), round(a * 257 + 0x8080), round(b * 257 + 0x8080)]
COLORS = [
    lab(53.24, 80.09, 67.20),   # right lung: red
    lab(32.30, 79.19, -107.86), # left lung: blue
    lab(87.74, -86.18, 83.18),  # heart: green
]


def load_model():
    """Load the site's model directory, or say exactly what to do about it.

    The failure has to be actionable: an operator who sees this message
    should know which environment variable to set and where the model is
    expected to come from, rather than reading a stack trace.
    """
    from transformers import AutoModel

    path = os.environ.get(MODEL_ENV)
    if not path:
        raise SystemExit(
            f"{MODEL_ENV} is not set.\n"
            "This script does not ship model weights. Download "
            "ianpan/chest-x-ray-basic into the site's own directory "
            "(config.json, model.safetensors, modeling.py, "
            "configuration.py, unet.py), then set "
            f"{MODEL_ENV} to that directory."
        )
    directory = pathlib.Path(path)
    if not (directory / "config.json").is_file():
        raise SystemExit(f"{MODEL_ENV}={path} is not a model directory")
    # trust_remote_code executes the model's modeling.py: trusted site
    # artifact only (see the header).
    model = AutoModel.from_pretrained(str(directory), trust_remote_code=True)
    model.eval()
    return model


def to_u8(image):
    """The model's own DICOM loader normalises to [0, 255] uint8, so a
    12/16-bit source frame follows the same path: min-max, not a shift."""
    if image.dtype == np.uint8:
        return image
    values = image.astype(np.float64)
    low, high = values.min(), values.max()
    if high > low:
        values = (values - low) * 255.0 / (high - low)
    return values.astype(np.uint8)


def predict(model, image):
    """The model's per-class mask for one frame, upsampled to its size.

    Mirrors the model card's own inference code: `model.preprocess` does
    the albumentations Resize(320, 320), the forward pass returns
    `out["mask"]` as a softmax over four classes, and the argmax over
    the channel dimension yields the segment number: 1 = right lung,
    2 = left lung, 3 = heart, 0 = background.
    """
    import cv2
    import torch

    resized = model.preprocess(to_u8(image))
    batch = torch.from_numpy(resized).unsqueeze(0).unsqueeze(0).float()
    with torch.inference_mode():
        out = model(batch)
    mask = out["mask"].argmax(1)[0].cpu().numpy().astype(np.uint8)
    if mask.shape != image.shape:
        # NEAREST keeps class labels crisp; the SEG frame must cover the
        # source geometry, so the model's 320x320 grid is upsampled.
        mask = cv2.resize(
            mask, (image.shape[1], image.shape[0]), interpolation=cv2.INTER_NEAREST
        )
    return mask


def pack_masks(segments, frame):
    """One packed bit plane per segment, in row-major mask order."""
    import numpy as np

    packed = []
    for number in range(1, len(CLASSES) + 1):
        plane = (segments == number).astype(np.uint8)
        packed.append(np.packbits(plane, axis=1).tobytes())
    return packed


def process_batch(workdir):
    import numpy as np

    workdir = pathlib.Path(workdir)
    manifest = json.loads((workdir / "manifest.json").read_text())
    model = load_model()

    # The ABI packs ONE bit plane per segment into a single mask file per
    # frame, so each frame's file holds the segment planes one after the
    # other: the first plane is segment 1, the second segment 2, and so on.
    # clinfer then emits ONE FRAME PER SEGMENT (PS3.3 C.8.20.3.1), naming
    # each frame's segment in its per-frame functional group.
    files = []
    for index, frame in enumerate(manifest["frames"]):
        raw = (workdir / "frames" / frame["file"]).read_bytes()
        # PS3.3 C.7.6.3.1.2: PixelRepresentation=1 means the samples are two's
        # complement, so a signed frame read as unsigned turns air into a value
        # far above everything else and the model sees inverted contrast.
        if frame["bits"] == 8:
            dtype = np.int8 if frame.get("signed") else np.uint8
        else:
            dtype = np.int16 if frame.get("signed") else np.uint16
        image = np.frombuffer(raw, dtype=dtype).reshape(
            frame["height"], frame["width"]
        )

        segments = predict(model, image)
        planes = pack_masks(segments, frame)

        name = f"seg-{index:04d}.raw"
        (workdir / name).write_bytes(b"".join(planes))
        files.append({"file": name, "frame": index + 1})

    return {
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {
            "script": "SEG-DX-chest.py",
            "script_version": "0.2.0-demo",
            "model": "ianpan/chest-x-ray-basic",
            "model_version": "demo-only",
        },
        "seg": {
            "segments": [
                {
                    "label": label,
                    "category": CATEGORY,
                    "type": TYPES[position],
                    "color": COLORS[position],
                    "algorithm": "AUTOMATIC",
                }
                for position, (label, _meaning) in enumerate(CLASSES)
            ],
            # The three masks are disjoint by construction (argmax).
            "overlap": "NO",
            "files": files,
        },
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
