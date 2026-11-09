#!/usr/bin/env python3
"""Chest X-ray anatomy regions: PSPNet masks reduced to outlines.

Runs torchxrayvision's chestx_det PSPNet over the workdir's chest
radiograph and emits a `measurements` block carrying each structure's
coverage fraction (the NUM) plus a `pr` block carrying the traced outline
of the model's own mask as a presentation-state contour.

The split is deliberate. The NUMBER belongs to the Enhanced SR the
`measurements` block produces; the OUTLINE belongs to a presentation state,
which is where a viewer draws geometry over an image. clinfer keeps them in
two encoders because they are two different objects with two different
lifetimes.

WHAT THIS SCRIPT IS, HONESTLY. The outline is a DERIVED artefact, not a
model output: PSPNet emits a per-pixel mask, and this script reduces that
mask to the closed contour of its largest connected component. The
algorithm is deterministic - the same frame and weights give the same
outline - and it is documented below so a reader can reproduce or replace
it. The number in the NUM is the model's own coverage fraction, written
verbatim; the outline is a faithful trace OF THAT MASK at the level set
0.5, not an independent finding.

WHY A CLOSED POLYLINE. POLYLINE is the ONLY way DICOM expresses an
outline: PS3.3 C.18.6.1.2 enumerates the legal (0070,0023) Graphic Type
values as POINT, MULTIPOINT, POLYLINE, CIRCLE and ELLIPSE, and POLYGON is
not among them. A closed region is therefore a POLYLINE whose last point
repeats the first, which is exactly what this script emits. (An earlier
comment here claimed POLYGON was legal and merely un-drawable by Weasis
4.7.x. The Weasis half of that was true - PrGraphicUtil.getGraphicBuilder
has no POLYGON case and warns "Unsupported graphic type" - but the
standards half was wrong, and the ABI now refuses POLYGON by name.)

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

Usage: python SR-DX-torchxrayvision-regions.py <workdir>

See ../abi/README.md for the contract.

WHAT CAN GO WRONG, AND WHAT TO DO ABOUT IT
------------------------------------------

1. THE OUTPUT IS LOGITS, NOT PROBABILITIES. PSPNet's forward pass ends in
   the network's own output with NO sigmoid or softmax: the checkpoint is a
   multi-label BCE model, so its 14 planes are independent and its raw
   values are unbounded. Thresholding those at 0.5 would mark nearly every
   pixel. This script applies torch.sigmoid ITSELF and then thresholds at
   0.5, which is the same as thresholding the logits at 0 but is written
   out so the intent is not lost to a later "optimisation".

2. 8 AND 16 BIT FRAMES ARE BOTH ACCEPTED, ON THE MODEL'S OWN SCALE. This is
   the same rule SEG-DX-torchxrayvision.py applies, because it is the same
   PSPNet: a 16-bit frame already carries the HU-like range the forward pass
   maps to [0,1] and is used as-is, while an 8-bit frame is STRETCHED to that
   range by a fixed gain. Any other bit depth is refused rather than rescaled
   by a rule this script would be inventing.

3. A STRUCTURE WITH NO PIXELS IS SKIPPED, NOT INVENTED. A channel whose
   mask is empty everywhere yields no region and no measurement: an empty
   polygon is not a region, and a zero-coverage NUM with nothing to point
   at would be a claim about a structure the model did not find. The
   skipped labels are printed to stderr so the operator sees which ones,
   and `SEG-DX-torchxrayvision.py` remains the producer to use when the masks themselves
   are wanted.

4. THE COORDINATES ARE SOURCE PIXELS, NOT MILLIMETRES. The region travels
   on the source frame's own grid - the same grid `SEG-DX-torchxrayvision.py` packs masks
   on - and clinfer stamps the DICOM geometry from the source instances.
   Converting here would duplicate a transformation this script cannot do
   correctly: it never sees the Image Plane attributes.

5. THE ANATOMY TABLE IS CHECKED AGAINST THE MODEL, NOT TRUSTED. The 14
   channel names come from the model's own `targets`; the table in
   xrv_anatomy.json must cover exactly those, in that order. A release that
   reorders a class is a HARD STOP, because a region labelled with another
   structure's name looks like a result. Every code was verified against a
   public terminology server, with the source recorded per row.
"""

