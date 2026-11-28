# AISPro four-leg loop demo (Step 2)

One recorded end-to-end run of the clinfer loop on a single real study:

1. **Declaration** - `GET /processors/Questionnaire` serves a FHIR R4
   `Questionnaire` per enabled model (`clinfer/src/receiver.rs`, serializer
   `clinfer/src/models/fhir.rs`).
2. **Values over the standard wire** - the UPS workitem carries Scheduled
   Processing Parameters (0074,1210) as PS3.3 Table 10-2 Content Items;
   clinfer reads them (`clinfer/src/dicomweb.rs`
   `scheduled_processing_parameters`) and copies them into the ABI manifest
   (`clinfer/src/worker.rs` `manifest_parameters`).
3. **Script runs** - file ABI `<interpreter> <script> <workdir>`, `argv[1]`
   is the work directory, the script writes `result.json`
   (`clinfer/abi/README.md`).
4. **Result lands in the PACS** - clinfer encodes the derived object and
   STOWs it back to Clarus (`clinfer/src/worker.rs` `run_external`,
   encoder `clinfer/src/derived.rs`).

The demo model is CLAHE (`clinfer/models/clahe.yaml`): parameters
`CLIP_LIMIT` (decimal, default 2.0) and `TILE_GRID` (integer, default 8),
output kind `derived` (Secondary Capture), series description "CLAHE".

## How the workitem and its (0074,1210) arise

`[ups] task_producer = true` is a server MODE, not the default: on a durable
STOW the origin mints ONE Scheduled Workitem Code Sequence (0040,4018)
workitem for the study. This is the data-centre mode - every arriving study
is offered an AI task. The default (`task_producer = false`, as in the
shipped `clarus.conf`) mints nothing; a Task Requester then creates workitems
itself.

The mint names the TASK, not the tuning: it carries the procedure code
(`IMG-CLAHE`, from `[ups.workitem_code]`) and the input study, and writes no
Scheduled Processing Parameters
(`clarus/src/handlers/cad_workitem.rs` `build_record_for_entry` ->
`scheduled_processing_parameters: None`). The parameter values are the Task
Requester's input - the side that knows the concrete intent - and they enter
only through a Task Requester, by UPS Create (PS3.18 11.4) or UPS Update
(11.5): `clarus/src/handlers/ups_pure.rs`. A workitem that carries none runs
with the model's documented defaults (`clinfer/abi/params.py` falls back and
never raises).

The driver therefore exercises both paths: Clarus mints the workitem on STOW
(the F1 path), and the driver then attaches the requester's
`CLIP_LIMIT`/`TILE_GRID` with one standard UPS Update before the 10 s
readiness debounce promotes the workitem to READY - so leg 2 shows a value
that really travelled over the standard wire.

## What to expect

- LEG 1 - the CLAHE `Questionnaire` (`CLIP_LIMIT`/`TILE_GRID` items):

      GET /processors/Questionnaire -> 200
      {"resourceType":"Bundle","type":"searchset","entry":[{...,
        "item":[{"linkId":"CLIP_LIMIT","type":"decimal","text":"Clip limit","required":true,
                 "initial":[{"valueDecimal":2}]},
                {"linkId":"TILE_GRID","type":"integer","text":"Tile grid","required":true,
                 "initial":[{"valueInteger":8}]}]}]}

- LEG 2 - the STOW, the minted workitem, the Update, the (0074,1210)
  round-trip:

      STOW 1 instance(s) -> 200
      minted workitem UID: 2.25.888747951387255631424888751912669980
      minted code: IMG-CLAHE   minted state: SCHEDULED   readiness: INCOMPLETE
      UPS Update 2.25.888747951387255631424888751912669980 -> 200

- LEG 3 - the state transitions, then the sidecar's `external:` audit lines:

      [11:23:23] workitem state: SCHEDULED
      [11:23:33] workitem state: IN PROGRESS
      [11:23:35] workitem state: COMPLETED
      external: ... model=clahe frames=1 bytes=8388608 ... workdir=./aispro-cache\abi\...
      external: ... model=clahe stored 1 derived image(s)

- LEG 4 - the derived Secondary Capture series and its instance, read back
  from the PACS:

      QIDO series for derived -> 200
        series 2.25.93217597796393579365684515896021383893 | description= CLAHE | modality= DX
          instance 2.25.320747753114892393543410695592203636273 | class 1.2.840.10008.5.1.4.1.1.7

## Reproducing

The driver script, the demo configs and the seed study are part of the
internal harness and are not published. The loop itself runs on the
published `clarus` and `clinfer` binaries: Clarus in `task_producer` mode
mints the workitem on STOW, clinfer claims it, runs the model, and STOWs the
result back. This document records one such run.

## Known limitations

- The demo model copy differs from the shipped model only in the interpreter
  name (`python` vs `python3`, a Windows venv provides `python.exe`) and the
  relative script path.
- `study_description_contains` is applied BOTH at the study level (claim
  decision) and at the series level (frame selection), so a study whose
  series descriptions differ from its study description is claimed and then
  cancelled with "no matching frames".
- A live bug was found and fixed while recording this demo: a UPS attribute
  Update dropped the server's `minted_by` provenance marker, which stopped
  the readiness sweeper from promoting a minted workitem from INCOMPLETE to
  READY. Fixed in `clarus/src/handlers/ups_pure.rs` (both update handlers
  now carry the stored marker forward).
