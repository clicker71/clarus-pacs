#!/usr/bin/env python3
"""Chest X-ray classification: 18 pathology probabilities as measurements.

Runs torchxrayvision's DenseNet over the workdir's chest radiograph and
emits a `measurements` block: one group per pathology, carrying the model's
own probability and the model's own operating threshold. Each group also
carries a `region`: the class activation map of that pathology at half its
own maximum, traced as a closed POLYLINE - see "WHAT THE REGION IS,
HONESTLY" below.

WHY A `measurements` BLOCK AND NOT A REPORT. The 18 outputs are numbers,
and a number belongs in a NUM content item, which only Enhanced SR
(1.2.840.10008.5.1.4.1.1.88.22 / TID 1500) can carry - Basic Text SR has no
NUM value type at all (PS3.3 A.35.1.3.1.1). So the numbers travel as
measurements and clinfer writes the Enhanced SR.
WHAT THE REGION IS, HONESTLY. The region is a class activation map, not
a segmentation and not a localization claim: it shows where the model's
evidence for that class concentrates, not where the disease is. The model
head pools the last feature maps with a global average and feeds a linear
layer, and for that head the closed-form Grad-CAM of a class is the
classifier row applied to the feature maps - computed directly from the
weights, with no backward pass and no randomness. The map is normalised
by its own maximum and thresholded at half of it, a deterministic level
set, not a clinical one; the outline is the trace of the LARGEST
component of that level set, the same reduction SR-DX-torchxrayvision-regions.py applies
to its masks. A class whose map has no positive value anywhere yields no
region (its probability still travels); the skip is printed to stderr.
DISCLAIMER: no clinical claim is made or implied. clinfer is transport,
not a diagnostic device, and it stores what the model emits without
judging it. The model's accuracy on this corpus is poor and is not the
subject of this script; see the report for the measured top-1 agreement.

WEIGHTS ARE NOT SHIPPED by this repository. The site downloads them, or
copies a pre-populated ~/.torchxrayvision onto an offline machine.

Site setup (see ../abi/README.md):

    1. A site-managed venv with torch (CPU) and torchxrayvision.
       Install torch from the CPU index, or pip pulls the CUDA wheels:

           pip install --index-url https://download.pytorch.org/whl/cpu \\
               torch torchxrayvision

    2. Pre-download the weights ONCE, OUTSIDE a timed run: the first run
       fetches ~140 MB, which alone can exceed a model's timeout and would
       cancel the workitem for a reason that has nothing to do with the
       study. One manual run of this script does it.

    3. Only then enable the rule that routes to this producer.

Usage: python SR-DX-SCOORD-torchxrayvision.py <workdir>

See ../abi/README.md for the contract.

WHAT CAN GO WRONG, AND WHAT TO DO ABOUT IT
------------------------------------------

1. A 16-BIT FRAME IS REFUSED, NOT RESCALED. This model's own pipeline
   normalises to [0,255] and applies a fixed mean/std, so it expects the
   8-bit presentation a radiograph is displayed at. A 16-bit frame carries
   a wider window in a different encoding, and silently dividing it into
   [0,255] would change the contrast the model sees - a wrong result
   stored is worse than a cancelled workitem. The refusal names the frame.

2. THE LABEL TABLE IS CHECKED AGAINST THE MODEL, NOT TRUSTED. The 18
   channel names come from the model's own `pathologies`; the table in
   xrv_labels.json must cover exactly those, in that order. A release that
   reorders or renames a class is a HARD STOP, because a probability
   labelled with the wrong finding looks like a result.

3. EVERY CODE IN THE TABLE WAS VERIFIED against a public terminology
   server, and the source is recorded per row. Two of the 18 are marked as
   approximations because SNOMED CT has no exact concept; the table says
   so rather than presenting a near-match as exact.

4. NO THRESHOLD JUDGEMENT IS MADE HERE. `op_threshs` is the model's own
   per-class operating point (0.0022..0.2020, not 0.5) and it is written
   beside the probability as data. Deciding whether a class is positive is
   the reader's business, not the transport's.

5. THE REGION COORDINATES ARE SOURCE PIXELS, NOT MILLIMETRES, and the
   region itself is a saliency artefact: the class activation map at half
   its own maximum, reduced to an outline the same way SR-DX-torchxrayvision-regions.py
   reduces its masks. It says where the model's evidence concentrated,
   not where the finding is, and a class whose map is empty is skipped
   (stderr) while its probability still travels.
"""

import json
import os
import pathlib
import sys

import params

# The weights name. torchxrayvision resolves it to a URL and caches the file
# under ~/.torchxrayvision/models_data/.
WEIGHTS = "densenet121-res224-all"

