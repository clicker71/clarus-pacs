# Clarus + clbridge vs Orthanc: DIMSE throughput benchmark (2026-08-23)

Status: final. n = 7 clean runs per condition (21 full stack cycles) for
Clarus+bridge; Orthanc extended to n = 9 clean uploads / n = 10 clean reads
to tighten its high variance.
All raw logs: `D:\Clarus\tmp\ab_runs\cycle{N}_{mode}.txt`, parser
`D:\Clarus\tmp\parse_ab_runs.py`, machine facts `D:\Clarus\tmp\ab_runs\machine.txt`.

## 1. Summary

| Metric (median)                  | Clarus+bridge AV OFF | Clarus+bridge AV ON | Orthanc (Docker) |
|----------------------------------|----------------------|---------------------|------------------|
| C-MOVE read (1034.7 MB)          | 34.9 s               | 36.1 s              | 103.3 s          |
|   = throughput                   | 29.7 MB/s            | 28.7 MB/s           | 10.0 MB/s        |
| Ingest, end to end = HOP1+HOP2 (1035.1 MB) (1)(2) | 58.8 s      | 90.3 s              | 76.9 s (1)       |
|   = throughput                   | 17.6 MB/s            | 11.5 MB/s           | 13.5 MB/s        |
| +- HOP1: DIMSE C-STORE into outbox (2)(3) | 23.3 s       | 24.6 s              | 76.9 s (1)       |
|   = throughput                   | 44.4 MB/s            | 42.1 MB/s           | 13.5 MB/s        |
| +- HOP2: outbox drain -> Clarus STOW | 36.5 s          | 65.0 s              | - (single hop)   |
|   = throughput                   | 28.4 MB/s            | 15.9 MB/s           | -                |

Sample sizes: n = 7 clean runs per condition for Clarus+bridge; Orthanc
n = 9 clean uploads / n = 10 clean reads.

(1) Orthanc does the whole job in ONE synchronous hop: its C-STORE phase IS
its full ingest (acceptance and commit happen before the response).
(2) Hop model: the bridge ingest is TWO hops - HOP1: DIMSE C-STORE
acceptance into the outbox (asynchronous; 0x0000 means "accepted to the
outbox", NOT "committed to Clarus"); HOP2: outbox drain -> STOW-RS into
Clarus (CAS + manifest + idx). The honest like-for-like ingest vs Orthanc's
single hop is HOP1+HOP2 - the row we headline. HOP1 and HOP2 overlap
slightly (the outbox drains while the modality keeps sending, and ingest
includes the 6 s idle-flush grace), so the two hop rows add up to a bit
more than the measured ingest wall. The bridge is a legacy sidecar,
deployable one per modality, and is expected to fade as modalities move to
DICOMweb; the optimization target is the Clarus core (the DICOMweb path).
(3) HOP1 "accept only" is the bridge's asynchronous C-STORE acceptance. It
is NOT comparable to Orthanc's synchronous C-STORE (which commits before
the response); nobody may quote 44.4 MB/s as the ingest number - the
like-for-like row is HOP1+HOP2 above it.

Headlines:

1. C-MOVE read is ~3.0x faster (34.9 s vs 103.3 s median): the strongest
   like-for-like metric - both sides transfer synchronously.
2. End-to-end ingest is ~24% faster WITH the recommended Defender exclusions
   (58.8 s vs 76.9 s; Orthanc commits synchronously, so its C-STORE phase
   IS its full ingest).
3. WITHOUT the exclusions, Defender real-time scanning eats 54% of ingest
   (58.8 s -> 90.3 s), concentrated in the STOW write path (outbox drain
   +78%). In that worst case Clarus+bridge is ~17% SLOWER than Orthanc
   (90.3 s vs 76.9 s) - stated directly, not "within noise".

### 1.1 Two-stream concurrency probe (exploratory, n=1)

Two CT studies (491.4 MB + 521.0 MB = 1012.4 MB) sent concurrently, same
conditions (AV OFF, fresh stores):

| | Clarus + 2 bridges (8104, 8106) | Orthanc, 2 streams, 1 instance |
|---|---|---|
| aggregate wall | 39.8 s | 69.0 s |
| aggregate MB/s | 25.4 | 14.7 |
| vs single-stream median | +44% | +9% |
| per-modality release | 16.5 s / 15.8 s | 66.6 s / 53.6 s |

The bridge side scaled (aggregate +44%; the shared Clarus STOW path is the
cap, ~50 MB/s in the drain phase on this HDD VM). Orthanc barely scaled:
its two streams collapsed to 7.4 and 9.7 MB/s each (SQLite commit
serializes writes). Shape of the curve: our lead widens with the number of
concurrent modalities; the modality-facing win is even larger (the outbox
absorbs the backlog, the scanner is released in ~16 s vs ~54-67 s).
Exploratory n=1 - a formal multi-stream suite would rerun this 3-5 times.

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
| clbridge DIMSE bridge | field exe v0.3.0-alpha built 2026-08-23 12:17 | DIMSE -> DICOMweb™ adapter, outbox batching (100 SOP / 96 MB / 5 s idle) |
| Orthanc | 1.12.11 (orthancteam/orthanc:latest) | Docker container, fresh container per cycle, storage inside the container |
| Bench harness | ab_test.py (repo root) | python 3.12.10, pynetdicom 3.0.4, pydicom 3.0.2, requests 2.34.2 |