import json
import pathlib
import sys

import numpy as np
import skimage.measure
import skimage.transform
import torch

import params

# The model's input geometry and normalisation window, from its own forward
# pass: fix_resolution(..., 512) then (x + 1024) / 2048. Part of WHAT THE
# MODEL IS, not site knobs.
SIZE = 512
HU_LOW = -1024.0
HU_HIGH = 1024.0

# The 8-bit source scale, mapped onto the model's window by a fixed gain.
U8_MAX = 255.0

# The binarization threshold, on the sigmoid, as the pack's own convention
# (chestx_anatomy.UNetResNet50 does `logits.sigmoid() > 0.5`).
THRESHOLD = 0.5

# The contour level, in the same 0/1 space. 0.5 is the boundary of the
# thresholded mask, so the trace follows exactly what was counted.
CONTOUR_LEVEL = 0.5
# One zero-valued voxel ring around a component before the contour trace. A
# component touching the frame edge has no outside, so the level set runs off
# the image and comes back as an open trace; the ring gives it an edge to close
# against. One voxel is the minimum that changes the topology; more would waste
# work and would have to be subtracted from every vertex anyway.
PAD = 1
# The vertex budget. A PSPNet outline is a smooth blob and needs far fewer
# than this; the cap exists because Graphic Data grows 8 bytes per point and
# a reader draws the whole list. The polygon is SIMPLIFIED to fit rather
# than dropped, and the simplification keeps the endpoints, so the shape
# stays closed.
MAX_VERTICES = 48

# The unit every coverage fraction carries. UCUM `1` is "no units", which is
# the honest answer for a fraction: the ABI makes `unit` required so that
# `1` and `%` are never confused.
UNIT = {"code": "1", "scheme": "UCUM", "meaning": "no units"}

# The measured concept: (122027, DCM, "Anatomical Structure") is what the
# number is ABOUT - the fraction of the image this structure covers.
CONCEPT = {"code": "122027", "scheme": "DCM", "meaning": "Anatomical Structure"}

# The anatomy table, beside this script. A site with its own vocabulary
# edits its own copy.
ANATOMY_FILE = pathlib.Path(__file__).resolve().parent / "xrv_anatomy.json"

# Environment override for the device, as in SR-DX-SCOORD-torchxrayvision.py.
DEVICE_ENV = "XRV_DEVICE"
DEVICE_DEFAULT = "cpu"


def read_anatomy():
    """The anatomy table, with its own channel consistency checked."""
    table = json.loads(ANATOMY_FILE.read_text())
    targets = table["targets"]
    channels = [row["channel"] for row in targets]
    if channels != list(range(len(channels))):
        raise SystemExit(
            f"{ANATOMY_FILE.name} declares channels {channels}, which is not "
            "0..N-1 in order. A channel index that does not match the "
            "model's output order labels a region with another structure's "
            "name."
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
            "reordered class would label every region with the wrong "
            "structure. Fix the table to match this model before running."
        )