# The label table, beside this script. A site with its own vocabulary edits
# its own copy.
LABELS_FILE = pathlib.Path(__file__).resolve().parent / "xrv_labels.json"

# The unit every probability in the block carries. UCUM `1` is "no units",
# which is the honest answer for a fraction: clinfer does not guess, and a
# probability is not a percentage. The ABI makes `unit` required for exactly
# this reason (see ../abi/README.md).
UNIT = {"code": "1", "scheme": "UCUM", "meaning": "no units"}

# The measurement concept, from the TID 1500 measurement template. 122157 is
# "Probability" in the DCM scheme, which is what a prediction is here.
CONCEPT = {"code": "122157", "scheme": "DCM", "meaning": "Probability"}

# The model input size and normalisation, from the model's own docs and its
# `xrv.datasets.XRayCenterCrop` / `XRayResizer` pipeline. Kept as constants
# because they are part of WHAT THE MODEL IS, not site knobs.
RESIZE = 224

# The saliency level set, on the map normalised by its own maximum. Half of
# the class's own peak is a deterministic, scale-free line; a saliency map
# has no operating point, only a level.
CAM_THRESHOLD = 0.5

# The vertex budget for the traced region, the same 48 SR-DX-torchxrayvision-regions.py uses:
# Graphic Data grows 8 bytes per point and the outline is SIMPLIFIED to fit
# rather than dropped.
MAX_VERTICES = 48

# Environment override for the device. CPU is the default because a demo
# stand is a CPU box and a silently-chosen CUDA device is how a run fails on
# a machine that has no GPU.
DEVICE_ENV = "XRV_DEVICE"
DEVICE_DEFAULT = "cpu"


def read_labels():
    """The label table, keyed by channel, with its own consistency checked."""
    table = json.loads(LABELS_FILE.read_text())
    labels = table["labels"]
    channels = [row["channel"] for row in labels]
    if channels != list(range(len(channels))):
        raise SystemExit(
            f"{LABELS_FILE.name} declares channels {channels}, which is not "
            "0..N-1 in order. A channel index that does not match the "
            "model's output order labels a probability with another "
            "finding's name, which is worse than no label."
        )
    return labels


def check_against_model(labels, model):
    """The table must cover the model's own class list, in its own order.

    The names on the left come from the model, so this compares the model's
    list against the table rather than the other way round: the table is the
    thing a human edited, and it is the thing that can silently drift.
    """
    model_labels = [str(name) for name in model.pathologies]
    table_labels = [row["model_label"] for row in labels]
    if model_labels != table_labels:
        raise SystemExit(
            f"the model reports {len(model_labels)} classes "
            f"{model_labels}, but {LABELS_FILE.name} declares "
            f"{len(table_labels)} {table_labels}. The table is keyed by "
            "channel index, so a reordered or renamed class would label "
            "every probability with the wrong finding. Fix the table to "
            "match this model before running."
        )


def read_frame(workdir, frame):
    """One frame's pixels, row-major. Only 8-bit is accepted; see the header.

    16-bit sources are REFUSED rather than scaled. The refusal names the
    frame so an operator can see which instance carried it.
    """
    import numpy as np

    bits = frame["bits"]
    if bits != 8:
        raise SystemExit(
            f"frame {frame['index']} ({frame['file']}) is {bits}-bit. This "
            "model reads the 8-bit presentation: its pipeline normalises to "
            "[0,255] with a fixed mean/std, so a 16-bit frame would have to "
            "be windowed or scaled first, and inventing that mapping here "
            "would change the contrast the model sees. Re-fetch 8-bit "
            "instances, or route this study to a producer built for its "
            "encoding."
        )
    raw = (workdir / "frames" / frame["file"]).read_bytes()
    values = np.frombuffer(raw, dtype=np.uint8)
    return values.reshape(frame["height"], frame["width"])


def load_model():
    """The classifier, on the declared device, in eval mode."""
    import torch
    import torchxrayvision as xrv

    device = torch.device(os.environ.get(DEVICE_ENV) or DEVICE_DEFAULT)
    model = xrv.models.DenseNet(weights=WEIGHTS)
    model.eval()
    model.to(device)
    return model, device


