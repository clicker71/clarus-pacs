# Clarus PACS

Clarus is a DICOMweb PACS server (QIDO-RS, WADO-RS, STOW-RS, UPS-RS) written
in Rust.

- **Status:** closed preview (pre-release). The source repository is private.
- **Footprint:** ~1.7 MB single static executable, zero runtime dependencies -
  no VCRedist, no JVM, no interpreter. Smaller than two of the CT slices it
  stores. Fits on a single 2.88 MB ED floppy - the format that lost to the
  50-cent HD.
- **Conformance:** being tested against DICOM PS3.18, including field
  interoperability testing with the Weasis viewer. Public test artifacts and
  field reports are linked from this repository.
- **Testing:** every release passes the public ap101 hot-path harness
  (https://github.com/clicker71/ap101) before it ships - hot paths are
  verified, not assumed. The repository opens when this bar holds, not
  before.
- **What is public now:**
  - Weasis interoperability field report (`weasis-report.md` in this
    repository)
  - DIMSE throughput benchmark vs Orthanc (`benchmark.md` in this
    repository), produced by the public harness `tools/ab_test.py`
  - Bug reports and discussions we file against third-party DICOM tooling
- **What is not public yet:** source code, binaries, documentation.
- **License:** the source already carries LGPL-3.0 headers; it is published
  under LGPL-3.0 when the preview ends. Closed preview is a quality gate,
  not a business model.
- **Contact:** issues in this repository (fastest), or
  https://github.com/clicker71.
  No raw email addresses - public READMEs get harvested by spam bots.

## Under the hood

- **No SQL.** Own B-tree index over the object store; no database server,
  no ORM.
- **No async runtime.** Synchronous thread-per-connection HTTP stack;
  timeouts and backpressure are ours, not a runtime's.
- **Content-addressed storage.** Every blob is keyed by its BLAKE3 hash in a
  two-level sharded layout (`{xx}/{yy}/{hash}.dcm`); re-sends deduplicate.
- **Search without a search engine.** Unicode-safe bitap fuzzy matching for
  patient names; no external index service.

## Legacy DIMSE

Clarus speaks DICOMweb. For the installed base of DIMSE-only modalities and
viewers, the companion bridge `clbridge` (Python, in the same source tree -
one repo by design) terminates the legacy protocol:

- **C-STORE** (SCP) from modalities - 71 SOP classes in the field build -
  with automatic charset coercion, then STOW-RS into Clarus;
- **C-FIND** (SCP) from legacy viewers - mapped onto QIDO-RS (13/13 query
  attributes);
- **C-MOVE** (SCU) toward legacy viewers, served from Clarus WADO-RS;
- **N-ACTION** (request handling; the storage commitment loop closes
  with N-EVENT-REPORT - field-verified 2026-08-23);
- optional **C-PRINT**;
- **C-ECHO**.

A study sent by a decades-old CT scanner lands in the same content-addressed
store as DICOMweb traffic and is immediately searchable through QIDO-RS -
including Unicode-safe fuzzymatch on patient names. Store-and-forward DIMSE
traffic is what the bridge was built for.

## Throughput benchmark (2026-08-23)

Clarus + clbridge vs Orthanc 1.12.11 over DIMSE, on the same VMware VM
(3 vCPU, 32 GB RAM, HDD-backed virtual disks), loopback. n = 7 clean runs
per condition; corpus 1035.1 MB (1063 instances, 3 studies). Full
methodology, per-run tables, statistics and honest limitations:
[benchmark.md](./benchmark.md).

| Median | Clarus+bridge, AV OFF | Clarus+bridge, AV ON | Orthanc (Docker) |
|--------|-----------------------|----------------------|------------------|
| C-MOVE read | 34.9 s (29.7 MB/s) | 36.1 s (28.7 MB/s) | 103.3 s (10.0 MB/s) |
| Ingest end to end (HOP1+HOP2) | 58.8 s (17.6 MB/s) | 90.3 s (11.5 MB/s) | 76.9 s (13.5 MB/s) (1) |
| +- HOP1: C-STORE into outbox | 23.3 s (44.4 MB/s) | 24.6 s (42.1 MB/s) | 76.9 s (1) |
| +- HOP2: outbox drain -> Clarus STOW | 36.5 s (28.4 MB/s) | 65.0 s (15.9 MB/s) | - (single hop) |

(1) Orthanc does the whole job in ONE synchronous hop: its C-STORE phase is
its full ingest. The bridge ingest is TWO hops: HOP1 = asynchronous DIMSE
acceptance into the outbox, HOP2 = drain into Clarus - the honest
like-for-like number is HOP1+HOP2. HOP1 alone must never be quoted as the
ingest rate (it is asynchronous acceptance, not commit). The two hops
overlap slightly, so HOP1+HOP2 is a bit above the measured ingest.
n = 7 clean runs per condition (Clarus+bridge); Orthanc n = 9 uploads /
n = 10 reads. AV OFF = the documented Defender exclusions on the Clarus data
dirs; the AV penalty (+54% ingest) lands entirely on the STOW write path.
Without the exclusions, Clarus+bridge is ~17% SLOWER than Orthanc - the
exclusions are an operational requirement, not a tuning tip.

## Scale-out shape: N bridges, one Clarus core

The deployment model is one Clarus core plus one bridge sidecar per
modality. First empirical probe (same VM, AV OFF, exploratory n=1 - a
formal multi-stream suite would rerun this 3-5 times), two CT studies
(1012.4 MB) sent through two bridges concurrently vs two streams into a
single Orthanc instance:

| | Clarus + 2 bridges (8104, 8106) | Orthanc, 2 streams, 1 instance |
|---|---|---|
| aggregate wall | 39.8 s | 69.0 s |
| aggregate MB/s | 25.4 | 14.7 |
| vs single-stream median | +44% | +9% |
| per-modality release | 16.5 s / 15.8 s | 66.6 s / 53.6 s |

The bridge side scales with the number of bridges (aggregate +44%; the
shared Clarus STOW path is the cap, ~50 MB/s on this HDD VM). Orthanc
barely scaled: its two streams collapsed to 7.4 and 9.7 MB/s each (SQLite
commit serializes writes). Shape of the curve: the lead widens with the
number of concurrent modalities, and the modality-facing win is even
larger - the outbox absorbs the backlog and releases the scanner in
~16 s vs ~54-67 s. Full numbers and caveats: [benchmark.md](./benchmark.md)
section 1.1.

The universal harness that produced these numbers is public:
[tools/ab_test.py](./tools/ab_test.py) - point it at any DIMSE or DICOMweb
server, it discovers the throughput plateau itself (adaptive window, outbox
drain polling, DIMSE retry hygiene). Compare anything with anything while
the source repository is still in closed preview.

## How we work

We report what we measure. When our field testing finds a bug in someone
else's tool, we file it publicly with numbers; when it finds a bug in ours,
we say so in the same report. The Weasis field report linked here is the
first example of both.

## Links

- [Weasis field report](./weasis-report.md)
- [DIMSE throughput benchmark vs Orthanc](./benchmark.md)
- [Universal DICOM A/B harness](./tools/ab_test.py)
- Upstream issues filed by the Clarus team will be linked here once filed.
