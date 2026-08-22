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

## How we work

We report what we measure. When our field testing finds a bug in someone
else's tool, we file it publicly with numbers; when it finds a bug in ours,
we say so in the same report. The Weasis field report linked here is the
first example of both.

## Links

- [Weasis field report](./weasis-report.md)
- Upstream issues filed by the Clarus team will be linked here once filed.
