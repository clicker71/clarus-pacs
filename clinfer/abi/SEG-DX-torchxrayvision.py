#!/usr/bin/env python3
"""Chest X-ray anatomy segmentation: PSPNet -> a seg block per frame.

Runs torchxrayvision's chestx_det PSPNet over the workdir's chest
radiograph and emits a `seg` block: 14 binary masks of the anatomical
structures the model predicts, one mask entry per source frame.

THE MASKS COME BACK ON THE SOURCE FRAME'S GRID. PSPNet runs at 512x512
after a center crop of the long dimension, and this script inverts exactly
that chain before packing, so every mask is the size and shape of the frame
it describes. That is not a convenience: the ABI's `seg` block carries NO
geometry at all (see ../abi/README.md 4.6), and clinfer stamps the object's
Orientation, Position and spacing from the SOURCE instance. A mask on its
own 512 grid laid over a 2000x2000 image would be blocky and would need the
producer to compute geometry - which is the class of bug that put geometry
inside clinfer in the first place.

DISCLAIMER: no clinical claim is made or implied. PSPNet is a research
model; clinfer is transport, not a diagnostic device.

WEIGHTS ARE NOT SHIPPED by this repository. The site downloads them, or
copies a pre-populated ~/.torchxrayvision onto an offline machine.

Site setup (see ../abi/README.md):

    1. A site-managed venv with torch (CPU) and torchxrayvision:
           pip install --index-url https://download.pytorch.org/whl/cpu \\
               torch torchxrayvision
    2. Pre-download the weights ONCE, OUTSIDE a timed run: the first run
       fetches the PSPNet checkpoint, which alone can exceed a model's
       timeout. One manual run of this script does it.
    3. Only then enable the rule that routes to this producer.

Usage: python SEG-DX-torchxrayvision.py <workdir>

See ../abi/README.md for the contract.

WHAT CAN GO WRONG, AND WHAT TO DO ABOUT IT
------------------------------------------

1. 8 AND 16 BIT FRAMES ARE ACCEPTED; ANY OTHER DEPTH IS REFUSED. PSPNet
   expects the [-1024,1024] range its own forward pass maps to [0,1] - i.e.
   the HU-like scale a radiograph is stored at in a 16-bit instance, which is
   therefore used as-is. An 8-bit frame is on [0,255], so it is STRETCHED to
   the model's range by a fixed gain rather than being fed as-is; the gain is
   stated here because it is part of what the model expects, not a site knob.
   A frame whose bit depth is neither 8 nor 16 is refused.

2. THE OUTPUT IS LOGITS, NOT PROBABILITIES. PSPNet's forward pass ends in
   the network's own output with NO sigmoid or softmax: the checkpoint is a
   multi-label BCE model, so its 14 planes are independent and its raw
   values are unbounded (measured min/max about -21.8/-4.4 on a zeros
   input). Thresholding those at 0.5 would mark nearly every pixel. The
   producer applies torch.sigmoid ITSELF and then thresholds at 0.5, which
   is the same as thresholding the logits at 0 but is written out so the
   intent is not lost to a later "optimisation".

3. THE STRUCTURES OVERLAP. The 14 channels are independent, so one pixel
   can belong to several segments (a clavicle lies over a lung). The block
   therefore DECLARES overlap "YES" rather than letting a reader assume the
   masks partition the image.

4. THE ANATOMY TABLE IS CHECKED AGAINST THE MODEL, NOT TRUSTED. The 14
   channel names come from the model's own `targets`; the table in
   xrv_anatomy.json must cover exactly those, in that order. A release that
   reorders a class is a HARD STOP, because a mask labelled with another
   structure's name looks like a result.

5. EVERY CODE IN THE TABLE WAS VERIFIED against a public terminology
   server, with the source recorded per row. The International Edition has
   no per-side concept for lung, clavicle, scapula or lung root, so the
   side is carried in the segment LABEL and the type code stays generic -
   the same convention SEG-DX-chest.py already uses.
"""

import json
import os
import pathlib
import sys

import numpy as np
import skimage.transform
import torch

import params

# The model's input geometry and normalisation range, from its own forward
# pass: fix_resolution(..., 512) then (x + 1024) / 2048. These are part of
# WHAT THE MODEL IS, not site knobs.
SIZE = 512
HU_LOW = -1024.0
HU_HIGH = 1024.0

# The 8-bit source scale. An 8-bit frame spans [0,255]; mapping it onto the
# model's [-1024,1024] window keeps the contrast linear rather than
# fabricating a window the source never had.
U8_MAX = 255.0

# The binarization threshold, on the sigmoid, as the pack's own convention
# (chestx_anatomy.UNetResNet50 does `logits.sigmoid() > 0.5`).
THRESHOLD = 0.5

# The anatomy table, beside this script. A site with its own vocabulary
# edits its own copy.
ANATOMY_FILE = pathlib.Path(__file__).resolve().parent / "xrv_anatomy.json"

