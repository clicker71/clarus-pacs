#!/usr/bin/env python3
"""Mesh builder: the reference STL producer, and the seg/stl bridge.

Runs marching cubes over the stack of threshold masks and writes the
resulting surface as a binary STL. The point is not the mesh - it is the
BRIDGE: this script and `SEG-threshold.py` share one piece of mathematics
(`image > THRESHOLD`), and this script proves it by building the surface of
those same masks in 3-D. One runs the filter per frame and stores masks,
the other runs it per frame and stores the surface that bounds the
thresholded volume.

Run by hand from the work directory, which is also how clinfer runs it:

    python STL-mesh.py /path/to/workdir

See ../abi/README.md for the contract.

Dependencies: numpy and scikit-image only. The STL is written by hand
(~25 lines) rather than through a library, because the binary STL format is
trivial and a dependency for it would be a dependency for nothing.
"""

import json
import pathlib
import struct
import sys

import numpy as np
from skimage import measure

import params

# The binarization threshold, in the frame's own units. Deliberately the
# same default as SEG-threshold.py: the bridge only means something if the
# two scripts threshold identically. The requester may override it, and the
# constant below is the default either way.
THRESHOLD = 128

# Stride: every STEP-th frame and every STEP-th pixel row/column. The
# reference site stack is 432 frames of 512x512; marching cubes over the
# full-resolution volume grinds for minutes, while STEP=2 quarter-frames
# the stack (1/2 along z, 1/4 per plane) and still resolves the organ
# surface.
STEP = 2

# The marching-cubes spacing is the STRIDED grid pitch, so it is derived from
# the stride the run actually used rather than held here: the work directory
# carries no rescale information, and reporting a mesh at a pitch other than
# the one it was built at would be a measurement this script cannot make.

# Marching-cubes level, in the same units as the thresholded volume. The
# volume is 0/1 after thresholding, so 0.5 is the midpoint between the two.
LEVEL = 0.5

# Decimation budget: the clustering grid has this many cells along the
# longest bounding-box axis. The raw marching-cubes surface of a torso is
# millions of triangles and no interactive viewer can draw that; 64 cells
# keeps the result around 100-200k triangles (a few CELLS^2 occupied
# vertices) while the organ silhouette stays intact.
DECIMATE_CELLS = 64


def read_volume(workdir, manifest, threshold, step):
    """The study's frames as one 3-D array of booleans."""
    planes = []
    for frame in manifest["frames"][::step]:
        raw = (workdir / "frames" / frame["file"]).read_bytes()
        dtype = np.uint8 if frame["bits"] == 8 else np.uint16
        image = np.frombuffer(raw, dtype=dtype).reshape(
            frame["height"], frame["width"]
        )
        # The same stride on the in-plane axes keeps the volume isotropic.
        planes.append((image > threshold)[::step, ::step])
    # Stacked in manifest order, which is frame order.
    return np.stack(planes)


def decimate(vertices, faces, cells):
    """Vertex clustering on a uniform grid: one vertex per occupied cell.

    Bounded by construction: with `cells` cells along the longest axis the
    output has at most a few cells^2 vertices (a surface, not a volume).
    The visible topology is preserved on the grid scale; the triangles the
    clustering collapses are sub-cell detail a viewer cannot resolve.
    """
    lo = vertices.min(axis=0)
    span = vertices.max(axis=0) - lo
    cell = span.max() / cells
    if cell == 0.0:
        cell = 1.0
    keys = np.floor((vertices - lo) / cell).astype(np.int64)
    _, inverse = np.unique(keys, axis=0, return_inverse=True)

    counts = np.bincount(inverse)
    sums = np.column_stack(
        [
            np.bincount(inverse, weights=vertices[:, axis])
            for axis in range(3)
        ]
    )
    clustered = sums / counts[:, None]

    remapped = inverse[faces]
    keep = (
        (remapped[:, 0] != remapped[:, 1])
        & (remapped[:, 1] != remapped[:, 2])
        & (remapped[:, 0] != remapped[:, 2])
    )
    return clustered, remapped[keep]


