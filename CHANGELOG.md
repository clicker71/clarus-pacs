# Changelog

All notable changes to Clarus PACS are documented here.

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
