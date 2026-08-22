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
viewers, the companion bridge `clbridge` (Python) terminates the legacy
protocol:

- **C-STORE** (SCP) from modalities - 71 SOP classes in the field build -
  with automatic charset coercion, then STOW-RS into Clarus;
- **C-FIND** (SCP) from legacy viewers - mapped onto QIDO-RS (13/13 query
  attributes);
- **C-MOVE** (SCU) toward legacy viewers, served from Clarus WADO-RS;
- **N-ACTION** (request handling; the storage commitment loop closes
  with N-EVENT-REPORT - in progress);
- optional **C-PRINT**;
- **C-ECHO**.

A study sent by a decades-old CT scanner lands in the same content-addressed
store as DICOMweb traffic and is immediately searchable through QIDO-RS -
including Unicode-safe fuzzymatch on patient names. Store-and-forward DIMSE
traffic is what the bridge was built for.

## How we work

We report what we measure. When our field testing finds a bug in someone
else's tool, we file it publicly with numbers; when it finds a bug in ours,
we say so in the same report. The Weasis field report linked here is the
first example of both.

## Links

- [Weasis field report](./weasis-report.md)
- Upstream issues filed by the Clarus team will be linked here once filed.
