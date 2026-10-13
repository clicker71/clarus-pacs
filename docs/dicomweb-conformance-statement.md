# DICOM Conformance Statement - Clarus

**Product:** Clarus DICOMweb origin server (`clarus` binary)
**Version:** 0.3.0-alpha
**Date:** 2026-09-24
**Standard:** DICOM PS3.18 (Web Services), 2026c edition; DICOM PS3.19 (XML
application of DICOM to web resources); DICOM PS3.2 (Conformance statement
structure)

This statement describes the DICOMweb conformance of the Clarus server. The
Clarus DIMSE bridge has its own statement:
[dimse-bridge-conformance-statement.md](./dimse-bridge-conformance-statement.md).

---

## 1. Implementation Model

Clarus is a high-performance DICOMweb **origin server** for medical imaging
archives: a single static binary with no external database and no async
runtime. Received instances are stored as-is in a content-addressed archive
(BLAKE3); search and retrieval are served from per-study CBOR manifests and
append-only B-tree/n-gram indexes.

- **Roles:** origin server for QIDO-RS, WADO-RS, STOW-RS, UPS-RS,
  WADO-URI and Storage Commitment (PS3.18 Section 13).
- **Real-world activities:** receives instances via STOW-RS (directly or
  through the Clarus DIMSE bridge), stores them durably, answers queries
  (QIDO-RS) and serves retrievals (WADO-RS, WADO-URI) to DICOMweb user agents
  such as viewers and workstations.
- **Concurrency model:** thread-per-connection; scheduling is delegated to
  the OS kernel.
- **Storage:** no embedded SQL database; manifests and indexes are memory-
  mapped for zero-copy reads; every stored instance keeps its received
  transfer syntax and full DICOM metadata.

## 2. Studies Service

### 2.1 Store (STOW-RS)

**Resource:** `/dicomweb/studies` (also `/dicomweb/studies/{study}`).

**Request media type:** `multipart/related; type="application/dicom"`.
**Response media type:** `application/dicom+json`.

**Required attributes per instance:** Study Instance UID (0020,000D),
Series Instance UID (0020,000E), SOP Instance UID (0008,0018), SOP Class
UID (0008,0016), Patient ID (0010,0020). Instances whose Study/Series/SOP
Instance UIDs violate the DICOM UID grammar are refused per instance before
any archive write, so a stored instance always stays addressable by
WADO-RS/QIDO-RS.

**Store Response Status Codes:**

| Code | Meaning |
|---|---|
| 200 | all instances stored, including exact-duplicate idempotent re-stores |
| 202 | stored with warnings (e.g. coerced Type 2 elements) or partial failures; per-instance failure reasons carried in the response payload |
| 400 | nothing stored (malformed request) |
| 409 | conflict (SOP-level) |

**Store Response Payload:** `ReferencedSOPSequence` per instance with the
outcome. Warning-level coercion is reported with Failure Reason
(0008,1197) `0116H` / Failed SOP attributes; the standard codes from
PS3.18 Section 10.5.2.1 are used as applicable.

**Conflict semantics:** exact duplicates are **not** conflicts. Per
PS3.18 10.5.3.1 a conflict is, e.g., an unsupported SOP Class or a Study
Instance UID mismatch; adding instances to an existing Study is allowed by
10.5.2. A byte-identical re-STOW is therefore a successful `200`
(idempotent). Updating an existing SOP Instance (same SOP Instance UID,
different content) is processed as an update-in-place: the manifest
re-points to the new content and the previous blob is reclaimed as orphan.

**Progress visibility:** `STOW_STUDY` progress log lines every 50 slices;
the final access-log line of a STOW request is written as soon as the
request completes.

### 2.2 Retrieve (WADO-RS)

| Resource | Media type returned |
|---|---|
| `/studies/{study}` | `multipart/related; type="application/dicom"` |
| `/series/{series}` | `multipart/related; type="application/dicom"` |
| `/instances/{sop}` | `application/dicom` (supports `Range`, partial content `206`) |
| `/instances/{sop}/metadata` | `application/dicom+json` (default); `application/dicom+xml` on negotiation |
| `/studies/{study}/series/{series}/metadata` | `multipart/related; type="application/dicom+json"` when Accept asks multipart (Required media type, PS3.18 10.4.4); a single-part `application/dicom+json` array otherwise (see 11.11) |
| `/instances/{sop}/frames/{n}` | `multipart/related; type="application/octet-stream"` with a per-frame part `Content-Type` naming the stored transfer syntax (see below) |
| `/instances/{sop}/thumbnail` | `image/jpeg` 64x64 (see 2.4) |
| bulkdata URIs (`/bulkdata/{tag}`) | `application/octet-stream` |