# Recommended display colours, CIELab as DICOM encodes it (PS3.3
# C.10.7.1.1, US values): L* x 655.35, a*/b* shifted by 0x8080 and scaled
# by 257 - the scaling dcm4che and Weasis decode. Plain [255,0,0] would
# decode as nearly black. A cycle of distinguishable hues, one per target.
def lab(lightness, a, b):
    return [round(lightness * 655.35), round(a * 257 + 0x8080), round(b * 257 + 0x8080)]


COLORS = [
    lab(80.0, 20.0, 60.0),
    lab(80.0, -40.0, 60.0),
    lab(70.0, 40.0, 40.0),
    lab(70.0, -30.0, 40.0),
    lab(60.0, 60.0, 40.0),
    lab(60.0, -60.0, 40.0),
    lab(55.0, 70.0, 10.0),
    lab(55.0, -70.0, 10.0),
    lab(50.0, 80.0, 30.0),
    lab(50.0, -80.0, 30.0),
    lab(45.0, 60.0, -40.0),
    lab(45.0, -60.0, -40.0),
    lab(90.0, 0.0, 0.0),
    lab(35.0, 10.0, -60.0),
]

# Environment override for the device, as in SR-DX-SCOORD-torchxrayvision.py.
DEVICE_ENV = "XRV_DEVICE"
DEVICE_DEFAULT = "cpu"


def read_anatomy():
    """The anatomy table, keyed by channel, with its own consistency checked."""
    table = json.loads(ANATOMY_FILE.read_text())
    targets = table["targets"]
    channels = [row["channel"] for row in targets]
    if channels != list(range(len(channels))):
        raise SystemExit(
            f"{ANATOMY_FILE.name} declares channels {channels}, which is not "
            "0..N-1 in order. A channel index that does not match the "
            "model's output order draws a structure's mask under another "
            "structure's name."
        )
    if len(COLORS) < len(targets):
        raise SystemExit(
            f"{len(COLORS)} colours are declared for {len(targets)} targets"
        )
    return table, targets


def check_against_model(targets, model):
    """The table must cover the model's own target list, in its own order."""
    model_labels = [str(name) for name in model.targets]
    table_labels = [row["model_label"] for row in targets]
    if model_labels != table_labels:
        raise SystemExit(
            f"the model reports {len(model_labels)} targets {model_labels}, "
            f"but {ANATOMY_FILE.name} declares {len(table_labels)} "
            f"{table_labels}. The table is keyed by channel index, so a "
            "reordered class would label every mask with the wrong "
            "structure. Fix the table to match this model before running."
        )


def read_frame(workdir, frame, hu_low, hu_high):
    """One frame's pixels, row-major, on the model's own intensity scale.

    Only 8 and 16 bit frames are accepted; the mapping onto the model's
    [hu_low, hu_high] window is stated rather than guessed. 16-bit data is used
    as-is because it already carries the HU-like scale the model expects;
    an 8-bit frame is stretched by a fixed gain so its contrast is linear
    in the source's own units.
    """
    bits = frame["bits"]
    if bits not in (8, 16):
        raise SystemExit(
            f"frame {frame['index']} ({frame['file']}) is {bits}-bit. This "
            "model reads an 8- or 16-bit radiograph; another depth would "
            "have to be rescaled by a rule this script would be inventing."
        )
    raw = (workdir / "frames" / frame["file"]).read_bytes()
    dtype = np.uint8 if bits == 8 else (np.int16 if frame.get("signed") else np.uint16)
    values = np.frombuffer(raw, dtype=dtype).astype(np.float32)
    values = values.reshape(frame["height"], frame["width"])
    if bits == 8:
        # [0,255] -> [hu_low, hu_high], the model's window, by a fixed gain.
        values = values / U8_MAX * (hu_high - hu_low) + hu_low
    return values


def load_model():
    """The segmenter, on the declared device, in eval mode."""
    from torchxrayvision.baseline_models.chestx_det import PSPNet

    device = torch.device(os.environ.get(DEVICE_ENV) or DEVICE_DEFAULT)
    model = PSPNet()
    model.eval()
    model.to(device)
    return model, device


def center_crop_box(height, width):
    """The square the model's center crop keeps, as (top, left, size).

    Copied from xrv's own XRayCenterCrop.crop_center: the crop size is the
    SHORTER side and the box is centered. Kept as its own function because
    the inverse mapping below has to use the same numbers.
    """
    size = min(height, width)
    top = height // 2 - size // 2
    left = width // 2 - size // 2
    return top, left, size


def masks_on_source_grid(probabilities, height, width, threshold):
    """The 14 probability planes, resampled onto the source frame's grid.

    The forward chain is: center crop the long dimension to a square, then
    resize that square to SIZE x SIZE. To come back, each SIZE x SIZE plane
    is resized up to the crop square and pasted into the crop box on an
    otherwise empty frame. Both steps use NEAREST so a binary mask stays
    binary and a structure's outline does not gain interpolated halos; the
    ABI's Bits Allocated 1 mask has no room for a fractional edge.

    skimage is used rather than cv2 because torchxrayvision already depends
    on it (its own XRayResizer defaults to the skimage engine) and the pack
    adds no dependency for this.
    """
    top, left, size = center_crop_box(height, width)
    on_grid = np.zeros((len(probabilities), height, width), dtype=bool)
    if size <= 0:
        return on_grid
    for index, plane in enumerate(probabilities):
        # Up to the crop square, nearest neighbour, preserving the label
        # values (preserve_range so a 0/1 mask is not renormalised).
        square = skimage.transform.resize(
            plane, (size, size), order=0, mode="edge", preserve_range=True,
            anti_aliasing=False,
        )
        on_grid[index, top : top + size, left : left + size] = square > threshold
    return on_grid