def probabilities(model, device, image):
    """The 18 sigmoid outputs for one frame, plus the head's feature maps.

    The model's own pipeline, in its own order: normalise 255 -> mean ->
    center crop -> resize. Reusing xrv's own transforms rather than
    reimplementing them is what keeps this script agreeing with the
    weights; a hand-rolled resize is how a run quietly diverges.

    These transforms are plain callables over a (1, H, W) numpy array, not
    torch Modules, so they are applied in sequence rather than composed
    into an nn.Sequential.

    The second return value is the activation the classifier pools - the
    output of `model.features`, captured with a forward hook on the SAME
    pass that produced the values, so the numbers and the saliency maps
    below describe one forward pass. One pass runs per frame.
    """
    import numpy as np
    import torch
    import torchxrayvision as xrv

    array = np.asarray(image, dtype=np.float32)
    array = xrv.datasets.normalize(array, 255)
    array = array[None, ...]
    array = xrv.datasets.XRayCenterCrop()(array)
    array = xrv.datasets.XRayResizer(RESIZE)(array)

    tensor = torch.from_numpy(array).unsqueeze(0).to(device)

    captured = {}

    def capture(module, inputs, output):
        captured["maps"] = output

    handle = model.features.register_forward_hook(capture)
    try:
        with torch.inference_mode():
            output = model(tensor)
    finally:
        handle.remove()

    values = output.detach().cpu().numpy().reshape(-1)
    maps = captured["maps"].detach().cpu().numpy()[0]
    return values, maps


def center_crop_box(height, width):
    """The square the model's center crop keeps, as (top, left, size).

    Copied from SR-DX-torchxrayvision-regions.py, which copied it from xrv's own
    XRayCenterCrop.crop_center; the inverse mapping below has to use the
    same numbers.
    """
    size = min(height, width)
    top = height // 2 - size // 2
    left = width // 2 - size // 2
    return top, left, size


