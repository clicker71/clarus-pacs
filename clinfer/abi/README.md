# The clinfer-abi processor contract

This directory is the contract between clinfer and an **external
processor**: an ordinary program, configured by the site, that clinfer
runs once per study.

clinfer collects the study's frames into a work directory, runs the
program over that directory, and reads the single `result.json` the
program writes back. clinfer then writes the derived DICOM objects
itself. The processor never sees a DICOM object, so it cannot corrupt
one: every attribute of every stored image is written by clinfer.

Everything in this document is the reference. The implementation is in
`src/abi/` (`manifest.rs`, `workdir.rs`, `schema.rs`, `external.rs`) and
in `src/derived.rs`.

---

## 1. Trust

Scripts are **operator-trusted configuration**, in the same trust domain
as the LLM API key. A processor runs as the same user as clinfer, with
the same filesystem access, and is not sandboxed. It is a program you
installed.

That is the whole trust model. Do not configure a processor you would
not run by hand.

---

## 2. How a processor is started

```
<interpreter> <script> <workdir>
```

- `argv[1]` is the absolute path to the work directory.
- the working directory is also the work directory.
- `stdin` is `/dev/null` (or `NUL`).
- the environment is cleared and then given exactly:
  - `PATH`, copied from clinfer's own environment (so the interpreter
    can be found and can find its own tools);
  - `PYTHONUNBUFFERED=1`, so stderr arrives in real time rather than at
    exit, which is what makes a hung processor diagnosable;
  - `PYTHONIOENCODING=utf-8`, so a producer's own output does not depend on
    the console codepage (on a cp1251 host a model's progress bar would
    otherwise kill the child with a `UnicodeEncodeError`). Python reads this
    variable; other interpreters ignore it;
  - on Windows only, the host-identity variables `SystemRoot`, `WINDIR`,
    `USERPROFILE`, `HOMEDRIVE` and `HOMEPATH`. Without `SystemRoot` a child
    cannot initialise Winsock, and `USERPROFILE` is where a site keeps its
    pre-downloaded model weights (`~/.torchxrayvision`). These are the
    machine's own facts rather than deployment state, so they pass through
    like `PATH`.
- the interpreter defaults to `python3` and is configured per model.
  A virtualenv's own python binary is the intended value; shell
  activation is an interactive convenience and has no place here.

Configured as:

```yaml
models:
  denoise:
    script: scripts/DERIVED-clahe.py
    interpreter: venv/bin/python
    timeout_secs: 300          # optional, 5..1800, default 300
```

A RELATIVE `script` is anchored at the directory of the configuration
file itself (e.g. `script: ./denoise.py` next to the config), never at
the process cwd and never at the workdir, which is minted per run.

Declaring `script` is what makes a model external. There is no
`kind: external` value: the `kind` enum describes the in-process engine
and disappears with it.

A processor is an **ordinary program**. You can reproduce any failing
run by hand:

```
cd <workdir>
<configured interpreter> <configured script> <workdir>
```

The work directory is left on disk for exactly this reason, and the
audit line printed at run time tells you which directory it was. By-hand
runnability is a feature, not a side effect.

---

## 3. The work directory

```
<workdir>/
  manifest.json
  frames/
    0000.raw
    0001.raw
    ...
    NNNN.raw
  result.json          <- the processor writes this
```

and anything else the processor wants to leave beside them.

- One raw file per frame, in frame order: series order, then instance
  order within each series.
- `0000.raw` is the first frame the manifest lists.
- A raw file holds the frame's pixel bytes **exactly as stored** in the
  source instance: no header, no padding, no DICOM. A file is always a
  whole number of samples (`bits / 8` bytes per sample).
- Frames are written once and never modified. They are the run's input.

### 3.1 `manifest.json`

```json
{
  "protocol": { "name": "clinfer-abi", "version": 1 },
  "clinfer_version": "0.3.0-alpha",
  "profile_version": 2,
  "parameters": { "CLIP_LIMIT": 2.0 },
  "study": {
    "uid": "1.2.840.113857.1.1",
    "description": "CHEST PA AND LAT",
    "modalities": ["DX"],
    "body_part": "CHEST"
  },
  "frames": [
    {
      "index": 1,
      "file": "0000.raw",
      "sop_uid": "1.2.840.113857.1.1.1",
      "width": 2048,
      "height": 2048,
      "bits": 16,
      "photometric": "MONOCHROME2",
      "bytes": 8388608
    }
  ]
}
```

- `frames[].index` is **1-based** and equals the file name's number plus
  one: `index: 1` is `0000.raw`. The index is the frame's identity in
  this contract.