**Multipart part headers:** every part of a study/series/frames multipart
response carries
`Content-Type: application/dicom; transfer-syntax=<stored transfer syntax>`
(instance-level) and `Content-Location` pointing at the part resource.
Frame parts carry the PS3.18 media type for the stored transfer syntax:
`image/jpeg` (JPEG Baseline/Lossless), `image/x-dicom-rle` (RLE),
`image/x-jls` (JPEG-LS), `image/jp2` (JPEG 2000), `image/jhc` (HTJ2K),
`application/octet-stream` otherwise - always with the
`transfer-syntax=` parameter.

**Retrieve Transfer Syntax:** the `?transferSyntax=` query parameter per
PS3.18 10.4.2. Instances are served in their stored transfer syntax by
default; on-the-fly transcoding (JPEG 2000, JPEG-LS, uncompressed targets)
is a compile-time feature (`--features transcode`). A requested transfer
syntax that cannot be produced returns `406`.

**Retrieve Metadata:** metadata responses carry the full stored DICOM
metadata. Values longer than 64 bytes (such as long Lookup Table values)
are returned as `BulkDataURI` references to the bulkdata resource.

**Retrieve Response Status Codes:**

| Code | Meaning |
|---|---|
| 200 | success (full content) |
| 206 | partial content (instance `Range` requests) |
| 400 | malformed request |
| 404 | unknown study/series/instance |
| 406 | requested media type or transfer syntax cannot be produced |

Optional query parameters (viewport, windowing, annotation, quality,
`charset`, `anonymize`): **not supported**.

**SOP classes:** no fixed allow-list. Any well-formed Composite SOP
Instance is stored and served in its received transfer syntax. Rendered
Presentation States: not supported.

### 2.3 Search (QIDO-RS)

**Resources:**

| Resource | Supported |
|---|---|
| `/studies` | yes |
| `/series` | yes (global and study-scoped) |
| `/instances` | yes (global, study- and series-scoped) |
| `/patients` (mini-viewer resource) | yes |

**Media types:** `application/dicom+json` (default) and
`application/dicom+xml` (negotiated on `Accept`, per PS3.19).

**Supported search parameters by level.** Matching types: E = exact,
W = wildcard (`*`/`?`), R = range (dates), F = fuzzymatching (person-name
keys, see below), A = CSV any-component (multi-valued attributes).

**Study level:**

| Attribute | Tag | Matching |
|---|---|---|
| PatientName | (0010,0010) | W, F |
| PatientID | (0010,0020) | W |
| PatientBirthDate | (0010,0030) | E, R, W |
| PatientSex | (0010,0040) | E |
| StudyDate | (0008,0020) | E, R |
| StudyTime | (0008,0030) | E, W |
| StudyID | (0020,0010) | W |
| StudyDescription | (0008,1030) | W |
| AccessionNumber | (0008,0050) | W |
| Modality | (0008,0060) | A |
| ModalitiesInStudy | (0008,0061) | A |
| ReferringPhysicianName | (0008,0090) | W, F |
| PerformingPhysicianName | (0008,1050) | W, F |
| NameOfPhysiciansReadingStudy | (0008,1060) | W, F |
| StudyInstanceUID | (0020,000D) | E |
| ResponsiblePerson | (0010,2297) | W, F (extension, see 11.1) |
| InstitutionName | (0008,0080) | W (extension) |
| RequestingPhysician | (0032,1032) | W (extension) |
| RequestedProcedureDescription | (0032,1060) | W (extension) |

**Series level:**

| Attribute | Tag | Matching |
|---|---|---|
| SeriesInstanceUID | (0020,000E) | E |
| SeriesNumber | (0020,0011) | W |
| SeriesDate | (0008,0021) | E |
| SeriesTime | (0008,0031) | W |
| Modality | (0008,0060) | A |
| SeriesDescription | (0008,103E) | W |
| PerformedProcedureStepStartDate | (0040,0244) | E |
| PerformedProcedureStepStartTime | (0040,0245) | W |
| StationName | (0008,1010) | W (extension) |
| BodyPartExamined | (0018,0015) | W (extension) |
| Manufacturer | (0008,0070) | W (extension) |
| InstitutionalDepartmentName | (0008,1040) | W (extension) |
| OperatorsName | (0008,1070) | W (extension) |
| ProtocolName | (0018,1030) | W (extension) |

**Instance level:** SOPInstanceUID (0008,0018) E, SOPClassUID (0008,0016)
E, InstanceNumber (0020,0013) E.

