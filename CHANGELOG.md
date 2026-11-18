# Changelog

All notable changes to Clarus PACS are documented here.

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