- `frames[].file` is a name inside `frames/`, never a path.
- `frames[].bytes` is the raw file's size; the sum of the fields is the
  total the audit line reports.
- `study` carries the clinical context the script may want. clinfer
  does not parse the script's output against it.

#### 3.1.1 Optional per-frame geometry

A frame may also carry the Image Plane Module attributes of the instance
it came from. Each is **optional and per frame**, and each is written
**only when the instance declared a value**:

```json
{
  "index": 1, "file": "0000.raw", "sop_uid": "..", "width": 512,
  "height": 512, "bits": 16, "photometric": "MONOCHROME2", "bytes": 524288,
  "pixel_spacing": [0.5, 0.5],
  "slice_spacing": 12.0,
  "position": [-12.4, -102.0, 6.0],
  "orientation": [1.0, 0.0, 0.0, 0.0, 1.0, 0.0]
}
```

| Key | Attribute | Shape |
|---|---|---|
| `pixel_spacing` | (0028,0030) | `[row spacing, column spacing]` |
| `slice_spacing` | (0018,0088) | a number |
| `position` | (0020,0032) | `[x, y, z]`, the first voxel's centre |
| `orientation` | (0020,0037) | six direction cosines, row first |

**The rule for every producer: assume NOTHING about geometry the
manifest does not carry.** An absent key means the instance declared no
such attribute. It does **not** mean 1 mm isotropic, and it does not
mean "the same as the previous frame". A producer that must have a real
spacing - one building a volume, for instance - has to **stop and say
so** rather than substitute a value: a fabricated spacing produces a
result that is wrong in a way no later check catches, and a wrong result
stored is worse than a cancelled workitem.

If you need geometry a frame does not carry, the fix belongs in the
source data or in the configuration. Editing a manifest to add it is
editing the run's own question, and the frames and manifest are written
once and never modified.

The protocol version stays `1` for these keys. The document is
additive: a reader that does not know a key ignores it, which is what
lets an existing producer keep working unchanged.

#### 3.1.2 `parameters`

The requester may attach a parameter list to the workitem - the UPS
Scheduled Processing Parameters Sequence (0074,1210), a list of Content
Items (PS3.3 Table C.30.2-1). clinfer does not interpret them: it
flattens them into an object and hands them to the script.

```json
"parameters": {
  "ALGORITHM": "clahe",
  "CLIP_LIMIT": 2.5,
  "BODY_SITE": "SRT:T-D0050"
}
```

**The key is always present.** It is `{}` when the workitem carried no
parameters, so a script reads `manifest["parameters"]` without a
presence check and gets "the requester stated nothing", not a lookup
error.

The key of each entry is the parameter's **Concept Name Code Value**
(0008,0100). It is a stable machine key; the Concept Name Code Meaning
is display text a site may translate, and is not used as a key.

The JSON type of the value is the parameter's Value Type:

| Value Type (0040,A040) | Rendered as | Source |
|---|---|---|
| `TEXT` | a JSON string | (0040,A160) Text Value, verbatim |
| `NUMERIC` | a bare JSON number | (0040,A30A) Numeric Value, first `\` component |
| `CODE` | the string `"scheme:value"` | (0040,A168) Concept Code Sequence, item's (0008,0102) + (0008,0100) |
| `DATETIME` | a JSON string | (0040,A120) DateTime, verbatim |

A NUMERIC DS is a backslash-separated list (PS3.5 6.2); the parameter's
value is its **first** component. A NUMERIC whose first component is not
a finite number is skipped, never rendered as a zero.

A CODE value is carried in Concept Code Sequence (0040,A168), a single
item holding the Code Sequence Macro - PS3.3 Table 10-2 selects that
sequence for Value Type CODE, and the value is **not** a bare
(0008,0100) on the content item. An item carrying a bare (0008,0100)
instead of the sequence has no CODE value and is skipped.

A DATETIME is carried verbatim, because DT states a precision and may
state a range: a script that wants a moment parses it with its own
rules, and reformatting here would discard what the requester said.

**These Value Types are NOT supported and are skipped with a warning on
stderr: DATE (0040,A121), TIME (0040,A122), UIDREF (0040,A124) and
PNAME (0040,A123).** The omission is documented rather than silent: a
parameter clinfer does not understand is not guessed at, and the script
sees the parameters that were readable and nothing invented. An item
with no Concept Name Code Value is skipped the same way. Every skip is
reported, so a requester can see which of its parameters did not arrive
by reading the run's stderr.

The warning names the parameter's **name only**, never its value: a
parameter list is requester-supplied text and may carry anything the
site typed into it.

`profile_version` and `protocol.version` are unaffected. The key is
additive - a producer written before it existed ignores it, and a
producer that reads it works against every manifest clinfer writes.

##### Reading the parameters

Every script in this directory shares one reader, `params.py`. Import it
beside `json` and read each tunable through it:

```python
import params