**Patients resource:** PatientName, PatientID, PatientBirthDate (E/R/W),
PatientSex, ResponsiblePerson.

RequestAttributesSequence (0040,0275) is accepted as a matching key; no
scalar matching is defined for sequence values, the sequence remains
retrievable through WADO-RS metadata.

Cross-level keys on relational resources follow PS3.18 10.6.1.2.1:
patient keys are accepted on all-studies/all-series/all-instances
resources; series keys are accepted on study-scoped and global
series/instances resources and are accepted-and-ignored on instance
resources; keys that are foreign to a resource (e.g. a series key on
`/studies`) are rejected with `400`. Unknown query keys are **ignored and
logged** (interop parity with mainstream servers).

**Fuzzymatching (documented per PS3.18 8.3.4.2):** `fuzzymatching` is
optional; when absent it is `false` and literal matching is performed.
When `true`, person-name keys (PatientName, ReferringPhysicianName,
PerformingPhysicianName, NameOfPhysiciansReadingStudy, ResponsiblePerson)
are matched with Levenshtein-1 typo tolerance (one insertion, deletion or
substitution) by a bit-parallel Bitap matcher over stack-allocated masks
(zero heap allocation). Candidates are pre-filtered through an n-gram
index; the Bitap matcher is the final judge; multi-criterion queries
combine sorted candidate streams with a K-way merge. Cost is
O(candidates), not O(studies). Fuzzy matching applies only to patterns
without wildcards; a pattern containing `*` or `?` is matched literally.
Wildcard-free fuzzy comparison is case-insensitive; wildcard patterns
follow the `case_sensitive_pn` option.

**Search Response:** every row carries the mandatory response baseline of
its level (PS3.18 Tables 10.6.3-3/4/5): study rows carry StudyDate,
StudyTime, AccessionNumber, ModalitiesInStudy, ReferringPhysicianName,
PatientName, PatientID, PatientBirthDate, PatientSex, StudyInstanceUID,
StudyID, NumberOfStudyRelatedSeries, NumberOfStudyRelatedInstances;
series rows carry Modality, SeriesInstanceUID, SeriesNumber,
NumberOfSeriesRelatedInstances; instance rows carry SOPClassUID,
SOPInstanceUID, InstanceNumber (plus Study/Series UIDs for context). The
conditional attributes `RetrieveURL` (0008,1190) and
`InstanceAvailability` (0008,0056) are emitted at all three levels when
applicable. `SpecificCharacterSet` (0008,0005) = `"ISO_IR 192"` is
emitted as the first attribute of every study, series and patient row
(the repertoire actually used in responses); instance-level rows do not
carry it.

When `includefield` is **absent** (or equals `all`), the response carries
**all captured attributes** of each row, not only the baseline. With
`includefield`, requested attributes are added to the mandatory baseline
(add-not-replace); up to 64 requested tags are honored, the baseline is
never evicted.

**Search Response Codes:** `200` success; `400` for an invalid value of a
supported key (e.g. malformed date range). Unknown keys never fail the
request. Default `limit` 250; `limit`/`offset` paging is supported.

### 2.4 Rendered Resources (thumbnails)

- `/instances/{sop}/thumbnail` serves a **64x64 JPEG** rendered from the
  raw pixel path (percentile windowing, MONOCHROME1 inversion, signed
  samples, 64-megapixel decode cap) for the three uncompressed transfer
  syntaxes in any build.
- Thumbnails of compressed instances return `406 Not Acceptable` in every
  build. In the `transcode` build, full-size rendered JPEG
  (`Accept: image/jpeg` on the instance resource) is available; when the
  transcode pool is saturated the server answers `503` with a
  `Retry-After` header.

### 2.5 Delete (admin extension)

Instance/study deletion is exposed through the **administrative API**, not
the DICOMweb DELETE method (see 11.16). Deletion is journaled: the study is
tombstoned first, blobs are reclaimed by a background worker, and every
deletion is recorded in the append-only audit journal with the author,
reason and timestamp.

## 3. Storage Commitment Service (PS3.18 Section 13)

- **Resource:** `/commitment-requests/{TransactionUID}` - two
  transactions on one resource: Commit (POST, 13.4) and Check Commit
  Result (GET, 13.5), the Asynchronous Request-Reply pattern.
- **Commit:** answered synchronously with Done - `200` carrying
  per-instance results (13.4.2.1; a `200` may report every instance
  failed). No `202` Working is ever sent. The Transaction UID travels
  in the PATH, never in the payload (13.4.1.1); a duplicate Transaction
  UID is `409`, a duplicated SOP Instance within one request is `400`,
  and attributes outside the Commit payload are `400`.
