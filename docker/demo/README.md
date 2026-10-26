# OHIF UPS-RS demo: Clarus + clinfer + seed (self-contained)

A reproducible DICOMweb UPS-RS demo end to end. Clarus is the **origin**
(Task Manager), clinfer the **Task Performer**, and the viewer is OHIF with
the `cornerstone-dicom-ups` extension. One patient `XRV^DEMO` with two
studies: a chest study the demo filter applies to, and a knee study it does
not - the negative control.

Nothing here is compiled from source: the Clarus and clinfer images download
their prebuilt Linux binaries from the `weasis-demo-v0.1` release, and the
demo study (chest + knee) from the `ohif-demo-v0.1` release, each verified
against pinned SHA-256 hashes.

## Run

    docker compose -f docker-compose.ohif-demo.yml up -d --build

DICOMweb root for OHIF: `http://127.0.0.1:8024/dicomweb`. On first boot the
`seed` service STOWs the two demo studies, then exits.

## Reset

State lives only in the named volumes (`clarus-data`, `clinfer-cache`).

    docker compose -f docker-compose.ohif-demo.yml down -v
    docker compose -f docker-compose.ohif-demo.yml up -d --build

`down -v` deletes the volumes (factory reset); the seed re-runs on the next
fresh boot. The seed is idempotent: a plain `up -d` without `-v` does not
duplicate the studies.

## What the demo shows

The `clinfer` config ships two model-free filters under the `99CLARUS`
scheme: `CXR-CLAHE` (contrast enhancement) and `CXR-UPSCALE` (bicubic x2).
A viewer that speaks UPS-RS (OHIF with `cornerstone-dicom-ups`) creates a
workitem; clinfer claims it, runs the filter, and stores the derived series
back to the same origin, SCHEDULED → IN PROGRESS → COMPLETED.

The chest study carries a `LUNG` body part, so the filter applies and the
workitem reaches COMPLETED. The knee study has no body part, so the same
code is cancelled by the performer - the negative control that shows the
catalogue rules decide what runs.

## Live URL (optional)

Uncomment the `cloudflared` service in the compose to expose the origin
through a Cloudflare quick tunnel, or publish the images to GHCR and point a
named tunnel at `http://clarus:8024`.
