#!/usr/bin/env python3
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false
"""TotalSegmentator -> result.json with an rtstruct block, one run.

Runs the `craniofacial_structures` task once over the workdir's CT stack and
emits an `rtstruct` block of traced organ contours, in pixel coordinates.

The contours come from ONE inference pass: the multilabel volume
TotalSegmentator returns is already back on the input grid (its own nnunet.py
resamples the prediction back and checks the shape and affine are identical to
the input), so every contour is traced on an array that sits on the frames the
structure set references.

The `seg` block was DROPPED after the 24.09 Weasis rendering finding: the SEG
object was conformant and geometrically correct, yet Weasis v4.7.2 drew it as
broken lines, and an independently built reference object rendered identically
- so the rendering defect is viewer-side and no conformant encoder change can
fix it. The seg machinery stays in clinfer and in `SEG-DX-chest.py` for the DX
chest-seg model; this producer emits the rtstruct block alone.

DISCLAIMER: no clinical claim is made or implied. TotalSegmentator is a
research tool; clinfer is transport, not a diagnostic device. Read the
tool's own documentation and licence before any use beyond a demonstration.

LICENCE / ATTRIBUTION: TotalSegmentator is Apache-2.0 (wasserth/TotalSegmentator,
LICENSE and setup.py). The `craniofacial_structures` task is in the openly
available set - it is NOT one of the commercial models the tool restricts.
WEIGHTS ARE NOT SHIPPED by this repository: the site downloads them, or
copies a pre-populated ~/.totalsegmentator onto an offline machine.

Site setup (see ../abi/README.md), in this order:

    1. A site-managed venv with TotalSegmentator installed into it.
    2. Pre-download the weights ONCE, OUTSIDE a timed run:

           /opt/site/venv/bin/python -m totalsegmentator.bin.totalseg_download_weights

       or one manual run of this script. The first run DOWNLOADS the
       ~2 GB of weights, which alone can exceed a model's timeout and
       would cancel the workitem for a reason that has nothing to do
       with the study.
    3. Only then enable the rule that routes to this producer.

Usage: python RTSTRUCT-CT-totalseg.py <workdir>

See ../abi/README.md for the contract.

WHAT CAN GO WRONG, AND WHAT TO DO ABOUT IT
------------------------------------------

1. NOT CT. TotalSegmentator's models are CT models: their input is
   Hounsfield units. On MR the intensities are meaningless and the masks
   would be confidently wrong, so this script REFUSES a study whose
   modalities do not include CT rather than produce one. The workitem is
   cancelled with that reason in clinfer's log.

2. THE MANIFEST HAS NO GEOMETRY. A nifti affine must state the real voxel
   spacing. When the frames declare neither positions (0020,0032) for at
   least two slices nor Spacing Between Slices (0018,0088), this script
   STOPS instead of assuming 1 mm isotropic: a fabricated spacing would
   compress the anatomy in z, and a wrong result stored is worse than a
   cancelled workitem. Re-fetch what it is missing (the source instances)
   or fix the manifest writer - do NOT edit the manifest of a run in
   progress.

   When BOTH are present the POSITIONS decide, because the declared spacing
   describes the acquisition rather than the stack that was sent: a workdir
   holding every twelfth slice still declares the original step, and that
   value is off by a factor of twelve. The declared spacing is used only
   when no two positions exist.

3. SLICE GAP. A sparse stack (the demo keeps every 12th slice) is fed as
   it is, with its real z spacing. TotalSegmentator handles anisotropy
   itself. No intermediate slice is invented: the masks come back on the
   frames that were sent, and the structure set references only those.

4. SIGNEDNESS. The pixels are read in the signedness the instance declares
   in (0028,0103). CBCT sources are commonly signed 16-bit, and reading
   their bytes as unsigned inverts the contrast - air at -1000 HU becomes
   64536 - which is a volume the model cannot segment. The manifest carries
   that flag per frame; a manifest without it is read unsigned, which is
   what this script did before the key existed.

5. TIMING. The task runs at 0.5 mm and REFUSES `--fast`, so there is no
   faster mode. Budget minutes, not seconds; the demo config uses
   timeout_secs = 1800. See the README for what the weights download
   costs on a cold machine.

6. EMPTY ORGANS. A slice with no `mandible` in it has no mandible
   contour, which is normal. An organ that is empty on EVERY frame is
   dropped from `rois` rather than emitted as an empty ROI, and if all
   the organs are empty the `rtstruct` block is omitted entirely - a
   structure set with no ROIs is not a result and the schema refuses it.

7. RADIUS OF THE ORGANS. Only three classes are declared (see ORGANS),
   although the task predicts seven. Declaring three of seven costs
   nothing at inference (nnU-Net computes every channel in one pass) and
   keeps the segmentation at three segments. The ids come from the run
   report, never from a hard-coded table, so a release that renumbers
   the classes does not silently relabel an organ.
"""

