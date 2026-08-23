# Clarus + clbridge vs Orthanc: DIMSE throughput benchmark (2026-08-23)

Status: final. n = 7 clean runs per condition (21 full stack cycles executed).
All raw logs: `D:\Clarus\tmp\ab_runs\cycle{N}_{mode}.txt`, parser
`D:\Clarus\tmp\parse_ab_runs.py`, machine facts `D:\Clarus\tmp\ab_runs\machine.txt`.

## 1. Summary

| Metric (median, n=7)            | Clarus+bridge AV OFF | Clarus+bridge AV ON | Orthanc (Docker) |
|---------------------------------|----------------------|---------------------|------------------|
| C-STORE phase                   | 23.3 s               | 24.6 s              | 77.5 s (1)       |
|   = throughput                  | 44.4 MB/s            | 42.1 MB/s           | 13.4 MB/s        |
| Ingest, end to end (1035.1 MB)  | 58.8 s               | 90.3 s              | 77.5 s (1)       |
|   = throughput                  | 17.6 MB/s            | 11.5 MB/s           | 13.4 MB/s        |
| C-MOVE read (1034.7 MB)         | 34.9 s               | 36.1 s              | 103.0 s          |
|   = throughput                  | 29.7 MB/s            | 28.7 MB/s           | 10.0 MB/s        |

(1) Orthanc C-STORE is synchronous: its C-STORE phase IS its full ingest
(acceptance and commit happen before the response). For the bridge the two
rows differ: C-STORE phase = time to accept 1063 C-STOREs into the outbox;
ingest = acceptance + outbox drain, i.e. the full roundtrip from the first
C-STORE to the last byte committed to Clarus storage (incl. the 6 s
idle-flush grace).

Headlines:

1. With the recommended Defender exclusions, the Clarus+bridge C-STORE
   phase is ~3.3x faster than Orthanc's (23.3 s vs 77.5 s median; for
   Orthanc the C-STORE phase equals the full ingest because it commits
   synchronously) and the full ingest is ~24% faster (58.8 s vs 77.5 s).
   C-MOVE read is ~3.0x faster (34.9 s vs 103.0 s).
2. Windows Defender real-time scanning costs Clarus+bridge +54% on end-to-end
   ingest (58.8 s -> 90.3 s), concentrated entirely in the STOW write path
   (bridge outbox drain 36.5 s -> 65.0 s, +78%). C-STORE acceptance and reads
   are unaffected (reads are not scanned by Defender).
3. Under AV ON (worst case, no exclusions) Clarus+bridge ingest is still
   within noise of Orthanc (90.3 s vs 77.5 s) on this small VM.

## 2. Environment (single VMware VM)

- Host: VMware Virtual Platform, Windows 10 Enterprise 22H2 build 19045
- CPU: Intel Xeon E5-2690 v4 @ 2.60 GHz, 3 vCPU / 3 logical cores
- RAM: 32 GB
- Disk: VMware virtual disks (350 GB system, 150 GB D:), HDD-backed
- Docker Desktop 29.1.3 (WSL2 backend, WSL 2.7.12)
- Microsoft Defender 4.18.26070.9, real-time protection enabled

Everything (Clarus, bridge, Orthanc, bench harness) ran on the same host over
loopback. This is a stack-vs-stack comparison, not a raw filesystem benchmark.

## 3. Systems under test

| System | Version | Notes |
|--------|---------|-------|
| Clarus PACS | field exe built 2026-08-23 01:26 | thread-per-connection, no tokio, CAS + CBOR manifests |
| clbridge DIMSE bridge | field exe v0.3.0-alpha built 2026-08-23 12:17 | DIMSE -> DICOMweb adapter, outbox batching (100 SOP / 96 MB / 5 s idle) |
| Orthanc | 1.12.11 (orthancteam/orthanc:latest) | Docker container, fresh container per cycle, storage inside the container |
| Bench harness | ab_test.py (repo root) | python 3.12.10, pynetdicom 3.0.4, pydicom 3.0.2, requests 2.34.2 |