...
manifest = json.loads((workdir / "manifest.json").read_text())
clip_limit = params.number(manifest, "CLIP_LIMIT", 2.0)
tile_grid = params.grid(manifest, "TILE_GRID", (8, 8))
```

| Helper | Returns | Used for |
|---|---|---|
| `params.number(manifest, key, default)` | `float` | a NUMERIC tunable |
| `params.count(manifest, key, default)` | `int` | a NUMERIC count or slice length |
| `params.grid(manifest, key, default)` | `(n, n)` | a square OpenCV grid size |
| `params.text(manifest, key, default)` | `str` | a TEXT tunable |
| `params.raw(manifest, key, default)` | as stored | anything else |
| `params.code(manifest, key, default)` | `str` | the value part of a CODE |

**The fallback rule: a missing key, a value of the wrong JSON type, or an
unparseable value returns the default and never raises.** A manifest
written before this key existed and one carrying `{}` behave identically.
That is deliberate - the scripts are the demo's reference producers, and
one that refuses to run because a hand-written workitem carried a stray
parameter would be less useful than one that runs with its documented
default. The default IS the documented behaviour, not an error path.

The default values in each script are unchanged, so a run whose
requester sends nothing produces byte-identical output to before.

---

## 4. `result.json`

The processor writes exactly one `result.json` into the work directory.
The schema is **strict**: an unknown key is an error, a missing required
key is an error, and a version mismatch is an error naming both versions.
A result clinfer cannot fully understand is not partially applied.

### 4.1 `protocol` (required)

```json
"protocol": { "name": "clinfer-abi", "version": 1 }
```

Both fields are required. Both are checked.

### 4.2 `producer` (required)

```json
"producer": {
  "script": "DERIVED-clahe.py",
  "script_version": "1.4.0",
  "model": "clahe-clip2",
  "model_version": "2024-06-01",
  "seed": "0"
}
```

`script` is required. The rest are optional strings, and they are
**the script's own declaration**: clinfer cannot know which model was
loaded, which revision of the script ran, or what seed it used. Stating
it is the only honest provenance chain. Each side stamps what it owns:
the script stamps this, and clinfer stamps its own version, the profile
version and the study UID, in the manifest.

### 4.3 `pr` (optional): boxes

```json
"pr": {
  "boxes": [
    { "label": "nodule", "confidence": 0.81,
      "x1": 100, "y1": 200, "x2": 140, "y2": 260, "frame": 1 }
  ]
}
```

Coordinates are in the source frame's pixel space.

`frame` names WHICH frame the box was measured on: **1-based**, matching
`frames[].index` in the manifest. It is optional and defaults to `1`,
because the common processor is a per-frame detector handed one frame. A
value that is not a positive whole number is an error, not a silent `1`:
zero or `1.5` is a processor that lost count, and attaching the box to the
wrong image is worse than refusing it.

clinfer stores **one presentation state per frame that has at least one
box**. A frame with no boxes gets no object - an overlay with no graphics
is not a result. The overlay is bound to that frame's own SOP instance at
both levels (the series reference and the annotation reference), and the
displayed area is that frame's own geometry, so the overlay can never crop
the image it is attached to.

A box naming a frame the study does not have cancels the cycle **before
anything is stored**: half a study's overlays is worse than none, because
it looks complete. An empty `boxes` list is legal and means the same as
omitting the block.

### 4.4 `sr` (optional): text

```json
"sr": { "text": "No acute cardiopulmonary process." }
```

Free text for a report. clinfer stores it **verbatim** in one Basic Text SR
per study: it adds no wording of its own, and the evidence names every
frame that was analyzed. Blank or whitespace-only text produces no object -
a document that says nothing is not a report.

### 4.5 `derived` (optional): replacement frames

```json
"derived": {
  "files": [
    { "file": "0000.raw" },
    { "file": "0001.raw", "width": 1024, "height": 1024 }
  ]
}
```

- The list is **positionally aligned with the manifest**: entry `i`
  replaces frame `i`. The count must equal the frame count, or the cycle
  is cancelled.
- Each entry is a **name inside the work directory**, never a path.
  Names containing `/`, `\` or `..` are refused: the work directory is
  the unit of work, and a result reaching outside it is one clinfer
  cannot bound or clean up.
- `width`, `height` and `bits` are **the geometry the processor declares
  for its OUTPUT**, and they may differ from the source: upscale,
  downscale and crop are all legal. DICOM allows any size, and the check
  that matters is not equality with the source but that the file is the
  shape it claims. A value that is not a positive whole number is an
  error, not a fallback.
- An **omitted** field means the source geometry **for that axis**, which
  is why a filter that preserves geometry (`DERIVED-clahe.py`) declares nothing.
- Validation is **byte consistency** against the declaration:
  `width x height x samples x ceil(bits/8)` must equal the actual file
  size. A mismatch is a CANCELED workitem, never a stored object. The
  comparison is against the DECLARED geometry - a derived image of a
  different shape is a resized image, and only a mis-sized file is an
  error. Nothing is ever padded, because padding would invent pixels that
  were never processed.
- **Samples per pixel, the photometric interpretation and the bit
  semantics (signedness) always come from the source**: a filter changes
  pixels, not the colour model.
- The file is read **after** the process has exited, so its size is
  whatever the script left behind.

### 4.6 `seg` (optional): per-frame binary masks

```json
"seg": {
  "segments": [
    { "label": "THRESHOLD",
      "category": { "code": "M-01000", "scheme": "SRT",
                    "meaning": "Morphologically Altered Structure" },
      "type":     { "code": "M-01000", "scheme": "SRT",
                    "meaning": "Morphologically Altered Structure" },
      "color": [255, 0, 0],
      "algorithm": "AUTOMATIC" }
  ],
  "files": [
    { "file": "mask-0000.raw", "frame": 1 },
    { "file": "mask-0001.raw", "frame": 2 }
  ]
}
```

Stores one BINARY Segmentation object per study (PS3.3 A.51).

- `segments` is **required and non-empty**; entry `i` is Segment Number
  `i + 1`, and that number is also the bit plane the mask bit belongs to.
- `category` and `type` are **required on every segment**, each with all
  three of `code`, `scheme` and `meaning`. clinfer copies them into the
  Segmented Property Category and Type Code Sequences. It never invents a
  category: the clinical vocabulary is the processor's to state, and a
  default would be clinfer making a clinical claim it cannot support.
- `color` is the Recommended Display CIELab Value (0062,000D): three
  whole numbers. It is a recommendation a reader may ignore, which is why
  it is optional.
- `algorithm` is the Segment Algorithm Type (0062,0008), default
  `"AUTOMATIC"`. A blank value is refused rather than defaulted - the key
  is present, so the processor meant something specific.
- Each `files` entry is a **name inside the work directory** (same rule as
  `derived`), plus an optional 1-based `frame` defaulting to `1`.
- `files` must have **one entry per source frame**, and the declared
  frames must be exactly the run **`1..=N`** with no gap and no repeat: a
  gap is an image with no mask and a repeat is one mask claimed twice.
  The list may name them in any order - the frame number, not the array
  position, is what associates a mask with an image.

**Mask layout.** A mask file is bit-packed, one bit per pixel, with each
row padded to a byte boundary - exactly what `numpy.packbits(mask, axis=1)`
produces for a 2-D array. A **frame** carries one plane per declared
segment, one after the other, so its size is `segments * rows * ceil(cols/8)`
bytes; the object's Pixel Data is the frames concatenated in frame order.
With one segment this is the `ceil(W*H/8)` shape the threshold filter
produces.

A file whose size is not exactly that is a **named cancel**: a mask that
is not the shape of the frame it describes cannot be laid over it. The
masks are copied into Pixel Data **verbatim** - there is no unpack/repack
step, so the bytes clinfer stores are the bytes the processor wrote.

### 4.7 `stl` (optional): one surface mesh

```json
"stl": { "file": "mesh.stl" }
```

Stores one Encapsulated STL object per study (PS3.3 A.85.1).

- `file` is **required** and is a **name inside the work directory** (same
  rule as `derived` and `seg`). There is exactly ONE file per result: A.85.1
  stores a single encapsulated document, so a list here would promise
  something the object cannot express.
- The file must be a **binary STL**: an 80-byte header, a little-endian
  `u32` triangle count at offset 80, then 50 bytes per triangle. clinfer
  validates `size == 84 + 50 * count` against the file's own declared
  count. A mismatch is a **named cancel**, never a stored object: a file
  whose length disagrees with its header is a truncated or concatenated
  mesh, which no viewer can load.
- **Negative vertex coordinates are normal** and are not checked. A.85.1.4.1
  Note records that medical meshes routinely carry them, so a sign check
  would refuse correct data.
- The bytes are copied into Encapsulated Document (0042,0011) **verbatim**.
  There is no re-encode step: the mesh a viewer loads is the mesh the
  processor wrote.
- clinfer writes MIME Type (0042,0012) = `"model/stl"` and Modality
  (0008,0060) = `"M3D"` (the enumerated value; A.85.1.4.3 has no `"STL"`).
  Frame of Reference (0020,0052) is Type 1 in A.85.1: it is the source
  images' value when they declare one, and a UID derived from the study,
  the model and the profile when they do not. A mesh has a coordinate
  system whether or not its source named one, and an own UID names that
  honestly rather than omitting a required attribute.

All five blocks may appear together in one result, and each is stored
independently: a result with boxes and text stores both.

---

## 5. The work directory's lifetime

| Outcome | What happens to the work directory |
|---|---|
| success (exit 0, valid result, geometry, counts) | **removed** after the objects are stored |
| non-zero exit, timeout, kill, schema error, geometry mismatch | **kept**, for forensics |
| startup sweep | any directory older than **24 hours** is removed |

The reason is asymmetric on purpose. On success the directory has served
its purpose, and leaving one per study fills the disk with frames that are
still in the PACS. On FAILURE it is the artefact an operator needs: the
frames, the manifest and whatever `result.json` was written are exactly
what makes a bad run reproducible.

An empty but valid result keeps its directory too - "found nothing" is the
case most often worth re-running by hand.

### 5.1 By-hand runs are still possible

Removal on success does not break the by-hand workflow. The audit line
names the directory, and the frames it held are the study's own pixels,
which the origin still has; a run can be reproduced by pointing the script
at a fresh directory built from the same study. What is NOT possible is
re-running against a directory that a successful run already removed -
which is the intended trade, since that run's result is already stored.

### 5.2 The sweep

The 24-hour threshold is a **constant, not a configuration key**: neither
an abandoned run nor a forensic artefact wants a per-site number.

The age comes from **`manifest.json`'s own timestamp**, not the
directory's. The manifest is written once, when the run starts, so its
mtime is the run's age; a directory's mtime moves whenever anything is
written into it, so a processor touching its scratch files could keep an
abandoned run alive indefinitely.

A directory with **no readable manifest** is kept rather than guessed at,
as is one whose timestamp is in the future. Deleting on the strength of a
failed `stat` is how a sweep removes the directory an operator is looking
at.

Only directories are considered, and the sweep runs under
`<cache_dir>/abi/`, so it never touches the `<cache_dir>/pr-*.done`
markers that live beside it.

---

## 6. Atomic write of `result.json`

Write to a temporary name in the work directory and `rename()` it into
place. A rename within one filesystem is atomic, so a reader either sees
no `result.json` or sees a complete one - never a half-written prefix.

clinfer also honours exit status over the file (see below), so a script
that fails partway should simply exit non-zero rather than write a
partial result.

---

## 7. Exit codes and the success rule

**Success is: exit 0 AND a schema-valid `result.json` AND geometry that
matches the input AND one derived image per declared frame.** Never the
exit code alone.

| Exit | Meaning | clinfer's action |
|---|---|---|
| `0`, valid `result.json` | success | stores the derived images |
| non-zero | the script declares failure | cancel the cycle |
| timeout | killed after `timeout_secs` | cancel the cycle |
| `0`, no `result.json` | the script said nothing | cancel the cycle |

- A non-zero exit is a failure **even when a valid `result.json` is on
  disk**: the script declared failure, and honouring its output anyway
  would persist a result its author said was wrong.
- A timeout or kill is a failure **even when `result.json` appeared**:
  the file may be half written.
- A valid result with no `pr`, no `sr` and no `derived` is not a
  failure. It is a processor that found nothing, which is a legitimate
  answer, and clinfer cancels the work item with a reason rather than
  completing it with an output that cannot be found.

A processor that fails does not leave a half-delivered study behind:
nothing is stored for a cycle that did not succeed.

Reason texts are fixed clinical-safe phrases, never the script's stderr:

| Cause | Reason For Cancellation |
|---|---|
| non-zero exit | `external processor failed (exit N)` |
| timeout / kill | `external processor timed out` |
| cannot start | `external processor could not be started` |
| no result | `external processor produced no result` |

---

## 8. Timeout and process-tree kill

The budget is one wall clock per run, end to end, from `timeout_secs`.

On timeout the **whole process tree** is killed, then reaped:

- Unix: the child is made a process-group leader at spawn, and the group
  is signalled. This is why a Python script using `multiprocessing`, a
  dataloader or a worker pool does not leave orphans holding the GPU.
- Windows: `taskkill /T /F /PID`.

A kill is also applied if clinfer itself is shutting down, so a stopped
sidecar does not leave a processor running.

There is no retry. A retry would double the latency a booth cannot
afford, and a script that failed deterministically will fail again.

---

## 9. stderr

Every line of the child's stderr is forwarded to clinfer's log with the
prefix:

```
[external:<model id>] <the line, verbatim>
```

The prefix is what tells an operator which processor a line came from
when several are configured; the script's own words are kept after it.

Limits, so a chatty script cannot flood the journal:

- **2048 bytes per line.** A line longer than that is truncated. A
  progress bar written with carriage returns is a single enormous line,
  and this is what keeps it from being one.
- **65536 bytes per run.** Once the budget is spent, further lines are
  counted, not kept, and a note is logged saying how many were dropped.

stdout is not forwarded: `result.json` is the output channel. Anything
you want an operator to see goes to stderr.

---

## 10. Size limits

| What | Limit |
|---|---|
| `result.json` | `max_result_bytes` per model, default 64 MiB (`DEFAULT_MAX_RESULT_BYTES`) |
| `manifest.json` | 4 MiB (`MAX_MANIFEST_BYTES`) |
| one stderr line | 2 KiB (`MAX_STDERR_LINE_BYTES`) |
| stderr per run | 64 KiB (`MAX_STDERR_BYTES`) |
| `timeout_secs` | 5 to 1800 seconds |

The `result.json` limit is checked **before** parsing, so an oversized
file is refused rather than read into memory. The default is large because
a `seg` or `rtstruct` block carries per-frame geometry, and a deployment
with a tighter budget sets `max_result_bytes` in the model's own block.

---

## 11. The processor is the pipeline

Composition happens **inside the script**. A processor that denoises and
then detects is one script that does both; clinfer runs one program per
model and does not chain them for you. If you want the intermediate
frames, write them into the work directory - it is yours for the run.

This is deliberate. A pipeline expressed in Python is a pipeline you can
debug, version and test with ordinary tools. A pipeline expressed in
configuration would need its own semantics, its own error model and its
own documentation, and would buy nothing the script cannot already do.

Resident mode (a later step) adds a control channel over stdio and keeps
the data form unchanged: the frames stay in the work directory.

---

## 12. Writing a processor

```python
#!/usr/bin/env python3
import json, pathlib, sys