- **The commitment is verifiable:** per-instance success means the SOP
  Instance UID is present in the study manifest - STOW answers `200`
  only after the manifests are durable, and the manifest's instance
  table embeds the CAS frame-chunk table, the "copy of the entire
  pixel data" PS3.4 J.1.1 requires. A missing instance is reported
  with Failure Reason `0112H` (No such SOP Instance) - the fact that
  was actually checked.
- **Check Commit Result:** GET on the same path, no query parameters
  (an extra parameter is `400`). Media types `application/dicom+json`
  (default) and `application/dicom+xml`, with the multipart/related
  forms answered as a single part. An answered transaction is retained
  `result_retention_days` days (default 30, `0` = forever); after the
  sweep Check answers `404`, which Table 13.5.3-1 allows for a deleted
  result - no `410` tombstone is kept.

## 4. Worklist Service (UPS-RS)
<!-- claim:ups_sections sections="11.4 11.5 11.6 11.7 11.8 11.9 11.10 11.11" not_supported="11.8" -->

- **Resources:** `/workitems` - search (GET and POST, 11.9), create
  (11.4), retrieve (11.5), update (11.6) and change state (PUT
  `/workitems/{uid}/state`, 11.7); the bare PUT on the workitem is the
  same state change; DELETE is a non-standard administration path.
  `/workitems/{uid}/events` - per-workitem SSE event stream
  (non-standard extension, see 11.9). Subscriptions (11.10/11.11):
  POST/DELETE
  `/workitems/{subscription-sop-class-uid}/subscribers/{subscriber}` -
  the 2026c subscribe-without-payload shape (`filter` query parameter,
  11.10.1.2/11.10.1.4), global (1.2.840.10008.5.1.4.34.5) and filtered
  (34.5.1) forms, and the `?deletionlock` query parameter (11.10). The
  WebSocket Notification Connection (8.10.4): `GET
  /dicomweb/subscribers/{requester}` with the RFC 6455 upgrade headers;
  Event Reports are JSON text frames (8.10.6).
- **Create and Update are told apart the way the standard does:**
  Create (11.4.1.1 Table 11.4.1-1) is `POST /workitems?workitem={uid}`
  - the Workitem UID is a QUERY parameter and there is no path segment
  after `workitems`. Update (11.6.1) is
  `POST /workitems/{uid}?TransactionUid={uid}` - the Workitem UID is a
  PATH segment and the Transaction UID is the query parameter. A path
  segment means the workitem resource, so a write to it is an update; no
  path segment means the worklist.
- **Update (11.6):** modifies attributes only; the workflow state is not
  touched and no event is published. The merge set is exactly the
  AIW-I 4.84.4.1.2.1 performer report - (0040,4033) Output Information
  Sequence, (0040,4019) Performed Workitem Code Sequence and
  (0040,4028) Performed Station Name Code Sequence - each replaced
  whole. An element outside that set is rejected (`400`) rather than
  ignored, so a client that meant something this implementation does not
  write finds out at once. The Transaction UID is required (query
  parameter) and checked (`400` on mismatch, `409` on a concurrent
  write); the request is refused while the workitem is unclaimed, since
  only the SCHEDULED -> IN PROGRESS transition issues the lock. An empty
  payload, and a present-but-empty (0040,4033), are both `400`.
<!-- claim:rad84 url="POST /workitems/{uid}?TransactionUid=" merge="0040,4033 0040,4019 0040,4028" no_event="no event is published" out_of_set="outside that set" -->
- **States:** SCHEDULED, IN PROGRESS, COMPLETED, CANCELED. State
  changes carry the Procedure Step State (0074,1000) and the locking
  Transaction UID (0008,1195) - both type 1 - and read nothing else
  (11.7.1.4).
- **Media types:** `application/dicom+json` (Default) and
  `application/dicom+xml`. The Required XML media type
  (`multipart/related; type="application/dicom+xml"`, PS3.18 11.1.3) is
  answered with the inner single part without the multipart wrapper
  (see 11.8).
- **Search keys:** Workitem UID (0008,0018), Study Instance UID
  (0040,4021 > 0020,000D), Accession Number (0040,A370 > 0008,0050),
  Procedure Step State
  (0074,1000), Procedure Step Label (0074,1204), Scheduled Station AE
  Title (0040,0001), Patient ID (0010,0020), and Modality (0008,0060)
  as a vendor extension (see below). An absent or empty key matches
  everything; a key with a value matches exactly (case-sensitive);
  Modality, being type CS, matches case-insensitively.