Orthanc configuration: DicomAlwaysAllowMove=true, DicomAlwaysAllowStore=true,
destination AE ABTEST_SCP registered in DicomModalities (host.docker.internal).

## 4. Corpus

1063 DICOM files / 1035.1 MB (987.2 MiB), 3 studies, 5 series:
two CT studies (432 and 626 instances) and one mixed study (3 JPEG-LS 1.2.840.10008.1.2.4.70
DX + 2 SR instances).

## 5. Harness and methodology

ab_test.py (D:\Clarus\ab_test.py) performs, per run:

- Upload: C-STORE to the bridge (port 8104) or Orthanc (port 4242), one
  instance per association, with an adaptive in-flight window: starts at 1
  concurrent association, grows by 1 per 32 MB chunk while throughput
  improves >= 3%, locks at a plateau (or earlier on an A-ASSOCIATE wall).
- Bridge-only: after the upload phase the harness polls the bridge metrics
  endpoint (clarus_bridge_outbox_pending) until the outbox is drained; the
  reported ingest time is upload acceptance + drain, i.e. true end-to-end
  ingestion through the bridge. Orthanc C-STORE is synchronous, so its upload
  time is already end to end.
- Read: C-MOVE at STUDY level to an in-process pynetdicom storage SCP that
  writes the received bytes to a temp dir and counts them. Same adaptive
  window on the receive rate.