def write_binary_stl(path, vertices, faces):
    """Write a binary STL: 80-byte header, u32 count, 50 bytes per face.

    The 1989 specification, which is what A.85.1 encapsulates. Every field
    is little-endian; the header is free-form and left as zeroes rather
    than a comment, because some readers treat a header beginning with
    "solid" as ASCII STL and mis-parse the rest. Faces are packed through
    numpy rather than a per-face Python loop: the decimated mesh is small,
    but the undecimated path is what the loop made unbearable.
    """
    tris = vertices[faces].astype("<f4")
    u = tris[:, 1] - tris[:, 0]
    v = tris[:, 2] - tris[:, 0]
    normals = np.cross(u, v)
    # The facet normal, computed from the winding so the file is
    # self-consistent: a reader that trusts the normal and one that
    # recomputes it must agree.
    length = np.sqrt((normals * normals).sum(axis=1))
    length[length == 0.0] = 1.0
    normals = normals / length[:, None]

    dtype = np.dtype(
        [
            ("normal", "<f4", (3,)),
            ("v1", "<f4", (3,)),
            ("v2", "<f4", (3,)),
            ("v3", "<f4", (3,)),
            ("attr", "<u2"),
        ]
    )
    records = np.empty(len(faces), dtype=dtype)
    records["normal"] = normals
    records["v1"] = tris[:, 0]
    records["v2"] = tris[:, 1]
    records["v3"] = tris[:, 2]
    # Attribute byte count: unused, and zero is what every writer emits.
    records["attr"] = 0

    buf = bytearray(80)
    buf += struct.pack("<I", len(faces))
    buf += records.tobytes()
    path.write_bytes(bytes(buf))
    return len(faces)


def process_batch(workdir):
    workdir = pathlib.Path(workdir)
    manifest = json.loads((workdir / "manifest.json").read_text())

    threshold = params.number(manifest, "THRESHOLD", THRESHOLD)
    step = params.count(manifest, "STEP", STEP)
    if step < 1:
        # A stride of zero would slice the stack to nothing; one is the
        # identity stride and the only value below the default that still
        # describes a volume.
        step = 1
    level = params.number(manifest, "LEVEL", LEVEL)
    cells = params.count(manifest, "DECIMATE_CELLS", DECIMATE_CELLS)
    if cells < 2:
        # Vertex clustering needs at least two cells along an axis to have a
        # grid at all, so a smaller request falls back to the default rather
        # than collapsing the mesh.
        cells = DECIMATE_CELLS
    # The spacing is the strided grid pitch, so it follows the ACTUAL stride:
    # a requester that asks for STEP=4 measured 4 mm voxels, and keeping the
    # constant would report a mesh at a scale it was not built at.
    spacing = (float(step), float(step), float(step))

    volume = read_volume(workdir, manifest, threshold, step)
    # A volume too thin for marching cubes, or one whose values all sit on one
    # side of the threshold, has no surface to extract: the call below raises a
    # bare traceback that says nothing about which of the two happened. Both are
    # legitimate outcomes of the rule (a mask with no object in it), so they are
    # reported as a diagnosis instead of a crash.
    if volume.size == 0 or min(volume.shape) < 2:
        raise SystemExit(
            f"volume is {volume.shape}, but marching cubes needs at least 2 "
            f"voxels on each axis to form a cell. Nothing to extract at "
            f"threshold {threshold}."
        )
    low, high = float(volume.min()), float(volume.max())
    # The volume is already binarized (0/1) by the threshold rule above, so the
    # level being crossed is LEVEL, not THRESHOLD: a mask with no object in it
    # is all zeros and never reaches it.
    if not (low <= level <= high):
        raise SystemExit(
            f"the volume spans [{low}, {high}] and never crosses the "
            f"marching-cubes level {level}, so there is no surface to "
            f"extract. The threshold ({threshold}) and the shape are the two "
            "things to check."
        )
    # Negative coordinates are normal and expected: the mesh is written in
    # the volume's own index space, and A.85.1.4.1 Note says medical meshes
    # routinely carry them.
    vertices, faces, _, _ = measure.marching_cubes(
        volume.astype(np.float32), level=level, spacing=spacing
    )
    # The raw marching-cubes surface is beyond what a viewer can draw;
    # cluster it to the decimation budget before writing.
    vertices, faces = decimate(vertices, faces, cells)
    triangles = write_binary_stl(workdir / "mesh.stl", vertices, faces)

    return {
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {
            "script": "STL-mesh.py",
            "script_version": "1.0.0",
            "model": "marching-cubes",
            "model_version": f"t{threshold}s{step}c{cells}",
        },
        "stl": {"file": "mesh.stl"},
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