- **Sequence-qualified keys:** Study Instance UID and Accession Number are
  reached through a sequence in PS3.4 Table CC.2.5-3 (Input Information
  Sequence and Referenced Request Sequence respectively). This server stores
  one value per workitem, so the sequence segment is carried in the
  parameter NAME: `00404021.0020000D` and `0040A370.00080050`. The bare
  attribute tags `0020000D` and `00080050` are accepted as the same
  statement, because a caller sending either means the same search.
- **Modality (0008,0060) - vendor extension:** the UPS IOD defines no
  Modality (PS3.4 Table CC.2.5-3 has no such attribute), so this is an
  extension, not a UPS conformance claim. When a workitem is created by
  the origin server from a stored study, its render carries the input
  study's Modality recorded at STOW time, and the same value is a
  matching key. The attribute is honest data or it is absent: a workitem
  created externally through Create (11.4) has no study behind it, so it
  renders no (0008,0060) and no Modality query selects it. The extension
  exists because the DIMSE MWL service requires Modality as a Return key
  Type 1 (PS3.4 Table K.6-1) and the bridge maps workitems onto MWL.
- **Optimistic locking:** state changes and updates require the current
  Transaction UID; a mismatch returns a conflict (`409`).
- **Not supported:** the Request Cancellation resource (11.8) - a
  cancellation is performed by the workitem owner through Change
  Workitem State. The subscription registry is process-lifetime: a
  restart forgets subscriptions and their deletion locks (the durable
  path is UPS Search; the profile sets no persistence requirement).

## 5. Modality Scheduled Procedure Step Service (PS3.18 Section 14)

- **Resources:** `/modality-scheduled-procedure-steps` - Search (GET, 14.4),
  the C-FIND analog. The service is **search-only**: 14.3 states that
  "it is not possible to create Modality Scheduled Procedure Steps using
  DICOM, neither with DIMSE, nor with DICOMweb", so this implementation
  offers no Create, Update or Delete on the resource. The surface is off by
  default; a deployment enables it with `[workflow] enabled = true`, and
  while it is off the resource is absent (`404`).
- **Matching keys:** the seven REQUIRED matching keys of PS3.4 Table K.6-1 are
  supported - Scheduled Station AE Title (0040,0001), Scheduled Procedure Step
  Start Date (0040,0002), Scheduled Procedure Step Start Time (0040,0003),
  Modality (0008,0060), Scheduled Performing Physician's Name (0040,0006),
  Patient's Name (0010,0010) and Patient ID (0010,0020). Nine optional keys are
  also supported: Accession Number (0008,0050), Requested Procedure ID
  (0040,1001), Requested Procedure Description (0032,1060), Study Instance UID
  (0020,000D), Patient's Birth Date (0010,0030), Patient's Sex (0010,0040),
  Requesting Physician (0032,1032), Referring Physician's Name (0008,0090) and
  Scheduled Procedure Step ID (0040,0009). The Scheduled Procedure Step
  Sequence (0040,0100) is matched with **Sequence Matching** and holds a single
  Item as K.6-1 requires; its keys are accepted both bare and in their
  sequence-qualified spelling.
- **Fuzzy matching:** the `fuzzymatching` control parameter (PS3.18 Table
  8.3.4-1, 8.3.4.2) is supported for the three Person Name matching keys -
  Patient's Name (0010,0010), Scheduled Performing Physician's Name (0040,0006)
  and Referring Physician's Name (0008,0090). With `fuzzymatching=true` a name
  matches within one Levenshtein edit (the same Unicode-aware Bitap matcher
  QIDO-RS fuzzymatch uses), and the PN separators `^`/`=` align with a
  hand-typed space. The default (`false`) is exact single-value matching, and
  non-PN keys always match exactly.
- **Unsupported matching keys are rejected with `400`,** naming the key. An
  unknown or misspelled key is never silently dropped: dropping one widens the
  result set to the whole worklist, which a caller cannot distinguish from a
  genuinely broad match.
- **Response:** the reply is the SPS-level list of matching scheduled procedure
  steps (14.4: "a list of matching scheduled procedure steps"), not a
  Study/Series/Instance hierarchy. Search control parameters `limit` and
  `offset` of PS3.18 Table 8.3.4-1 are honoured; a client `limit` above
  `[workflow] mwl_max_responses` is clamped to that hard ceiling.
- **Media types:** `application/dicom+json` (Default), `application/dicom+xml`
  (Required), `multipart/related; type="application/dicom+json"` (Required) and
  `multipart/related; type="application/dicom+xml"` (Required), all per 14.1.3.