import json
import os
import pathlib
import subprocess
import sys

import params

# --- site-editable constants ---------------------------------------------

# The task this site runs. craniofacial_structures is the jaw-relevant model
# in TotalSegmentator's openly available set (mandible, teeth_lower, skull,
# head, sinus_maxillary, sinus_frontal, teeth_upper); there is no maxilla
# class in it. Switching tasks is a site decision - edit this constant and
# the ORGANS table together.
TASK = "craniofacial_structures"

# Where the device comes from. CPU is the default because the demo stand is
# a CPU box and a silently-chosen CUDA device is how a run fails on a
# machine that has no GPU.
DEVICE_ENV = "TOTALSEG_DEVICE"
DEVICE_DEFAULT = "cpu"

# TotalSegmentator exit codes, from its AGENTS.md: 0 ok, 1 runtime error,
# 2 argument error. The script reports which one it got rather than a bare
# "failed".
EXIT_OK = 0

# The organs this site declares. `class_name` is matched against the run
# report's classes map (index -> name), so an id is never hard-coded here.
# A site with its own vocabulary edits this table in its own copy - that is
# what it is here for.
#
# Since the 24.09 seg drop the producer emits an rtstruct block only, so a
# row carries what an RT ROI needs: the label, the class name, a description
# and the RGB for ROI Display Color (3006,002A). The SEG-only fields - the
# SRT type code and the CIELab colour - are gone with the block that read
# them; the table's shape is the ABI's rtstruct half and nothing else.
ORGANS = [
    {
        "label": "MANDIBLE",
        "class_name": "mandible",
        "description": "mandible, TotalSegmentator craniofacial_structures",
        "rgb": [255, 170, 60],
    },
    {
        "label": "TEETH_LOWER",
        "class_name": "teeth_lower",
        "description": "lower teeth, TotalSegmentator craniofacial_structures",
        "rgb": [120, 255, 170],
    },
    {
        "label": "TEETH_UPPER",
        "class_name": "teeth_upper",
        "description": "upper teeth, TotalSegmentator craniofacial_structures",
        "rgb": [140, 160, 255],
    },
]

# HOW TO ADD AN ORGAN - copy one of these blocks above (uncommented) into
# the ORGANS list. The task predicts seven classes in one pass, so adding
# a row costs nothing at inference:
#
#   {
#       "label": "SKULL",
#       "class_name": "skull",
#       "description": "skull, TotalSegmentator craniofacial_structures",
#       "rgb": [200, 200, 200],
#   },
#   {
#       "label": "HEAD",
#       "class_name": "head",
#       "description": "head, TotalSegmentator craniofacial_structures",
#       "rgb": [90, 220, 120],
#   },
#   {
#       "label": "SINUS_MAXILLARY",
#       "class_name": "sinus_maxillary",
#       "description": "maxillary sinus, TotalSegmentator craniofacial_structures",
#       "rgb": [255, 120, 120],
#   },
#
# There are NO accuracy knobs: the task runs on its trained 0.5 mm weights
# and refuses --fast, so every run is already at the tool's best quality.
# The only tunables are WHICH organs land in the structure set (this table)
# and WHERE the model runs (TOTALSEG_DEVICE=cpu|cuda).