- DIMSE hygiene: transient 0xA700 (Out of Resources, PS3.7 C.4) is retried
  with 4 x 1.5 s backoff (the bridge answers 0xA700 while its 5 s health
  poller sees Clarus down during the harness's server restarts). A run is
  "clean" only if every C-STORE and every C-MOVE succeeds; dirty runs were
  retried by the driver (max 3 attempts, fresh store each attempt) and are
  excluded from the statistics. Every attempt is kept in the raw logs.
- The SCP answers the bridge's post-move Storage Commitment N-ACTION with
  0x0000 (lab convenience; real viewers typically reject it - see section 9).

Per condition, the store was wiped and the server restarted before every run.
Orthanc was recreated as a fresh container before every upload (its read phase
reused the same container). Within a cycle, AV OFF/AV ON order alternates to
avoid time-of-day drift.

## 6. Conditions

- AV OFF: Defender exclusion paths active for the Clarus data dir and the
  bridge outbox dir (the production-recommended configuration documented in
  clarus.conf / clbridge.conf).
- AV ON: no exclusions; Defender real-time scanning of every file create in
  those paths.
- Orthanc: its storage lives inside the Docker container (ext4.vhdx); Windows
  Defender does not scan container-internal files. This is inherent to the
  deployment model and is noted as a condition, not as an Orthanc handicap
  or advantage that we introduced.

## 7. Raw results (clean runs only)

Clarus+bridge AV OFF (seconds: accept / drain / ingest / read):

| cycle | accept | drain | ingest | read |
|-------|--------|-------|--------|------|
| 1 | 26.3 | 32.5 | 58.8 | 34.4 |
| 2 | 23.5 | 36.5 | 60.0 | 34.9 |
| 3 | 21.0 | 30.4 | 51.4 | 35.0 |
| 4 | 19.3 | 38.5 | 57.8 | 34.7 |
| 5 | 23.3 | 30.5 | 53.8 | 35.0 |
| 6 | 23.3 | 40.3 | 63.7 | 33.6 |
| 7 | 22.7 | 40.8 | 63.5 | 35.9 |

Clarus+bridge AV ON:

| cycle | accept | drain | ingest | read |
|-------|--------|-------|--------|------|
| 1 | 25.3 | 65.0 | 90.3 | 36.2 |
| 2 | 28.0 | 62.5 | 90.6 | 40.2 |
| 3 | 24.6 | 62.4 | 87.0 | 36.1 |
| 4 | 21.2 | 65.0 | 86.2 | 36.0 |
| 5 | 20.7 | 66.6 | 87.3 | 36.0 |
| 6 | 19.5 | 72.7 | 92.2 | 36.5 |
| 7 | 28.0 | 64.7 | 92.7 | 36.1 |

Orthanc (upload seconds / read seconds):

| cycle | upload | read |
|-------|--------|------|
| 1 | 82.5 | 109.5 |
| 2 | 59.1 | 92.6 |
| 3 | 77.5 | 96.3 |
| 4 | 76.9 | 98.8 |
| 5 | 104.8 | 132.1 |
| 6 | 92.9 | 128.4 |
| 7 | 65.8 | 103.0 |

## 8. Statistics (n=7)

| Metric | median | mean | SD | min | max |
|--------|--------|------|----|-----|-----|
| avoff ingest (s) | 58.8 | 58.4 | 4.3 | 51.4 | 63.7 |
| avon ingest (s) | 90.3 | 89.5 | 2.4 | 86.2 | 92.7 |
| avoff drain (s) | 36.5 | 35.6 | 4.2 | 30.4 | 40.8 |
| avon drain (s) | 65.0 | 65.6 | 3.2 | 62.4 | 72.7 |
| avoff accept (s) | 23.3 | 22.8 | 2.0 | 19.3 | 26.3 |
| avon accept (s) | 24.6 | 23.9 | 3.2 | 19.5 | 28.0 |
| avoff read (s) | 34.9 | 34.8 | 0.6 | 33.6 | 35.9 |
| avon read (s) | 36.1 | 36.7 | 1.4 | 36.0 | 40.2 |
| orthanc upload (s) | 77.5 | 79.9 | 14.4 | 59.1 | 104.8 |
| orthanc read (s) | 103.0 | 108.7 | 14.5 | 92.6 | 132.1 |

AV effect on Clarus+bridge (medians): ingest +53.6%, outbox drain +78.1%,
C-STORE acceptance +5.6%, read +3.4%. The AV penalty is entirely in the STOW
write path; acceptance and reads are AV-insensitive (Defender scans creates,
not reads).

Read throughput medians: 29.7 MB/s (avoff) vs 28.7 MB/s (avon) vs 10.0 MB/s
(Orthanc). Ingest throughput medians: 17.6 MB/s (avoff) vs 11.5 MB/s (avon)
vs 13.4 MB/s (Orthanc upload). C-STORE phase throughput medians:
44.4 MB/s (avoff) vs 42.1 MB/s (avon) vs 13.4 MB/s (Orthanc, synchronous
C-STORE).

## 9. Honest notes and limitations

- Single VM, loopback, HDD-backed virtual disks, 3 vCPU. Numbers are
  comparable between the two stacks on this box; they are not production
  capacity claims.
- n=7; we report median/mean/SD/range, not p-values. Orthanc upload/read
  variance is materially higher (SD ~14 s) than the Clarus+bridge variance
  (SD 2-4 s on ingest).
- The bridge's C-STORE acceptance is asynchronous by design: 0x0000 means
  "accepted to the outbox", not "committed to Clarus". The honest ingest
  metric is acceptance + outbox drain, which is what we report.
- Harness restarts Clarus between runs; during the ~5 s health-check gap the
  bridge legitimately answers 0xA700 and the harness retries (PS3.7 C.4).
- Storage Commitment: the bridge sends a post-move N-ACTION and, per its push
  model, one N-EVENT-REPORT per batch. Real viewers generally do not
  implement Storage Commitment (nothing to confirm - the PACS keeps the data);
  the bridge treats every SC outcome as non-fatal. During this benchmark the
  field bridge exe predated the N-EVENT-REPORT fix found by the new wire
  regression test (tests/test_storage_commitment_wire.py), so no N-EVENT-REPORT
  was observed in the harness; this has no impact on the throughput metrics.
- Defender exclusions are the documented production recommendation and were
  active only in the AV OFF condition.

## 10. Reproducibility

- Harness: D:\Clarus\ab_test.py (usage in its docstring).
- Driver: D:\Clarus\tmp\ab_loop.ps1 (7 cycles, self-healing retries).
- Parser: D:\Clarus\tmp\parse_ab_runs.py -> ab_runs\summary.json.
- Orthanc config: D:\Clarus\fieldtest\orthanc.json.