Orthanc configuration: DicomAlwaysAllowMove=true, DicomAlwaysAllowStore=true,
destination AE ABTEST_SCP registered in DicomModalities (host.docker.internal).

## 4. Corpus

1063 DICOM® files / 1035.1 MB (987.2 MiB), 3 studies, 5 series:
two CT studies (432 and 626 instances) and one mixed study (3 JPEG-LS 1.2.840.10008.1.2.4.70
DX + 2 SR instances).

## 5. Harness and methodology

The published harness
[ab_test.py](./tools/ab_test.py)
performs, per run:

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
- Hop model: the bridge ingest is TWO hops - HOP1: DIMSE C-STORE acceptance
  into the outbox (median 23.3 s); HOP2: outbox drain into Clarus STOW
  (median 36.5 s). Orthanc does the same work in ONE synchronous hop
  (median 76.9 s). The optimization target is the Clarus core (the DICOMweb
  path); the bridge is a legacy sidecar, deployable one per modality, and we
  expect it to fade as modalities move to DICOMweb. Even with two hops the
  end-to-end ingest beats Orthanc's single hop because Orthanc pays a
  per-instance synchronous commit while the bridge batches and overlaps.
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
- Deployment model honesty: Orthanc ran in Docker because there is NO
  upstream native Windows build - Docker is the supported Windows deployment
  and what clinics actually run. WSL2 is heterogeneous: CPU is near-native
  (~0-5% overhead), but loopback goes through the WSL vNIC (higher
  per-request latency than a native loopback), and small synchronous writes
  pay the layered ext4.vhdx-over-NTFS-over-HDD-vmdk stack - exactly
  Orthanc's per-C-STORE pattern (DICOM file + SQLite index). So its 13.4
  MB/s is "Orthanc in its default Windows deployment on HDD", not "Orthanc
  is slow". Our bridge writes the same disk on the same machine, but
  asynchronously in batches (100 SOP / 96 MB) - a product architecture
  choice, not benchmark tuning.

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
| 8 | (dirty, excluded) | 103.6 |
| 9 | 69.7 | 97.5 |
| 10 | 73.2 | 104.7 |

## 8. Statistics

n = 7 clean runs per condition for Clarus+bridge. Orthanc: n = 9 clean
uploads, n = 10 clean reads (extended to tighten its high variance).

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
| orthanc upload (s) (n=9) | 76.9 | 78.0 | 13.2 | 59.1 | 104.8 |
| orthanc read (s) (n=10) | 103.3 | 106.7 | 12.7 | 92.6 | 132.1 |

AV effect on Clarus+bridge (medians): ingest +53.6%, outbox drain +78.1%,
C-STORE acceptance +5.6%, read +3.4%. The AV penalty is entirely in the STOW
write path; acceptance and reads are AV-insensitive (Defender scans creates,
not reads).

Read throughput medians: 29.7 MB/s (avoff) vs 28.7 MB/s (avon) vs 10.0 MB/s
(Orthanc). Ingest throughput medians: 17.6 MB/s (avoff) vs 11.5 MB/s (avon)
vs 13.5 MB/s (Orthanc upload). C-STORE phase throughput medians:
44.4 MB/s (avoff) vs 42.1 MB/s (avon) vs 13.5 MB/s (Orthanc, synchronous
C-STORE).

## 9. Honest notes and limitations

- Single VM, loopback, HDD-backed virtual disks, 3 vCPU. Numbers are
  comparable between the two stacks on this box; they are not production
  capacity claims.
- n=7 per condition (Clarus+bridge); Orthanc n=9 uploads / n=10 reads. We
  report median/mean/SD/range, not p-values. Orthanc variance remains higher
  (SD ~13 s) than Clarus+bridge (SD 2-4 s on ingest).
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
  The fix (plus peer-config resolution of the report destination) was built
  and field-verified after the benchmark: N-ACTION-RSP 0x0000, 5/5 SOPs
  WADO-confirmed, N-EVENT-REPORT delivered back (event_type=1).
- Defender exclusions are the documented production recommendation and were
  active only in the AV OFF condition.

## 10. Reproducibility

- Harness: [tools/ab_test.py](./tools/ab_test.py)
  (usage in its docstring; local working copy D:\Clarus\ab_test.py).
- Driver: D:\Clarus\tmp\ab_loop.ps1 (7 cycles, self-healing retries).
- Parser: D:\Clarus\tmp\parse_ab_runs.py -> ab_runs\summary.json.
- Orthanc config: D:\Clarus\fieldtest\orthanc.json.

## 11. Trademarks

DICOM® is the registered trademark of the National Electrical Manufacturers
Association (NEMA) for its standards publications relating to digital
communications of medical information. DICOMweb™ is a trademark of NEMA.
Clarus is not affiliated with or endorsed by NEMA.
