# Changelog

All notable changes to Clarus PACS are documented here.

## [0.3.4-beta] — 2026-11-26

### Added

- **clinfer serves a FHIR R4 capability document at
  `/processors/Questionnaire`.** A `searchset` Bundle holds one Questionnaire
  per enabled model. The output is R4-conformant: extension URLs are absolute
  canonical URLs, the provider is a complex extension (name/vendor/version),
  `Coding.system` is a URI (`urn:clarus:scheme:<designator>`) rather than a
  bare private scheme designator, and `Bundle.link.url` / `entry.fullUrl` are
  absolute URLs built from the request's Host header.
- **`fhir.extension_base` is configurable** (default: the public repository).
  The extension StructureDefinitions ship under `clinfer/fhir/StructureDefinition/`
  and the listener mirrors them at `/fhir/StructureDefinition/*.json` (the
  bytes are compiled in), so offline validators can dereference the canonical
  URLs without reaching the public copy.
- **The model procedure code selects the model.** The derived rule carries the
  `procedure_code` Code Value as its workitem-code key; `clahe.yaml` uses the
  local `99CLARUS` code `IMG-CLAHE` instead of a SNOMED procedure.
- **clinfer runs out of the box with a license-free CLAHE task.** The default
  configuration enables a built-in CLAHE filter (`99CLARUS` `IMG-CLAHE`) that
  needs no model weights, carries no license encumbrance, and runs on modest
  hardware, so a fresh install exercises the full UPS-RS AI loop immediately.
  The result is stored as a DERIVED series - a second, contrast-enhanced copy
  beside the source, which is never modified. To clinfer, CLAHE and a
  heavyweight inference model are the same contract: swap the model file to
  plug in a detector or segmentation model.

### Fixed

- **clinfer reads the UPS input study from Referenced Request Sequence
  (0040,A370).** A conformant Unified Procedure Step carries the study it
  relates to in Referenced Request Sequence (PS3.4 CC.2.5-3, Type 1); clinfer
  previously read only Input Information Sequence (0040,4021), so workitems
  whose study rode in (0040,A370) were cancelled with "no rule matched". The
  input study is now read from (0040,A370) first, with (0040,4021) as a
  fallback.
- **clinfer reads LongCodeValue (0008,0119) as a CodeValue fallback.** A code
  sequence item that carries the long form instead of (0008,0100) now resolves
  its code correctly.

### Conformance

- **Series-level QIDO returns the Study Instance UID.** The JSON and XML
  series writers now inject (0020,000D) from the study context on both the
  scoped (`/studies/{study}/series`) and global (`/series`) routes, and an
  explicit `includefield=0020000D` is no longer silently ignored. The Study
  UID is not a Table 10.6.3-4 Series return key, so it is emitted as an
  always-on serving attribute rather than consuming an includefield slot.
  Clients that read it unguarded (dwv) no longer crash on a conformant server.

### Server robustness

- **Worklist, UPS-RS and Storage Commitment conformance** hardened across
  request handling, readiness state and the XML/JSON renderers.
- **STOW conformance**: malformed or conflicting instances are rejected before
  the batch is committed, and the commitment remainder paths are closed.
- **Data-movement workers publish only after verify-before-publish**: a
  partially written object is never exposed as stored.
- **PHI masking, the I/O fault seam, lock scoping and the FFI boundary**
  tightened.
- **Log durability, shutdown ordering and validation conformance** aligned
  with the standard.

### DIMSE bridge

- **C-MOVE retrieves PDF and Segmentation instances again.** The C-MOVE
  pre-flight misread the query result and failed, so the association proposal
  overflowed its presentation-context limit and dropped these SOP classes;
  the pre-flight now consumes the query page as produced, and all negotiated
  classes are proposed.
- **C-STORE conflict handling**: the bridge answers a store association only
  after the origin has decided the STOW, and the server rejects a second
  patient stored under the same study within a single request, not only
  across requests.
- **The conformance statement now documents the bridge SCP transfer-syntax
  list, duplicate-stored semantics, and the server media types.**

## [0.3.3-beta] — 2026-11-18

### UPS-RS / Task Manager interoperability

- **Wildcard-prefix label filters now deliver notifications.** A filtered
  Worklist Subscription carrying `filter=00741204=PREFIX*` (Procedure Step
  Label) previously accepted the request but delivered no N-EVENT; it now
  matches wildcard prefixes per PS3.4 C.2.2.2, while exact-label filters stay
  exact (`PREFIX` never matches `PREFIX_MORE`). Reported by OctoFHIR.