import params

def process_batch(workdir):
    workdir = pathlib.Path(workdir)
    manifest = json.loads((workdir / "manifest.json").read_text())
    # A tunable the requester may set. `params` returns the default for a
    # missing key or an unusable value and never raises, so this line is
    # safe against a manifest that predates the key.
    level = params.number(manifest, "MY_LEVEL", 0.5)
    files = []
    for frame in manifest["frames"]:
        raw = (workdir / "frames" / frame["file"]).read_bytes()
        out = transform(raw, frame, level)
        (workdir / frame["file"]).write_bytes(out)
        # `files` holds OBJECTS, one per derived frame - the same shape
        # `parse_derived_file` reads. A bare file name is refused.
        files.append({"file": frame["file"]})
    return {
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {"script": "my_processor.py", "script_version": "1.0.0"},
        "derived": {"files": files},
    }

if __name__ == "__main__":
    workdir = pathlib.Path(sys.argv[1])
    result = process_batch(workdir)
    tmp = workdir / "result.json.tmp"
    tmp.write_text(json.dumps(result))
    tmp.rename(workdir / "result.json")
```

The `def process_batch(workdir)` shape is the contract's own: a future
shim imports the function directly, and resident mode then arrives
without the script changing.

### 12.1 Included examples

| File | What it shows |
|---|---|
| `DERIVED-clahe.py` | an OpenCV filter, one derived frame per input frame |
| `DERIVED-upscale.py` | a DECLARED geometry: the first consumer of the relaxed derived rule |
| `PR-openvino-detect.py` | OpenVINO detection returning `pr.boxes` |
| `SR-llm.py` | calling an OpenAI-compatible endpoint and returning `sr.text` |
| `SEG-threshold.py` | the reference `seg` producer: a threshold filter, no model |
| `STL-mesh.py` | the reference `stl` producer: the same threshold in 3-D, meshed |
| `SEG-DX-chest.py` | a real model reaching the same `seg` key (DEMO-ONLY, see below) |
| `SR-opacity.py` | pure arithmetic, reported as `sr.text` measurements |
| `RTSTRUCT-CT-totalseg.py` | a real CT model emitting `seg` AND `rtstruct` from ONE run |
| `params.py` | the shared reader every example uses for `manifest["parameters"]` |
| `manifest.example.json` | a manifest as clinfer writes it |
| `requirements.txt` | one virtualenv for the demos |

`SR-llm.py` contacts its endpoint **itself**. clinfer never talks to a
model on the external path: the endpoint, the key and the HTTP client
are the script's own business, configured where the script lives. (The
native remote path is separate and unrelated.)
The **prompt** may come from a file: set `SR_LLM_PROMPT` to its path and
that file is read instead of the embedded template. A path that is set
but unreadable - or empty - is a **failure**, never a silent fallback:
using the template when the operator pointed at their own wording would
run the wrong prompt and report success.

The five demos are deliberately at the same level, one per result kind
and two for `seg`: a filter, a super-resolution model, a detector, an LLM,
a threshold masker, a clinical segmentation model and a pure arithmetic
text producer. That lineup is the point - the runtime carries them all
without knowing which is which.

`SEG-DX-chest.py` is **DEMO-ONLY**. Its model card states no license and its
training masks come from CheXmask (PhysioNet, non-commercial), so the
weights likely inherit non-commercial restrictions. **The weights are NOT
shipped here and are not redistributed**: the site downloads the model
into its own directory as an operator-trusted artifact, and the script
exits with an instruction when `CHEST_SEG_MODEL` (a model DIRECTORY) is
unset. The model ships its own `modeling.py`, loaded with
`trust_remote_code=True` - that file is a program and is executed at
load time, so point the script only at a directory you downloaded
yourself from the model's own repository. Runtime deps live in the
site's own venv: torch (CPU) + transformers (<5: the model's custom
class predates 5.x) + timm + albumentations (see requirements.txt).
The model card's own disclaimer is reproduced in the script's header.

### 12.2 `RTSTRUCT-CT-totalseg.py`: one block from one run

`RTSTRUCT-CT-totalseg.py` runs [TotalSegmentator](https://github.com/wasserth/TotalSegmentator)
once over a CT stack and emits an **`rtstruct`** block of traced organ
contours, in pixel coordinates. It does NOT emit a `seg` block: that half was
dropped in the 24.09 change (the script's own header records it), so a rule
pointed at this producer with `kind: seg` gets nothing.

Licence and attribution: TotalSegmentator is **Apache-2.0**, and the
`craniofacial_structures` task used here is in its **openly available** set
- it is not one of the commercial models the tool restricts. **The weights
are NOT shipped here and are not redistributed**: they are a site artifact,
downloaded by the site (or copied onto an offline machine).

**Pre-download the weights before the rule goes live.** The first run
fetches roughly 2 GB into `~/.totalsegmentator`; on a cold machine that
alone can exceed the model's `timeout_secs` and cancel a workitem for a
reason that has nothing to do with the study. Do it once, outside a timed
run:

```
/opt/site/venv/bin/python -m totalsegmentator.bin.totalseg_download_weights
```

then keep a copy of `~/.totalsegmentator` for your offline machines.

Site configuration:

```yaml
models:
  totalseg:
    script: abi/RTSTRUCT-CT-totalseg.py
    interpreter: venv/bin/python
    timeout_secs: 1800        # the task runs at 0.5 mm and refuses --fast