# Polygon discipline, copied from RTSTRUCT-contours.py because it is the
# same problem: drop dust, keep the biggest components, and keep every
# polygon inside the byte ceiling of the DICOM element it becomes.
MIN_AREA_PX = 100
MAX_COMPONENTS = 24

# ContourData is a DS element and a DS element's length field is 16 bits
# (PS3.5 7.1.2 and Table 7.1-2 - the 16-bit length table, which is where DS
# lives), so ONE polygon's encoded text may not exceed 65534 bytes. clinfer
# writes three text values per point (patient x, y, z in millimetres), so
# the ceiling is on BYTES, not on points: on the live CT this was measured
# against a point cost about 17.7 bytes, and 2000 points is a margin under
# 65534 / 17.7. clinfer's encoder REFUSES an over-long contour rather than
# truncating it, so a polygon kept under this budget is what makes the
# whole chain succeed instead of cancelling. Raise it in your own copy
# only with that arithmetic in mind.
MAX_POINTS = 2000

# The engine submodule TotalSegmentator exposes for its CLI.
ENGINE = "totalsegmentator.bin.TotalSegmentator"


def manifest_of(workdir):
    """The workdir's parsed manifest.json."""
    return json.loads((workdir / "manifest.json").read_text())


def read_manifest(workdir, task):
    """The workdir manifest, with the study checked to be one this model can read."""
    manifest = manifest_of(workdir)
    modalities = manifest.get("study", {}).get("modalities") or []
    if "CT" not in modalities:
        raise SystemExit(
            f"this study is not CT (modalities: {', '.join(modalities) or 'none'}).\n"
            f"{task} is a CT model: it reads Hounsfield units, and on another "
            "modality its output would be confidently wrong. Route this study "
            "to a producer for its own modality, or narrow the rule's "
            "`modality` key."
        )
    return manifest


def read_frame(workdir, frame):
    """One frame's stored pixels, row-major, in the instance's OWN signedness.

    (0028,0103) decides: a CBCT source is commonly PixelRepresentation=1, so
    its bytes are two's complement and reading them as unsigned would turn
    air at -1000 HU into 64536. The model would then be looking at an
    inverted-contrast volume, which is why the mask is empty rather than
    merely shifted. `signed` is written by clinfer from the same attribute;
    an absent key means the source declared unsigned (or predates the key),
    which is the unsigned read this script has always done.
    """
    import numpy as np

    raw = (workdir / "frames" / frame["file"]).read_bytes()
    if frame.get("signed"):
        dtype = np.int8 if frame["bits"] == 8 else np.int16
    else:
        dtype = np.uint8 if frame["bits"] == 8 else np.uint16
    values = np.frombuffer(raw, dtype=dtype)
    return values.reshape(frame["height"], frame["width"])