def read_frame(workdir, frame, hu_low, hu_high):
    """One frame's pixels, row-major, on the model's own intensity scale.

    Same rule as `SEG-DX-torchxrayvision.py`, and it has to stay the same: both
    drive the same PSPNet, so an 8-bit source is stretched by the fixed gain and
    a 16-bit source is used as-is, because it already carries the HU-like scale
    the model's forward pass expects. A second, subtly different mapping is how
    two producers of the same model drift apart, so this is a copy on purpose -
    with the gain and the accepted depths stated here rather than inferred.
    """
    bits = frame["bits"]
    if bits not in (8, 16):
        raise SystemExit(
            f"frame {frame['index']} ({frame['file']}) is {bits}-bit. This "
            "model reads an 8- or 16-bit radiograph; another depth would "
            "have to be rescaled by a rule this script would be inventing."
        )
    raw = (workdir / "frames" / frame["file"]).read_bytes()
    if bits == 8:
        dtype = np.int8 if frame.get("signed") else np.uint8
    else:
        dtype = np.int16 if frame.get("signed") else np.uint16
    values = np.frombuffer(raw, dtype=dtype).astype(np.float32)
    values = values.reshape(frame["height"], frame["width"])
    if bits == 8:
        # [0,255] -> [hu_low, hu_high], the model's window, by a fixed gain.
        values = values / U8_MAX * (hu_high - hu_low) + hu_low
    return values


def load_model():
    """The segmenter, on the declared device, in eval mode."""
    import os

    from torchxrayvision.baseline_models.chestx_det import PSPNet

    device = torch.device(os.environ.get(DEVICE_ENV) or DEVICE_DEFAULT)
    model = PSPNet()
    model.eval()
    model.to(device)
    return model, device


def center_crop_box(height, width):
    """The square the model's center crop keeps, as (top, left, size).

    Copied from SEG-DX-torchxrayvision.py, which copied it from xrv's own
    XRayCenterCrop.crop_center; the inverse mapping below has to use the
    same numbers, and this is proven identical to xrv's over odd shapes.
    """
    size = min(height, width)
    top = height // 2 - size // 2
    left = width // 2 - size // 2
    return top, left, size


def masks_on_source_grid(probabilities, height, width, threshold):
    """The 14 probability planes as masks on the SOURCE frame's grid.

    The forward chain is: center crop the long dimension to a square, then
    resize that square to SIZE x SIZE. To come back, each SIZE x SIZE plane
    is resized up to the crop square and pasted into the crop box on an
    otherwise empty frame. NEAREST keeps a structure's outline from gaining
    interpolated halos.

    This is the same inverse chain SEG-DX-torchxrayvision.py uses, deliberately: two
    producers of one model must agree on what the mask is, or their
    numbers describe different things.
    """
    top, left, size = center_crop_box(height, width)
    on_grid = np.zeros((len(probabilities), height, width), dtype=bool)
    if size <= 0:
        return on_grid
    for index, plane in enumerate(probabilities):
        square = skimage.transform.resize(
            plane,
            (size, size),
            order=0,
            mode="edge",
            preserve_range=True,
            anti_aliasing=False,
        )
        on_grid[index, top : top + size, left : left + size] = square > threshold
    return on_grid