rules:
  - match:
      modality: [CT]
      code: [CT-ORGAN-SEG]
    model: totalseg
    result:
      kind: rtstruct
```

What the script decides, and what it refuses:

| Situation | Behaviour |
|---|---|
| Study is not CT | **STOP**: the model reads Hounsfield units, so its output on another modality would be confidently wrong |
| Manifest has no `slice_spacing` and no `position` | **STOP**: a fabricated z spacing would distort the volume; it never assumes 1 mm |
| `slice_spacing` absent but positions present | z comes from the **median** step between consecutive positions (a median, not a mean, so one duplicated or missing position cannot drag it) |
| A declared organ is missing from the task's class list | **STOP**: a segmentation with one segment fewer than the site declared looks complete and is not |
| An organ is empty on every frame | The organ keeps its **segment** and is dropped from **`rois`**; empty is normal for a slice, and only a reason to omit a ROI |
| All organs empty | No `rtstruct` block at all - a structure set with no ROIs is not a result |
| Engine exits nonzero | STOP, naming the run report and the exact command |

Three site-editable constants sit at the top of the file: `TASK`, the
`ORGANS` table (labels, SRT codes, and **two colour spaces** - CIELab for
the `seg` block and plain RGB for the ROI Display Color of the `rtstruct`
block) and `DEVICE_ENV` (`TOTALSEG_DEVICE`, default `cpu`).

Organ ids are read from the run report's own `classes` map and **never**
hard-coded, so a release that renumbers the classes cannot silently
relabel an organ. Every contour is decimated under the 65534-byte
`ContourData` ceiling of PS3.5 Table 7.1-2 - the 16-bit Value Length
table, which is where DS lives - because clinfer's encoder **refuses** an
over-long contour rather than truncating it.

Deferred, deliberately: an `sr` block of volumes
(`totalseg --statistics`), the per-tooth FDI `teeth` task, the tool's own
`dicom_seg`/`dicom_rtstruct` output types (the ABI produces the objects),
and any reconstruction of missing slices - a sparse stack is fed as it is,
at its real z spacing, and the tool handles the anisotropy itself.

---

## 13. ABI and Trust Model

### 13.1 The ABI is PROCESS-LEVEL

clinfer spawns the processor as a **separate OS subprocess**. In-process
Python is not supported, for three reasons that each stand alone: the GIL
would serialise inference against clinfer's own work, a crash would take
the sidecar with it, and there would be no boundary at all between the
processor's memory and clinfer's.

Parallelism is **by processes, not threads**: one child per study. There
is no pool size to tune, and adding one would only move the same work
onto a different scheduler.

### 13.2 clinfer stays lightweight

Python, the virtualenv and the model dependencies are **site-managed
artifacts**. Which Python, which packages, which weights and where they
live is the processor's own business: clinfer knows the interpreter path
and the script path from the configuration and nothing else. A site that
wants a different interpreter for a different model configures it per
model.

### 13.3 What clinfer GUARANTEES

- **Crash isolation.** A processor that crashes, hangs or leaks cannot
  crash the host: it runs out of process, under a watchdog, and a timeout
  ends its whole process tree. The worst case is one cancelled workitem.
- **Environment hygiene.** The child's environment is cleared and then
  given exactly the variables it needs (see section 2), so a stray
  variable in clinfer's own environment cannot change what the processor
  does.
- **Bounded resources.** A result over the limit is refused before it is
  read; a run over its timeout is killed; stderr is capped per line and in
  total.

### 13.4 What clinfer does NOT guarantee

- **It is NOT a sandbox.** The child runs with the **same OS user's
  privileges as clinfer** and can read and write anything that user can,
  including clinfer's own configuration and the LLM key if they are
  readable by that user.
- **It does NOT limit privileges.** There is no seccomp filter, no cgroup
  quota, no filesystem namespace and no capability drop. `env_clear` is
  **hygiene, not a sandbox**: it removes variables, not permissions.

A harder boundary is **site policy**, not a clinfer knob. The usual
shapes, as one-line examples rather than a recipe: run clinfer under a
**dedicated OS user** that owns only the cache directory and its own
configuration; put that user in a **cgroup** with a memory and CPU cap;
add a **seccomp** profile when the interpreter's syscall set is known.
All three are configured outside clinfer, and none of them require
changing the contract above.

### 13.5 The contract in one line

**Processors are equal citizens, and the runtime's job is to isolate
their failures, not to sandbox their privileges.**

| | Guaranteed | Not guaranteed |
|---|---|---|
| Failure isolation | crash isolation, watchdog, process-tree kill, reap | - |
| Environment | cleared, then exactly what the contract lists | - |
| Privileges | - | sandbox, privilege limits, filesystem confinement |
| Resources | result size, run timeout, stderr caps | memory, CPU, disk quota |

---

## 14. Version history

- **version 1** - the contract described here. `pr.boxes[].frame`, the
  work-directory lifetime rules, the `seg` block, the `stl` block and the
  relaxed `derived` geometry (objects with an optional declared `width`,
  `height` and `bits`, validated by byte consistency rather than equality
  with the source) were added within version 1. All are additive: a missing `frame`
  means frame 1, and an omitted geometry field means the source's, so
  neither changes the meaning of a result that already conformed. A
  `derived.files` entry that is a bare string is **no longer accepted** -
  the entry is now an object. That is the one breaking change, and it is
  deliberate: the object form is what carries the declaration.
- **version 1, optional per-frame geometry** (section 3.1.1) - the four
  Image Plane Module keys `pixel_spacing`, `slice_spacing`, `position` and
  `orientation`. Additive, so the version is unchanged: a reader that does
  not know the keys ignores them, and a frame whose instance declared no
  such attribute carries no key at all rather than a default. Absent is a
  fact about the source, and the contract's rule for a producer that needs
  the geometry anyway is to stop rather than to assume.