def slice_spacing_of(frames, normal=(0.0, 0.0, 1.0)):
    """The signed step between frame centres along the slice normal.

    POSITIONS WIN OVER THE DECLARED SPACING. (0018,0088) describes the
    acquisition: a stack that keeps every twelfth slice still declares the
    original 0.16 mm step, and trusting it compresses the volume twelvefold
    in z. The positions describe the stack actually sent, so they are the
    honest answer whenever at least two of them exist; the declared spacing
    is the fallback for a manifest that carries no positions at all.

    THE SIGN FOLLOWS THE POSITIONS, NOT THE LIST ORDER. The first-to-last
    displacement is projected onto the slice normal and divided by the number
    of intervals, so a stack acquired head-to-foot and one acquired
    foot-to-head come out with opposite steps. Using the absolute value put
    the frames in list order regardless of where they actually are, which
    mirrors every mask on a reverse-acquired series.

    The magnitude is the MEDIAN of the consecutive steps, not the mean: one
    duplicated or dropped position would drag a mean off, while the median
    still describes what the stack looks like in the middle of itself.
    """
    points = [
        [float(v) for v in frame["position"]]
        for frame in frames
        if frame.get("position")
    ]
    if len(points) >= 2:
        steps = []
        for first, last in zip(points, points[1:]):
            # PS3.3 C.7.6.2.1.1 and C.7.6.2.1.2: Image Position (Patient) is
            # the centre of the first voxel, so the difference between two
            # frames is a vector along the slice normal. Its sign says
            # whether the stack was acquired along the normal or against it;
            # keeping the sign is what lets the affine describe a
            # reverse-acquired series instead of mirroring it.
            displacement = sum(
                (last[axis] - first[axis]) * normal[axis] for axis in range(3)
            )
            steps.append(displacement)
        ordered = sorted(steps)
        signed = ordered[len(ordered) // 2]
        if signed != 0:
            return signed
        # Every position is at the same z, so the positions describe no
        # stack at all. That is not the same as having no positions, so it
        # does not silently fall through to the declared spacing - a
        # declared value beside identical positions is exactly the
        # inconsistency this function exists to catch.
        raise SystemExit(
            "every pair of frame positions is at the same z, so the slice "
            "spacing cannot be derived. Refusing rather than assuming 1 mm."
        )

    declared = frames[0].get("slice_spacing")
    if declared is None:
        raise SystemExit(
            "the manifest carries neither positions (0020,0032) for at least "
            "two frames nor slice_spacing (0018,0088), so the slice spacing is "
            "unknown. Refusing rather than assuming 1 mm: on a sparse stack "
            "that would compress the anatomy in z."
        )
    declared = float(declared)
    if declared <= 0:
        raise SystemExit(
            f"the manifest declares slice_spacing {declared}, which is not a "
            "positive distance. Refusing rather than assuming 1 mm."
        )
    return declared


def volume_and_affine(workdir, manifest):
    """The frame stack as an LAS volume, plus its nifti affine.

    The engine's own DICOM reader (dicom2nifti) reorients its input to
    LAS - x toward Left, y toward Anterior, z toward Superior - and the
    model's training grid is that layout. A volume handed over in any
    other axis layout or orientation draws all-zero masks without an
    error (the 24.09 live-run lesson), so this function reproduces the
    reader's transformation exactly: its `create_affine` (nibabel DICOM
    convention) followed by the LAS reorientation. The affine states
    the REAL spacing: when the manifest carries no geometry to build
    one from, this stops - a fabricated 1 mm z would make the model
    read a volume compressed in z and return masks that are wrong on a
    grid that is native in shape - wrong in a way no later check
    catches. The fourth return value is the inverse orientation
    transform, so `build` can map the masks back onto the frame grid.
    """
    import numpy as np
    import nibabel as nib

    frames = manifest["frames"]
    if not frames:
        raise SystemExit("the manifest lists no frames")

    planes = [read_frame(workdir, frame).astype(np.int16) for frame in frames]
    rows, cols = planes[0].shape
    for frame, plane in zip(frames, planes):
        if plane.shape != (rows, cols):
            raise SystemExit(
                f"frame {frame['index']} is {plane.shape[0]}x{plane.shape[1]}, "
                f"but frame 1 is {rows}x{cols}: one volume cannot hold both. "
                "A workdir should hold one series."
            )
    stack = np.stack(planes)  # (slices, rows, cols)

    spacing = frames[0].get("pixel_spacing")
    if not spacing:
        raise SystemExit(
            "the manifest carries no pixel_spacing (0028,0030), so no honest "
            "affine can be built. Refusing rather than assuming 1 mm."
        )
    spacing_x, spacing_y = float(spacing[1]), float(spacing[0])

    row = [1.0, 0.0, 0.0]
    column = [0.0, 1.0, 0.0]
    orientation = frames[0].get("orientation")
    if orientation and len(orientation) == 6:
        row = [float(value) for value in orientation[:3]]
        column = [float(value) for value in orientation[3:]]
    normal = [
        row[1] * column[2] - row[2] * column[1],
        row[2] * column[0] - row[0] * column[2],
        row[0] * column[1] - row[1] * column[0],
    ]
    slice_spacing = slice_spacing_of(frames, normal)
    step = [value * slice_spacing for value in normal]
    position = frames[0].get("position")
    pos = [float(value) for value in position] if position else [0.0, 0.0, 0.0]
    if slice_spacing < 0:
        # The stack was acquired against the normal, so frame 1 - not frame 0
        # - is the slice at the normal's origin, and the stack runs the other
        # way. PS3.3 C.7.6.2.1.1 puts every frame's own Image Position
        # (Patient) on the object, which is where the origin comes from; the
        # list order carries no geometry at all.
        pos = [value - step[axis] for axis, value in enumerate(pos)]

    # The dicom2nifti create_affine form (nibabel DICOM convention): the
    # x column is -row, the y column -column, the z column step, and the
    # origin -position. `step` carries the SIGNED spacing along the normal
    # and is used as-is; its magnitude in the z column put the origin on the
    # wrong end of the stack and moved every mask one slice off.
    affine = np.array(
        [
            [-row[0] * spacing_x, -column[0] * spacing_y, -step[0], -pos[0]],
            [-row[1] * spacing_x, -column[1] * spacing_y, -step[1], -pos[1]],
            [row[2] * spacing_x, column[2] * spacing_y, step[2], pos[2]],
            [0, 0, 0, 1],
        ],
        dtype=float,
    )

    # The LAS reorientation the engine's reader applies on top of it.
    xyz = np.transpose(stack, (2, 1, 0))  # (cols, rows, slices)
    start_ornt = nib.orientations.io_orientation(affine)
    target_ornt = nib.orientations.axcodes2ornt(("L", "A", "S"))
    to_las = nib.orientations.ornt_transform(start_ornt, target_ornt)
    volume = nib.orientations.apply_orientation(xyz, to_las)
    affine = affine @ nib.orientations.inv_ornt_aff(to_las, xyz.shape)
    to_stack = nib.orientations.ornt_transform(target_ornt, start_ornt)

    return volume, affine, (rows, cols), to_stack


def run_engine(run_dir, nifti, task):
    """One TotalSegmentator pass. Returns its output path and run report.

    `--ml` asks for ONE multilabel file rather than one file per class.
    `--fast` is deliberately absent: this task refuses it (its config
    declares disallow_fast), so passing it would be an argument error rather
    than a faster run.

    The child is given THIS SCRIPT'S OWN DIRECTORY on its import path. `-m`
    puts the child's working directory on `sys.path`, not the directory of
    whoever invoked it, so without this a site that keeps the producer and
    its modules side by side would get "No module named totalsegmentator" and
    a cancelled workitem - and a site whose modules live in its venv would
    never notice, which is the worst kind of difference between two
    installations.
    """
    output = run_dir / "seg.nii.gz"
    report = run_dir / "run_report.json"
    device = os.environ.get(DEVICE_ENV) or DEVICE_DEFAULT
    argv = [
        sys.executable,
        "-m",
        ENGINE,
        "-i",
        str(nifti),
        "-o",
        str(output),
        "-ta",
        task,
        "--ml",
        "-d",
        device,
        "--quiet",
        "--report",
        str(report),
    ]
    child_env = dict(os.environ)
    here = str(pathlib.Path(__file__).resolve().parent)
    existing = child_env.get("PYTHONPATH")
    child_env["PYTHONPATH"] = here if not existing else os.pathsep.join([here, existing])
    completed = subprocess.run(argv, cwd=str(run_dir), env=child_env, check=False)
    if completed.returncode != EXIT_OK:
        raise SystemExit(
            f"TotalSegmentator exited {completed.returncode} "
            f"({'runtime error' if completed.returncode == 1 else 'argument error'}).\n"
            f"Its run report is {report} and its output directory is {run_dir}.\n"
            f"The command was: {' '.join(argv)}"
        )
    if not report.is_file():
        raise SystemExit(
            f"TotalSegmentator wrote no run report at {report}, so the "
            "organ ids cannot be resolved from it. Older releases may not "
            "support --report; upgrade the tool rather than hard-coding ids."
        )
    return output, json.loads(report.read_text())


def resolve_organs(run_report, task):
    """The site's ORGANS table with each entry's class id resolved.

    The ids come from the run report's own `classes` map (index -> name),
    never from a table in this file: a release that renumbers the classes
    would otherwise silently relabel every mask, and a mandible drawn as a
    sinus looks like a result.

    A declared organ that the task does not carry is a HARD STOP rather than
    a skip. Dropping it would produce a segmentation with one segment fewer
    than the site declared, and a reader counting segments against the
    configuration would find a result that looks complete and is not - the
    same failure mode as an invented affine. The remedy is to fix the
    ORGANS/TASK pair, which the message says.
    """
    classes = run_report.get("classes") or {}
    by_name = {}
    for index, name in classes.items():
        by_name[str(name).strip().lower()] = int(index)

    resolved = []
    for organ in ORGANS:
        class_id = by_name.get(organ["class_name"].lower())
        if class_id is None:
            raise SystemExit(
                f"the run report for task '{task}' has no class "
                f"'{organ['class_name']}' (it has: "
                f"{', '.join(sorted(by_name))}).\n"
                f"{organ['label']} is declared in this script's ORGANS table, "
                "so the TASK constant and that table disagree. Fix one of "
                "them: a segmentation missing a declared organ looks complete "
                "and is not."
            )
        resolved.append({**organ, "class_id": class_id})
    return resolved


def contours_of(mask, limits):
    """Outer contours of one organ's mask, decimated, as [column, row] pairs."""
    import cv2

    found, _hierarchy = cv2.findContours(mask, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_NONE)
    if not found:
        return []
    found = sorted(found, key=cv2.contourArea, reverse=True)[: limits["max_components"]]
    polygons = []
    for contour in found:
        if cv2.contourArea(contour) < limits["min_area_px"]:
            continue
        points = contour.reshape(-1, 2).tolist()  # (x=column, y=row) from OpenCV
        if len(points) < 3:
            continue
        if len(points) > limits["max_points"]:
            # Decimation, not a drop: the polygon keeps its shape and its
            # first point, and only its resolution changes. Ceiling division
            # so the kept count is always AT OR BELOW max_points - a float
            # step could land one point over and put the polygon back over
            # the 65534-byte ceiling the encoder enforces.
            step = -(-len(points) // limits["max_points"])  # ceiling division
            points = points[::step]
        # ABI points are [column, row], which is EXACTLY what OpenCV hands
        # over: findContours returns (x, y) and x is the column index, y the
        # row index. So the pair is emitted AS-IS - renaming the elements
        # (for (row, col) in points) and re-emitting them swapped is the bug
        # the 24.09 Weasis run found: every contour landed transposed on the
        # anatomy because x and y had been exchanged. The values stay
        # INTEGERS: clinfer multiplies them by the pixel spacing, which this
        # script never sees, so the precision of the millimetre value is
        # clinfer's to decide.
        polygons.append([[int(point[0]), int(point[1])] for point in points])
    return polygons


def build(workdir):
    """The whole producer: one inference pass, two result blocks."""
    import numpy as np

    workdir = pathlib.Path(workdir)
    # The task names the model to run and the limits are the encoder's
    # polygon budget; the requester tunes both, and each read falls back to
    # the site constant above.
    task = params.text(manifest_of(workdir), "TASK", TASK)
    manifest = read_manifest(workdir, task)
    limits = {
        "min_area_px": params.number(manifest, "MIN_AREA_PX", MIN_AREA_PX),
        "max_components": params.count(manifest, "MAX_COMPONENTS", MAX_COMPONENTS),
        "max_points": params.count(manifest, "MAX_POINTS", MAX_POINTS),
    }
    for key in ("max_components", "max_points"):
        # Both are slice lengths and divisors: zero would drop every contour
        # or divide by zero, so an unusable request keeps the default.
        if limits[key] < 1:
            limits[key] = {
                "max_components": MAX_COMPONENTS,
                "max_points": MAX_POINTS,
            }[key]
    volume, affine, (rows, cols), to_stack = volume_and_affine(workdir, manifest)

    # One subdirectory of the workdir per engine run, so the nifti and the
    # tool's report sit beside the result rather than in the frames'
    # listing - an operator reproducing this by hand needs to see which
    # files were the tool's.
    run_dir = workdir / "totalseg"
    run_dir.mkdir(exist_ok=True)

    import nibabel as nib

    nifti = run_dir / "ct.nii.gz"
    # The engine's own reader saves its nifti in millimetre units and
    # float64; replicating the exact conversion means replicating the
    # dtype and the header units too, not only the array and the affine.
    image = nib.Nifti1Image(volume.astype(np.float64), affine)
    image.header.set_slope_inter(1, 0)
    image.header.set_xyzt_units(2)  # millimetres for xyz
    nib.save(image, str(nifti))

    output, run_report = run_engine(run_dir, nifti, task)
    predicted = np.asanyarray(nib.load(str(output)).dataobj)

    # The tool guarantees the output is back on the input grid
    # (its nnunet.py resamples the prediction back and then checks the
    # shape and affine are identical). A cheap assert keeps a future
    # release honest: a mismatched grid here would silently misalign every
    # contour against the frames the structure set references.
    if predicted.shape != volume.shape:
        raise SystemExit(
            f"TotalSegmentator returned {predicted.shape} for a "
            f"{volume.shape} input ({run_dir / 'run_report.json'} describes "
            "the run). The ABI needs masks on the source grid; a resampling "
            "step here would be this script inventing geometry."
        )

    # The engine answers on the LAS grid. Undo the reorientation and the
    # transpose so predicted[position] is the mask of frame `position`
    # on the (slices, rows, cols) grid the loops below read.
    predicted = np.transpose(
        nib.orientations.apply_orientation(predicted, to_stack), (2, 1, 0)
    )

    organs = resolve_organs(run_report, task)

    # TotalSegmentator's own version, for the producer block: a result that
    # does not say which model produced it cannot be reproduced.
    model_version = str(
        run_report.get("totalsegmentator_version")
        or run_report.get("version")
        or "unknown"
    )

    frames = manifest["frames"]
    contours = {organ["label"]: [] for organ in organs}
    for position, frame in enumerate(frames):
        for organ in organs:
            mask = (predicted[position] == organ["class_id"]).astype(np.uint8)
            for polygon in contours_of(mask, limits):
                contours[organ["label"]].append(
                    {"frame": frame["index"], "points": polygon}
                )

    # The result carries an rtstruct block ONLY. The seg half was dropped
    # after the 24.09 Weasis rendering finding: the SEG object was
    # conformant and geometrically correct (frame-by-frame agreement with an
    # independently built reference mask was 1.000), yet Weasis v4.7.2 drew
    # it as broken lines - and the manual reference object, built by a
    # DIFFERENT builder, rendered identically. That places the rendering
    # defect on the viewer side, so no change to a conformant encoder can
    # fix it; emitting an object the one target viewer cannot read is work
    # spent for nothing. The seg machinery - seg.rs, the Seg schema,
    # store_segmentation and SEG-DX-chest.py - is untouched and stays for the
    # DX chest-seg model.
    result = {
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {
            "script": "RTSTRUCT-CT-totalseg.py",
            "script_version": "1.0.1",
            "model": "TotalSegmentator",
            "model_version": model_version,
        },
    }

    rois = [
        {
            "name": organ["label"],
            "color": organ["rgb"],
            "interpreted_type": "ORGAN",
            "generation_algorithm": "AUTOMATIC",
            "description": organ["description"],
            "contours": contours[organ["label"]],
        }
        for organ in organs
        if contours[organ["label"]]
    ]
    if rois:
        result["rtstruct"] = {
            "label": "TOTALSEG",
            "name": f"TotalSegmentator {task}",
            "rois": rois,
        }
    return result


def main(workdir):
    result = build(workdir)
    workdir = pathlib.Path(workdir)
    tmp = workdir / "result.json.tmp"
    tmp.write_text(json.dumps(result, indent=2))
    tmp.rename(workdir / "result.json")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <workdir>", file=sys.stderr)
        raise SystemExit(2)
    main(sys.argv[1])