- **Scheduled Procedure Step Start DateTime (0040,4005) matching** now honours
  the full DT grammar: UTC offsets after truncated values, fractional seconds,
  day/month/hour/minute period semantics, negative offsets at every precision,
  and a configurable deployment offset (`[ups] local_utc_offset`) for
  offset-free values compared in the local frame.

### Durability

- Uniform crash-safety discipline across the manifest, workitem (UPS),
  commitment, and MWL/MPPS stores: file data and directory entries are
  persisted before an operation reports success, deletion fsyncs its parent
  directory, and encoded bytes are validated before the atomic rename replaces
  a good manifest.

### Conformance

- QIDO DA/TM range matching (StudyTime, PPS Start Date/Time) with inclusive
  open bounds.
- QIDO study/instance emission now includes Patient ID Issuer (0010,0021) and
  Number Of Frames (0028,0008); series answers carry the captured PPS and
  Request attributes in both JSON and XML.
- MPPS performed-series references are nested under Referenced Image Sequence
  (0008,1140) and carry the Series Instance UID.
- Native DICOM Model XML uses the schema element names `NamePrefix` /
  `NameSuffix`; XML-1.0-forbidden control characters are dropped.
- UID-list matching for SOPClassUID; `2.25` UIDs derive from the full 128-bit
  UUID.

### Memory safety

- Manifest reads no longer mmap a file whose immutability the server cannot
  guarantee: the mmap entry points are `unsafe` behind a typed
  `TrustedManifestDir`, and arbitrary paths use a copy-based read.

### Correctness and robustness

- Manifest cache abort/reader-block invalidation, retention sweep TOCTOU fixed
  with a UID stripe lock, HTTP `Connection` header flag accumulation, head
  terminator checked before the read deadline, prefetch byte order, torn-tail
  boundaries in the modality index and recovery, manifest attribute merge
  order, and encode size bounds.
- IOCM hide-only rejections (113037/113038) can no longer be re-accepted by a
  later STOW.

### Testing

- Allocation-measurement tests isolated from parallel test interference.

## [0.3.2-beta] — 2026-11-13

### Performance

- **STOW-RS ingest** is up to ~28% faster on HDD-backed storage: the body
  write now uses a 1 MiB buffered writer instead of 64 KiB chunks (A/B
  median 5.26 s → 3.78 s for an 824 MiB instance on a VMware virtual HDD).
  Durability contract unchanged.

## [0.3.1-beta] — 2026-11-12

### Interoperability

- **STOW-RS** now honours `Accept: application/dicom+xml`; Weasis no longer
  fails with "Content is not allowed in prolog" on a store-with-warnings
  response.
- **WADO-RS** metadata and the stored object now carry the coerced patient
  identity for anonymised studies with an empty PatientID/PatientName — QIDO
  and `/metadata` agree, so viewers and FHIR gateways no longer fail with a
  missing PatientID.
- **UPS-RS**: conformant workitems, subscription and event-channel handling,
  and correct cancel-state transitions. Thanks to
  [@octoshikari](https://github.com/octoshikari) for reporting the
  non-conformance and the reference implementation.

### Storage Commitment

- Fixed ownership/abandonment races, verdict-persistence retries, and
  reloaded-abandoned scheduling — commitment verdicts are no longer lost
  across restarts.
- Only `Success` (0x0000) is treated as an accepted commitment; warning and
  failure statuses are reported as not committed.

### DICOM Print

- Correct handling of `0x0116` (Attribute Value Out of Range) and `0xB603`
  (empty film); films with insufficient capacity are rejected before printing
  instead of being reported as printed; `SpecificCharacterSet` and
  `MemoryAllocation` are preserved on the Film Session.

### Query / Retrieve

- C-FIND level handling and UID quoting hardened; C-MOVE worker scheduling
  no longer blocks other peers while waiting for a busy destination.

### Server and listener

- The HTTP listener validates the `Content-Type` media type exactly and
  bounds request-line and header reading with a total deadline.
- STOW-RS serialisation failures are isolated per instance, so one bad
  dataset no longer aborts the rest of the batch.

### Conformance and robustness

- Broad conformance and robustness fixes across the DICOMweb server and the
  DIMSE bridge (query parsing, status-code handling, persistence ordering,
  and metrics).