def simplify_polygon(points, limit):
    """A closed polygon with at most `limit` vertices, same shape.

    Uniform decimation of the vertex LIST, keeping the first point, so the
    outline stays closed and its resolution is the only thing that changes.
    Ceiling division, so the kept count is always AT OR BELOW the limit: a
    fractional step could land one vertex over and put the region past the
    size the writer was budgeted for.

    `points` is the open contour skimage returns, in (row, column) order.
    """
    if len(points) <= limit:
        return points
    step = -(-len(points) // limit)  # ceiling division
    kept = points[::step]
    # The closing vertex duplicates the first for a closed contour; keep the
    # list at or under the limit after that duplication.
    return kept[:limit]


def outline_of(mask, contour_level, max_vertices):
    """The outline of a mask's LARGEST connected component, in [col, row].

    Returns `None` when the mask is empty, which the caller turns into a
    skipped structure rather than an empty region.

    `skimage.measure.find_contours` walks the level set at 0.5 and returns
    (row, column) pairs, which is why the pair is swapped on the way out:
    the ABI's point order is [column, row], the same order SCOORD's Graphic
    Data uses and the same order the rtstruct block uses. Emitting them
    unswapped is the transpose defect the 24.09 run found.
    """
    if not mask.any():
        return None

    labeled = skimage.measure.label(mask, connectivity=2)
    if labeled.max() == 0:
        return None
    # The largest component: a stray speck elsewhere on the frame must not
    # become the region - the outline is meant to be where the structure
    # IS, and the biggest blob is the model's best answer for it.
    sizes = np.bincount(labeled.ravel())
    sizes[0] = 0  # background
    largest = int(sizes.argmax())
    component = labeled == largest

    # A component touching the frame edge has no voxel outside it, so the level
    # set runs off the image and find_contours returns an OPEN trace - the
    # outline "closes" along the border by jumping across the frame. One
    # zero-valued ring around the mask gives every edge-touching component an
    # outside to close against; the ring's offset is subtracted again below.
    padded = np.pad(
        component.astype(np.float32), PAD, mode="constant", constant_values=0.0
    )
    contours = skimage.measure.find_contours(padded, contour_level)
    if not contours:
        return None
    # find_contours returns every closed loop; the longest is the outer
    # boundary of this component.
    longest = max(contours, key=len)
    if len(longest) < 3:
        return None
    simplified = simplify_polygon(longest, max_vertices)
    # (row, column) -> [column, row], rounded to a whole pixel: SCOORD FL
    # values are read back as pixel positions, and a sub-pixel vertex
    # carries no more information than the mask it came from. The swap is
    # the point - emitting (row, column) is the transpose defect. The PAD
    # offset comes off here, and the result is clamped into the frame: the
    # ring exists to close the trace, not to describe space outside it.
    height, width = component.shape
    points = [
        [
            float(min(max(round(point[1]) - PAD, 0), width - 1)),
            float(min(max(round(point[0]) - PAD, 0), height - 1)),
        ]
        for point in simplified
    ]
    # A region is a CLOSED trace, emitted as a POLYLINE: the last vertex
    # must repeat the first so a reader draws the outline shut. skimage
    # returns the closing vertex, but uniform decimation may have dropped
    # it; replacing the last vertex restores closure without exceeding
    # MAX_VERTICES.
    if len(points) >= 2 and points[0] != points[-1]:
        points[-1] = points[0]
    return points


def process_batch(workdir):
    workdir = pathlib.Path(workdir)
    manifest = json.loads((workdir / "manifest.json").read_text())
    # Which block THIS run serves. The demo configuration runs this script
    # under TWO rules sharing one model: one rule names kind=measurements
    # and the other kind=pr. A rule stores EVERY block its script emits, so
    # emitting both blocks would store two SRs and two presentation states.
    # The workdir manifest carries the profile_version the calling rule
    # declared, which is what lets the script emit exactly the block that
    # rule consumes: profile 3 is the demo's `pr` rule, anything else is a
    # measurements rule (including a single-rule site, the historical
    # shape).
    emit_pr = manifest.get("profile_version") == 3
    frames = manifest.get("frames") or []
    if not frames:
        raise SystemExit("the manifest lists no frames")

    _table, targets = read_anatomy()
    model, device = load_model()
    check_against_model(targets, model)
    # A measurements block has no per-frame key, so the FIRST frame is
    # analyzed and the regions land on it. A multi-frame workdir would need
    # one document per frame, which is a different feature.
    frame = frames[0]
    height, width = frame["height"], frame["width"]
    # The requester tunes the probability threshold, the level the outline is
    # traced at, the vertex budget and the 8-bit window; the constants above
    # are the defaults either way.
    threshold = params.number(manifest, "THRESHOLD", THRESHOLD)
    contour_level = params.number(manifest, "CONTOUR_LEVEL", CONTOUR_LEVEL)
    max_vertices = params.count(manifest, "MAX_VERTICES", MAX_VERTICES)
    if max_vertices < 3:
        # A polygon needs three vertices to enclose anything, so a smaller
        # request keeps the default rather than emitting a degenerate region.
        max_vertices = MAX_VERTICES
    hu_low = params.number(manifest, "HU_LOW", HU_LOW)
    hu_high = params.number(manifest, "HU_HIGH", HU_HIGH)
    image = read_frame(workdir, frame, hu_low, hu_high)

    # The chain is split between here and the model: THIS script center
    # crops to a square (fix_resolution raises otherwise), and the model
    # repeats to 3 channels, resizes to 512, maps the window and normalises.
    # Passing a square, single-channel, [-1024,1024] frame is the contract.
    top, left, size = center_crop_box(height, width)
    cropped = image[top : top + size, left : left + size]
    tensor = torch.from_numpy(cropped).float().unsqueeze(0).unsqueeze(0).to(device)

    with torch.inference_mode():
        logits = model(tensor)

    # LOGITS -> probabilities, explicitly: the checkpoint applies no
    # activation, so without this every pixel would clear 0.5.
    probabilities = torch.sigmoid(logits)[0].detach().cpu().numpy()
    if probabilities.shape[0] != len(targets):
        raise SystemExit(
            f"the model produced {probabilities.shape[0]} planes for "
            f"{len(targets)} declared targets; refusing to pair them up"
        )

    masks = masks_on_source_grid(probabilities, height, width, threshold)

    groups = []
    contours = []
    skipped = []
    for position, target in enumerate(targets):
        mask = masks[position]
        coverage = float(mask.sum()) / float(mask.size) if mask.size else 0.0
        points = outline_of(mask, contour_level, max_vertices)
        if points is None:
            # A structure the model did not find at all. Skipping it is the
            # honest answer: an empty outline is not an outline, and a
            # zero-coverage number with nothing to point at would claim a
            # structure that is not there.
            skipped.append(target["label"])
            continue
        measurement = {
            "concept": CONCEPT,
            # The model's own coverage fraction, verbatim. No threshold is
            # written: this is a segmenter, not a classifier, so there is no
            # operating point to report.
            "value": coverage,
            # The same ring, as a SCOORD region in the SR: the SR link
            # "Show image" draws it over the image in Weasis. The region
            # duplicates the PR contour on purpose - the SR carries the
            # measurement and where it came from, the PR carries all
            # outlines at once. Both are the SAME trace of the SAME mask.
            "region": {"type": "POLYLINE", "points": points},
        }
        groups.append(
            {
                "finding": {
                    "code": target["code"],
                    "scheme": target["scheme"],
                    "meaning": target["model_label"],
                },
                "measurements": [measurement],
            }
        )
        # The outline goes to the presentation state, not to the SR: it is
        # geometry to draw over the image, and a contour is the object for
        # that. `frame` is 1-based per DICOM and this script analyzes the
        # first frame only (see the frame note above), so it is always 1.
        contours.append(
            {
                "label": target["model_label"],
                "points": points,
                "frame": frame["index"],
            }
        )

    if skipped:
        print(
            f"skipped {len(skipped)} structure(s) with no pixels on frame "
            f"{frame['index']}: {', '.join(skipped)}",
            file=sys.stderr,
        )
    if not groups:
        raise SystemExit(
            "every structure came back empty, so there is nothing to store. "
            "Check the frame's contrast and orientation before re-running; a "
            "result with no groups is refused by the ABI."
        )

    document = {
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {
            "script": "SR-DX-torchxrayvision-regions.py",
            "script_version": "1.0.0",
            "model": "torchxrayvision chestx_det PSPNet",
            "model_version": _xrv_version(),
        },
    }
    if emit_pr:
        # The presentation state: the outlines, and nothing else. The NUM
        # belongs to the measurements rule, whose run emits it below.
        document["pr"] = {"contours": contours}
    else:
        # The Enhanced SR: the coverage NUMs, each with its SCOORD region
        # so a reader can click from the number to the drawn outline.
        document["measurements"] = {
            "unit": UNIT,
            "groups": groups,
        }
    return document


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