- **Record store:** MWL rows live in `[workflow] dir` (default
  `{btree.root}/workflow`), under `mwl/`. DICOMweb defines no MWL Create, so
  this server never writes a row through the DICOMweb surface; the store is
  read by the search and written by a scheduler forwarder that is outside this
  document's scope.

## 6. Modality Performed Procedure Step Service (PS3.18 Section 15)

- **Resources:** `/modality-performed-procedure-steps/{mppsUID}` - Create
  (POST, 15.4, N-CREATE), Update (POST with the **mandatory** `?update` query
  parameter, 15.5, N-SET - the parameter is what distinguishes Update from
  Create, the two otherwise sharing one URI) and Retrieve (GET, 15.6, N-GET).
  There is **no list/search transaction**: 15.3 states that "it is not possible
  to list all Modality Performed Procedure Steps using DICOM, neither with
  DIMSE, nor with DICOMweb". The surface is off by default
  (`[workflow] enabled = true`); while off the resource is absent (`404`).
- **MPPS Retrieve is implemented.** 15.6 marks it OPTIONAL for the origin
  server; this implementation provides it.
- **Create:** the Performed Procedure Step Status (0040,0252) is Type 1 at
  N-CREATE and the request is accepted **only** when its value is
  `IN PROGRESS` (PS3.4 F.7.2.1). A missing status, or any other value, is a
  `400`. A Create on an MPPS UID that already exists is a `409` carrying the
  standard's Duplicate SOP Instance code `0111H`.
- **Update:** the state machine is `IN PROGRESS -> COMPLETED | DISCONTINUED`.
  An N-SET after a final status is refused, and an N-SET naming an unknown MPPS
  UID is refused (`404`) - there is no implicit create, because DICOMweb is not
  the DIMSE vendor-tolerance path. The Performed Series Sequence (0040,0340)
  must **not be empty** when the status becomes `COMPLETED` or `DISCONTINUED`
  (PS3.4 F.7: "a Series must exist for every Performed Procedure Step").
- **Discontinuation reason:** (0040,0281) is OPTIONAL (type 3/3 in Table
  F.7.2-1) and is **not** Type 1C. When a performer wants to use it, it must be
  created zero-length at N-CREATE, because a top-level attribute, once set,
  cannot be deleted later (PS3.4 F.7.2.1.1 note 5). The store keeps an empty
  reason distinct from an absent one, so that rule is representable.
- **Study link:** the Study Instance UID lives inside the Scheduled Step
  Attributes Sequence, (0040,0270) > (0020,000D), not as a top-level attribute;
  the render and the reader both use that carriage.