def simplify_polygon(points, limit):
    """A closed polygon with at most `limit` vertices, same shape.

    Copied from SR-DX-torchxrayvision-regions.py. Uniform decimation of the vertex LIST,
    keeping the first point, so the outline stays closed and the kept
    count is always AT OR BELOW the limit.
    """
    if len(points) <= limit:
        return points
    step = -(-len(points) // limit)  # ceiling division
    kept = points[::step]
    return kept[:limit]


def outline_of(mask, max_vertices):
    """The outline of a mask's LARGEST connected component, in [col, row].

    Copied from SR-DX-torchxrayvision-regions.py rather than re-derived: two producers that
    both trace model output must trace it the same way. Returns `None`
    when the mask is empty. `find_contours` returns (row, column) pairs,
    so the pair is swapped on the way out: the ABI's point order is
    [column, row], the same order SCOORD's Graphic Data uses.
    """
    import numpy as np
    import skimage.measure

    if not mask.any():
        return None

    labeled = skimage.measure.label(mask, connectivity=2)
    if labeled.max() == 0:
        return None
    sizes = np.bincount(labeled.ravel())
    sizes[0] = 0  # background
    largest = int(sizes.argmax())
    component = labeled == largest

    contours = skimage.measure.find_contours(
        component.astype(np.float32), 0.5
    )
    if not contours:
        return None
    longest = max(contours, key=len)
    if len(longest) < 3:
        return None
    simplified = simplify_polygon(longest, max_vertices)
    points = [
        [float(round(point[1])), float(round(point[0]))] for point in simplified
    ]
    if len(points) >= 2 and points[0] != points[-1]:
        points[-1] = points[0]
    return points


def cams_on_source_grid(model, features, height, width):
    """One class activation map per class, on the SOURCE frame's grid.

    The model head pools the last feature maps with a global average and
    feeds a linear layer, so class c's logit is GAP(sum_k W[c,k] A_k + b_c)
    exactly. That inner sum IS Grad-CAM for this head - the gradient of
    the logit with respect to the maps is the classifier row - so it is
    computed here in closed form from the weights: no backward pass, no
    randomness.

    Each map is normalised by its own maximum and bilinearly resized to
    the center-crop square, then pasted into the crop box - the inverse of
    the model's own transform chain. Bilinear, not nearest: the maps are
    already smooth 7x7 planes, and a staircase upsampling would turn the
    outline into pixel cells.

    A class whose map has no positive value anywhere yields an all-zero
    plane: its probability travels, its region is skipped by the caller.
    """
    import numpy as np
    import skimage.transform

    weight = model.classifier.weight.detach().cpu().numpy()
    bias = model.classifier.bias.detach().cpu().numpy()
    # (18, 1024) x (1024, 7, 7) -> (18, 7, 7); the ReLU is Grad-CAM's own.
    maps = np.einsum("ck,khw->chw", weight, features)
    maps = maps + bias[:, None, None]
    np.maximum(maps, 0.0, out=maps)

    top, left, size = center_crop_box(height, width)
    on_grid = np.zeros((maps.shape[0], height, width), dtype=np.float32)
    if size <= 0:
        return on_grid
    for index, plane in enumerate(maps):
        peak = float(plane.max())
        if peak <= 0.0:
            continue  # no positive evidence anywhere: the plane stays zero
        normalised = plane / peak
        square = skimage.transform.resize(
            normalised,
            (size, size),
            order=1,
            mode="edge",
            preserve_range=True,
            anti_aliasing=False,
        )
        on_grid[index, top : top + size, left : left + size] = square
    return on_grid


def thresholds_of(model):
    """The model's own operating thresholds, or None when it declares none.

    `op_threshs` is part of the weights, not a knob. When a model does not
    carry them the key is omitted rather than filled with 0.5: a threshold
    this script invented would be the transport making a clinical claim.
    """
    thresholds = getattr(model, "op_threshs", None)
    if thresholds is None:
        return None
    return [float(value) for value in thresholds]


def build(workdir):
    """The whole producer: one frame, one inference pass, one measurements block."""
    workdir = pathlib.Path(workdir)
    manifest = json.loads((workdir / "manifest.json").read_text())
    frames = manifest.get("frames") or []
    if not frames:
        raise SystemExit("the manifest lists no frames")

    # The requester tunes the saliency threshold and the vertex budget; the
    # constants above are the defaults either way.
    cam_threshold = params.number(manifest, "CAM_THRESHOLD", CAM_THRESHOLD)
    max_vertices = params.count(manifest, "MAX_VERTICES", MAX_VERTICES)
    if max_vertices < 3:
        # A polygon needs three vertices to enclose anything, so a smaller
        # request keeps the default rather than emitting a degenerate region.
        max_vertices = MAX_VERTICES

    labels = read_labels()
    model, device = load_model()
    check_against_model(labels, model)

    # A radiograph is one image. A workdir with several frames classifies the
    # FIRST and says so; the block has no per-frame key, so claiming more
    # would be a block that describes one image while looking like it
    # describes the study.
    frame = frames[0]
    image = read_frame(workdir, frame)
    values, features = probabilities(model, device, image)
    thresholds = thresholds_of(model)

    if len(values) != len(labels):
        raise SystemExit(
            f"the model produced {len(values)} values for {len(labels)} "
            "labelled classes; refusing to pair them up"
        )

    cams = cams_on_source_grid(model, features, frame["height"], frame["width"])

    groups = []
    skipped = []
    for position, label in enumerate(labels):
        measurement = {
            "concept": CONCEPT,
            # Written verbatim: the model's own number, as a Python float.
            # No rounding to a "clinical" number of digits - the transport
            # does not decide how much of the model's output matters.
            "value": float(values[position]),
        }
        if thresholds is not None:
            measurement["threshold"] = thresholds[position]
        # The saliency region of this class: where the model's evidence for
        # it concentrated. A visual explanation attached to the number - the
        # map at half its own maximum, traced like the anatomy producer
        # traces its masks. clinfer writes it as the measurement's SCOORD,
        # which is what makes the finding link drawable in a reader.
        points = outline_of(cams[position] >= cam_threshold, max_vertices)
        if points is not None:
            measurement["region"] = {"type": "POLYLINE", "points": points}
        else:
            skipped.append(label["model_label"])
        groups.append(
            {
                "finding": {
                    "code": label["code"],
                    "scheme": label["scheme"],
                    # The model's own class name, so the row is honest about
                    # what the number is even when the concept is a
                    # near-match (see the table's 'match' column).
                    "meaning": label["model_label"],
                },
                "measurements": [measurement],
            }
        )

    if skipped:
        print(
            f"skipped {len(skipped)} class(es) whose saliency map is empty: "
            f"{', '.join(skipped)}",
            file=sys.stderr,
        )

    import torchxrayvision as xrv

    return {
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {
            "script": "SR-DX-SCOORD-torchxrayvision.py",
            "script_version": "1.1.0",
            "model": f"torchxrayvision DenseNet {WEIGHTS}",
            "model_version": str(getattr(xrv, "__version__", "unknown")),
        },
        "measurements": {
            "unit": UNIT,
            "groups": groups,
        },
    }


def main(workdir):
    result = build(workdir)
    workdir = pathlib.Path(workdir)
    # Written under a temporary name and renamed into place, so a reader sees
    # either no result.json or a complete one.
    tmp = workdir / "result.json.tmp"
    tmp.write_text(json.dumps(result, indent=2))
    tmp.rename(workdir / "result.json")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <workdir>", file=sys.stderr)
        raise SystemExit(2)
    main(sys.argv[1])