def pack_masks(binary_masks):
    """One packed bit plane per segment, in row-major mask order."""
    packed = []
    for plane in binary_masks:
        packed.append(np.packbits(plane.astype(np.uint8), axis=1).tobytes())
    return b"".join(packed)


def process_batch(workdir):
    workdir = pathlib.Path(workdir)
    manifest = json.loads((workdir / "manifest.json").read_text())
    frames = manifest.get("frames") or []
    if not frames:
        raise SystemExit("the manifest lists no frames")

    # The requester tunes the probability threshold and the 8-bit window; the
    # constants above are the defaults either way.
    threshold = params.number(manifest, "THRESHOLD", THRESHOLD)
    hu_low = params.number(manifest, "HU_LOW", HU_LOW)
    hu_high = params.number(manifest, "HU_HIGH", HU_HIGH)

    table, targets = read_anatomy()
    model, device = load_model()
    check_against_model(targets, model)

    files = []
    for index, frame in enumerate(frames):
        height, width = frame["height"], frame["width"]
        image = read_frame(workdir, frame, hu_low, hu_high)

        # THE CHAIN IS SPLIT BETWEEN HERE AND THE MODEL, and getting the
        # split wrong is what broke this script twice.
        #
        # THIS SCRIPT must center-crop to a square. PSPNet.forward calls
        # utils.fix_resolution, which RAISES when the input is not square
        # and says so ("Perform a center crop first"), so the crop is the
        # caller's job, not the model's.
        #
        # THE MODEL does the rest:
        #     x = x.repeat(1, 3, 1, 1)        - 1 channel in, 3 channels used
        #     x = utils.fix_resolution(x, 512, self)   - bilinear to 512x512
        #     x = (x + 1024) / 2048          - the [-1024,1024] window
        #     x = self.transform(x)          - torchvision Normalize
        # So this passes ONE channel of a SQUARE frame already on its
        # [-1024,1024] scale, and nothing else. Repeating to three channels
        # here made the model repeat it to nine and fail on a shape error.
        top, left, size = center_crop_box(height, width)
        cropped = image[top : top + size, left : left + size]

        tensor = (
            torch.from_numpy(cropped).float().unsqueeze(0).unsqueeze(0).to(device)
        )

        with torch.inference_mode():
            logits = model(tensor)

        # LOGITS -> probabilities, explicitly. See the header note 2: the
        # checkpoint applies no activation, so without this every pixel
        # would clear a 0.5 threshold.
        probabilities = torch.sigmoid(logits)[0].detach().cpu().numpy()
        if probabilities.shape[0] != len(targets):
            raise SystemExit(
                f"the model produced {probabilities.shape[0]} planes for "
                f"{len(targets)} declared targets; refusing to pair them up"
            )

        on_grid = masks_on_source_grid(probabilities, height, width, threshold)
        payload = pack_masks(on_grid)

        expected = len(targets) * height * ((width + 7) // 8)
        if len(payload) != expected:
            raise SystemExit(
                f"frame {frame['index']}: packed {len(payload)} bytes, but "
                f"{len(targets)} segments over {height}x{width} is {expected} "
                "bytes. Refusing to write a mask file the ABI will reject."
            )

        name = f"seg-{index:04d}.raw"
        (workdir / name).write_bytes(payload)
        files.append({"file": name, "frame": frame["index"]})

    category = table["category"]
    return {
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {
            "script": "SEG-DX-torchxrayvision.py",
            "script_version": "1.0.0",
            "model": "torchxrayvision chestx_det PSPNet",
            "model_version": _xrv_version(),
        },
        "seg": {
            "segments": [
                {
                    "label": target["label"],
                    "category": {
                        "code": category["code"],
                        "scheme": category["scheme"],
                        "meaning": category["meaning"],
                    },
                    "type": {
                        "code": target["code"],
                        "scheme": target["scheme"],
                        "meaning": target["meaning"],
                    },
                    "color": COLORS[position],
                    "algorithm": "AUTOMATIC",
                }
                for position, target in enumerate(targets)
            ],
            # The channels are independent, so their masks overlap.
            "overlap": "YES",
            "files": files,
        },
    }


def _xrv_version():
    import torchxrayvision as xrv

    return str(getattr(xrv, "__version__", "unknown"))

def main(workdir):
    result = process_batch(workdir)
    workdir = pathlib.Path(workdir)
    # Written under a temporary name and renamed into place.
    tmp = workdir / "result.json.tmp"
    tmp.write_text(json.dumps(result, indent=2))
    tmp.rename(workdir / "result.json")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <workdir>", file=sys.stderr)
        raise SystemExit(2)
    main(sys.argv[1])