- **Notifications are not supported** (PS3.18 15.1: "Notifications, as defined
  in Section F.9 in PS3.4, are not supported by this service"). For
  notification-like behaviour the standard points to the IHE Scheduled Workflow
  Performed Procedure Step Manager forwarding creates and updates to interested
  actors; this server does not act as that Manager.
- **Concurrency is last-writer-wins.** MPPS defines no Transaction UID (unlike
  UPS 0008,1195), so there is no optimistic locking: one MPPS UID is written by
  one modality, and the most recent write is the record. Documenting this is
  the whole treatment - no lock is invented.
- **Media types:** `application/dicom+json` (Default) and
  `application/dicom+xml` (Required), per 15.1.3. **There is no
  `multipart/related` media type for this service**, unlike MWL.
- **Record store:** MPPS rows live in `[workflow] dir`, under `mpps/`. A
  retention sweep removes FINAL rows older than `[workflow] retention_days`; an
  `IN PROGRESS` row is never swept.

## 7. WADO-URI

**Resource:** `/wadouri` - single-instance retrieval by UID query
parameters. Conformance claim is limited to **Retrieve DICOM Instance,
`application/dicom` media type**. Supported query parameters: the
mandatory set - `requestType` (=WADO), `studyUID`, `seriesUID`,
`objectUID`. Optional parameters of PS3.18 9.1.2.2 (rendering, `charset`,
`anonymize`, transfer syntax) are **not supported**; a request for
`requestType != WADO` is rejected with `400`.

## 8. Capabilities

**Resource:** `/dicomweb/` - returns the service capabilities document
(`application/dicom+json`). Advertised keys: `qido-rs`, `wado-rs`,
`stow-rs`, `ups-rs`, `wado-uri`, `commitment-rs`,
`transferCapabilities`.

## 9. Transfer Syntaxes

- **Ingest:** store-as-received; instances are accepted and stored in any
  transfer syntax present in the STOW payload, with no re-encoding.
- **Retrieve:** served in the stored transfer syntax; a requested
  `?transferSyntax=` requires the `transcode` build feature.

### 7.1 Transcode support (`transcode` build)

**Decoders** (stored transfer syntax to pixels): Implicit VR Little
Endian, Explicit VR Little Endian, Explicit VR Big Endian, RLE Lossless,
JPEG Baseline (4.50), JPEG Lossless P14 (4.57 / 4.70), JPEG-LS
Lossless/Near-Lossless (4.80 / 4.81), JPEG 2000 Lossless/Lossy (4.90 /
4.91).

**Encoders** (pixels to requested transfer syntax): Explicit VR Little
Endian, JPEG Baseline, JPEG Lossless, JPEG-LS Lossless/Near-Lossless,
JPEG 2000 Lossless/Lossy.

**JPEG 2000 component policy:** 1 or 3 components matching Samples per
Pixel, 8/16-bit: fully supported. A codestream whose component count
differs from Samples per Pixel has no legal DICOM representation
(PS3.3 C.7.6.3.1.1/C.7.6.3.1.2), so the transcode is refused with `406`;
the instance remains retrievable in its stored transfer syntax. Signed
codestream samples are preserved bit-exactly; the output carries the
`PixelRepresentation` (0028,0103) of the stored instance.

**Not supported:** High-Throughput JPEG 2000 (4.201-4.203), JPIP, JPX,
JPEG XL, MPEG/HEVC, Deflated Explicit VR LE. Such instances are stored
and served as-is; a transcode request for them returns `406`.

**Failure behavior:** a transient codec error falls back to the original
bytes with a logged WARN; a non-conformant codestream (component count
mismatch) is a deterministic refusal with `406`.

## 10. Character Sets

- The DICOM default character repertoire and Latin-1 pass through
  natively; UTF-8 (ISO_IR 192) is handled in JSON/XML responses.
- QIDO-RS responses always emit `SpecificCharacterSet` (0008,0005) =
  `"ISO_IR 192"` at study, series and patient levels (see 2.3).
- CJK character sets (ISO 2022 IR 13/87 - Japanese, KS X 1001 - Korean,
  GB18030 - Chinese) are decoded by the DICOM parser only when the binary
  is built with the `cjk-charsets` feature; without it CJK text is not
  decoded. The shipped default build includes the feature.

## 11. Security

- Authentication and TLS termination are **delegated** to the reverse
  proxy (nginx). The server trusts the proxy identity headers
  (`X-Authenticated-User`, `X-Real-IP` / `X-Forwarded-For`) only from
  configured `trusted_proxies`; spoofed headers from untrusted peers are
  ignored. Optional strict mode `require_proxy_identity = true` rejects
  authenticated-proxy requests without an identity with `502`.
- An optional `X-Clarus-Role` header controls auditor masking in
  administrative responses.
- Logs mask patient names (first/last letter pattern); audit trails are
  retained in the append-only journal.
- Encryption at rest is the OS-level mount encryption (BitLocker / LUKS /
  EFS); the server binary contains no symmetric encryption of its own.

## 12. Configuration

- Server: nginx-style config file (`clarus.conf`), validated by
  `clarus --test-config`.
- Communication: HTTP/1.1 over TCP, keep-alive, thread-per-connection;
  TLS is terminated by the reverse proxy (see 9).
- Storage Commitment retention: `[commitment] result_retention_days`
  (default 30, `0` = forever).
- Compile-time features: `transcode` (JPEG 2000 + JPEG-LS on-the-fly),
  `s3` (cold-tier fallback), `cjk-charsets` (CJK decoding);
  `china_crypto` (planned: SM3 content-addressing instead of BLAKE3).

## 13. Extensions and Deviations

Vendor extensions beyond the mandatory key set of PS3.18 Table 10.6.1-5
are legal per PS3.18 8.3.4.1 and are declared here.

**11.1 `ResponsiblePerson` (0010,2297)** - additional matching key at the
study and patient levels (veterinary workflows). Person-name semantics:
exact and wildcard matching, `fuzzymatching=true` applies (Levenshtein-1),
case sensitivity follows `case_sensitive_pn`. Cyrillic values are matched
as decoded per the stored `SpecificCharacterSet`.

**11.2 Series-level search keys `StationName` (0008,1010) and
`BodyPartExamined` (0018,0015)** - wildcard matching keys with
`SeriesDescription` semantics (dcm4chee-arc / Orthanc parity). On
instance resources they are accepted-and-ignored per 10.6.1.2.1; on the
study resource they are foreign and rejected with `400`. No fuzzymatching
and no n-gram indexing is wired for these keys.

**11.3 `PatientBirthDate` range** - in addition to the exact `YYYYMMDD`
form, the key accepts `YYYYMMDD-YYYYMMDD` and the wildcard `*` at the
study level and on the `/dicomweb/patients` resource. Matching is an
inclusive numeric range over the stored date.

**11.4 Seven captured search attributes** - study level:
`InstitutionName` (0008,0080), `RequestingPhysician` (0032,1032),
`RequestedProcedureDescription` (0032,1060); series level:
`Manufacturer` (0008,0070), `InstitutionalDepartmentName` (0008,1040),
`OperatorsName` (0008,1070), `ProtocolName` (0018,1030). All are captured
at STOW and re-captured on rebuild; all are wildcard matching keys. The
PN keys (`RequestingPhysician`, `OperatorsName`) follow the person-name
template: the `case_sensitive_pn` option applies exactly as for
`ReferringPhysicianName` / `PerformingPhysicianName`; `fuzzymatching` is
not wired for these keys.

**11.5 `fuzzymatch_override`** - a server option that forces
Levenshtein-1 matching on person-name keys even when the client does not
send `fuzzymatching=true`. Non-standard; **enabled in the shipped
server config** (real clients such as Weasis never send
`fuzzymatching=true`). Set `fuzzymatch_override = false` for strict
standard behavior.

**11.6 `case_sensitive_pn = false` in the shipped config** - person-name
wildcard matching is case-insensitive (Orthanc-style convenience). Set
`true` for strict PS3.4 behavior. Wildcard-free fuzzy matching is
case-insensitive by construction.

**11.7 Series-root instances query `GET /dicomweb/series/{series}/instances`**
- non-standard resource: PS3.18 defines series resources only scoped under
a study. Weasis appends `/instances` to a series-level `RetrieveURL`; the
resource resolves the owning study via a manifest scan and answers the
standard study-scoped query. Standard clients keep using the study-scoped
path.

**11.8 UPS-RS XML media type** - the Required XML media type
(`multipart/related; type="application/dicom+xml"`, PS3.18 11.1.3) is
answered as a single `application/dicom+xml` part without the multipart
wrapper; `Accept` negotiation is a substring match (`q=` factors are not
parsed).

**11.9 UPS-RS SSE event stream** - `GET /workitems/{uid}/events` is a
non-standard per-workitem SSE stream (`event: StateChange` frames,
keep-alive); the conformant notification path is the WebSocket
Notification Connection (8.10.4) and the durable path is UPS Search.

**11.10 WADO-RS / WADO-URI rendered-resource optional query parameters**
- not supported (viewport, windowing, annotation, quality, `charset`,
`anonymize`).

**11.11 Series metadata without an explicit multipart Accept** - a
single-part `application/dicom+json` array is returned instead of the
Required `multipart/related` (PS3.18 10.4.4). OHIF's dicomweb-client
requests this resource with `responseType=json` and fails on any
multipart body; the single-array answer is the deployed practice of
dcm4chee/Google Cloud Healthcare and the behavior field-verified against
OHIF/Weasis. The Required multipart media type remains fully supported
for clients that ask for it.

**11.12 Veterinary attributes** - Species (0010,2201), Breed
(0010,2292), Neutered (0010,2203) are captured at STOW and re-captured on
rebuild but are not searchable QIDO keys and are not returned in QIDO
responses; they remain retrievable through WADO-RS metadata.

**11.13 Compressed-instance thumbnails** - `406` in every build; the raw
64x64 thumbnail path decodes uncompressed pixel samples only. Compressed
data is served in full size via `Accept: image/jpeg` (transcode build).

**11.14 HTJ2K and exotic transfer syntaxes** - High-Throughput JPEG 2000
(4.201-4.203) and other non-listed syntaxes are stored and served as-is;
transcode requests for them return `406`.

**11.15 STOW exact duplicates** - deliberately `200` (idempotent), see 2.1.

**11.16 Admin API, delete/erase and audit** - the administrative HTTP
API (recovery/rebuild, erase, metrics, status), the append-only audit
journal and the access journal are non-DICOMweb extensions and are not
covered by this statement beyond 2.5.

**11.17 SpecificCharacterSet always-on emission** - `(0008,0005) =
"ISO_IR 192"` is emitted on every study/series/patient QIDO row although
the attribute is not part of the PS3.18 2026c response tables. The value
names the repertoire actually used in the response (UTF-8).

## 14. Trademarks

DICOM is the registered trademark of the National Electrical
Manufacturers Association (NEMA) for its standards publications relating
to digital communications of medical information. DICOMweb is a trademark
of NEMA. Clarus is not affiliated with or endorsed by NEMA.
