# DICOM Conformance Statement - Clarus

**Product:** Clarus DICOMweb origin server (`clarus` binary)
**Version:** 0.3.0-alpha
**Date:** 2026-09-08
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

- **Roles:** origin server for QIDO-RS, WADO-RS, STOW-RS, UPS-RS and WADO-URI.
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
| `/studies/{study}/series/{series}/metadata` | `multipart/related; type="application/dicom+json"` when Accept asks multipart (Required media type, PS3.18 10.4.4); a single-part `application/dicom+json` array otherwise (see 10.11) |
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
| ResponsiblePerson | (0010,2297) | W, F (extension, see 10.1) |
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
the DICOMweb DELETE method (see 10.4). Deletion is journaled: the study is
tombstoned first, blobs are reclaimed by a background worker, and every
deletion is recorded in the append-only audit journal with the author,
reason and timestamp.

## 3. Worklist Service (UPS-RS)

- **Resources:** `/workitems` - search (GET and POST), create, get,
  update, delete; `/workitems/{uid}/events` - SSE event subscription.
  State changes (SCHEDULED to IN PROGRESS to COMPLETED/CANCELED/SUSPENDED)
  are driven through Update Workitem (PS3.18 11.6).
- **Media types:** `application/dicom+json` (Default) and
  `application/dicom+xml`. The Required XML media type
  (`multipart/related; type="application/dicom+xml"`, PS3.18 11.1.3) is
  answered with the inner single part without the multipart wrapper
  (see 10.9).
- **Search keys:** Workitem UID (0020,000D), Modality (0008,0060),
  Scheduled Station AE Title (0040,0001), Patient ID (0010,0020).
- **Optimistic locking:** updates require the current Transaction UID; a
  mismatch returns a conflict (`409`).
- **Not supported:** the Subscription resource (11.10), the dedicated
  Change Workitem State resource (11.7) and the Request Cancellation
  resource (11.8). State changes are performed through Update Workitem.

## 4. WADO-URI

**Resource:** `/wadouri` - single-instance retrieval by UID query
parameters. Conformance claim is limited to **Retrieve DICOM Instance,
`application/dicom` media type**. Supported query parameters: the
mandatory set - `requestType` (=WADO), `studyUID`, `seriesUID`,
`objectUID`. Optional parameters of PS3.18 9.1.2.2 (rendering, `charset`,
`anonymize`, transfer syntax) are **not supported**; a request for
`requestType != WADO` is rejected with `400`.

## 5. Capabilities

**Resource:** `/dicomweb/` - returns the service capabilities document
(`application/dicom+json`).

## 6. Transfer Syntaxes

- **Ingest:** store-as-received; instances are accepted and stored in any
  transfer syntax present in the STOW payload, with no re-encoding.
- **Retrieve:** served in the stored transfer syntax; a requested
  `?transferSyntax=` requires the `transcode` build feature.

### 6.1 Transcode support (`transcode` build)

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

## 7. Character Sets

- The DICOM default character repertoire and Latin-1 pass through
  natively; UTF-8 (ISO_IR 192) is handled in JSON/XML responses.
- QIDO-RS responses always emit `SpecificCharacterSet` (0008,0005) =
  `"ISO_IR 192"` at study, series and patient levels (see 2.3).
- CJK character sets (ISO 2022 IR 13/87 - Japanese, KS X 1001 - Korean,
  GB18030 - Chinese) are decoded by the DICOM parser only when the binary
  is built with the `cjk-charsets` feature; without it CJK text is not
  decoded. The shipped default build includes the feature.

## 8. Security

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

## 9. Configuration

- Server: nginx-style config file (`clarus.conf`), validated by
  `clarus --test-config`.
- Communication: HTTP/1.1 over TCP, keep-alive, thread-per-connection;
  TLS is terminated by the reverse proxy (see 8).
- Compile-time features: `transcode` (JPEG 2000 + JPEG-LS on-the-fly),
  `s3` (cold-tier fallback), `cjk-charsets` (CJK decoding);
  `china_crypto` (planned: SM3 content-addressing instead of BLAKE3).

## 10. Extensions and Deviations

Vendor extensions beyond the mandatory key set of PS3.18 Table 10.6.1-5
are legal per PS3.18 8.3.4.1 and are declared here.

**10.1 `ResponsiblePerson` (0010,2297)** - additional matching key at the
study and patient levels (veterinary workflows). Person-name semantics:
exact and wildcard matching, `fuzzymatching=true` applies (Levenshtein-1),
case sensitivity follows `case_sensitive_pn`. Cyrillic values are matched
as decoded per the stored `SpecificCharacterSet`.

**10.2 Series-level search keys `StationName` (0008,1010) and
`BodyPartExamined` (0018,0015)** - wildcard matching keys with
`SeriesDescription` semantics (dcm4chee-arc / Orthanc parity). On
instance resources they are accepted-and-ignored per 10.6.1.2.1; on the
study resource they are foreign and rejected with `400`. No fuzzymatching
and no n-gram indexing is wired for these keys.

**10.3 `PatientBirthDate` range** - in addition to the exact `YYYYMMDD`
form, the key accepts `YYYYMMDD-YYYYMMDD` and the wildcard `*` at the
study level and on the `/dicomweb/patients` resource. Matching is an
inclusive numeric range over the stored date.

**10.4 Seven captured search attributes** - study level:
`InstitutionName` (0008,0080), `RequestingPhysician` (0032,1032),
`RequestedProcedureDescription` (0032,1060); series level:
`Manufacturer` (0008,0070), `InstitutionalDepartmentName` (0008,1040),
`OperatorsName` (0008,1070), `ProtocolName` (0018,1030). All are captured
at STOW and re-captured on rebuild; all are wildcard matching keys. The
PN keys (`RequestingPhysician`, `OperatorsName`) follow the person-name
template: the `case_sensitive_pn` option applies exactly as for
`ReferringPhysicianName` / `PerformingPhysicianName`; `fuzzymatching` is
not wired for these keys.

**10.5 `fuzzymatch_override`** - a server option that forces
Levenshtein-1 matching on person-name keys even when the client does not
send `fuzzymatching=true`. Non-standard; **enabled in the shipped
server config** (real clients such as Weasis never send
`fuzzymatching=true`). Set `fuzzymatch_override = false` for strict
standard behavior.

**10.6 `case_sensitive_pn = false` in the shipped config** - person-name
wildcard matching is case-insensitive (Orthanc-style convenience). Set
`true` for strict PS3.4 behavior. Wildcard-free fuzzy matching is
case-insensitive by construction.

**10.7 Series-root instances query `GET /dicomweb/series/{series}/instances`**
- non-standard resource: PS3.18 defines series resources only scoped under
a study. Weasis appends `/instances` to a series-level `RetrieveURL`; the
resource resolves the owning study via a manifest scan and answers the
standard study-scoped query. Standard clients keep using the study-scoped
path.

**10.8 UPS-RS XML media type** - the Required XML media type
(`multipart/related; type="application/dicom+xml"`, PS3.18 11.1.3) is
answered as a single `application/dicom+xml` part without the multipart
wrapper; `Accept` negotiation is a substring match (`q=` factors are not
parsed).

**10.9 UPS-RS Subscription (11.10), Change Workitem State (11.7) and
Request Cancellation (11.8)** - not supported; state changes go through
Update Workitem; the embedded viewer uses a private in-process SSE fan-out.

**10.10 WADO-RS / WADO-URI rendered-resource optional query parameters**
- not supported (viewport, windowing, annotation, quality, `charset`,
`anonymize`).

**10.11 Series metadata without an explicit multipart Accept** - a
single-part `application/dicom+json` array is returned instead of the
Required `multipart/related` (PS3.18 10.4.4). OHIF's dicomweb-client
requests this resource with `responseType=json` and fails on any
multipart body; the single-array answer is the deployed practice of
dcm4chee/Google Cloud Healthcare and the behavior field-verified against
OHIF/Weasis. The Required multipart media type remains fully supported
for clients that ask for it.

**10.12 Veterinary attributes** - Species (0010,2201), Breed
(0010,2292), Neutered (0010,2203) are captured at STOW and re-captured on
rebuild but are not searchable QIDO keys and are not returned in QIDO
responses; they remain retrievable through WADO-RS metadata.

**10.13 Compressed-instance thumbnails** - `406` in every build; the raw
64x64 thumbnail path decodes uncompressed pixel samples only. Compressed
data is served in full size via `Accept: image/jpeg` (transcode build).

**10.14 HTJ2K and exotic transfer syntaxes** - High-Throughput JPEG 2000
(4.201-4.203) and other non-listed syntaxes are stored and served as-is;
transcode requests for them return `406`.

**10.15 STOW exact duplicates** - deliberately `200` (idempotent), see 2.1.

**10.16 Admin API, delete/erase and audit** - the administrative HTTP
API (recovery/rebuild, erase, metrics, status), the append-only audit
journal and the access journal are non-DICOMweb extensions and are not
covered by this statement beyond 2.5.

**10.17 SpecificCharacterSet always-on emission** - `(0008,0005) =
"ISO_IR 192"` is emitted on every study/series/patient QIDO row although
the attribute is not part of the PS3.18 2026c response tables. The value
names the repertoire actually used in the response (UTF-8).

## 11. Trademarks

DICOM is the registered trademark of the National Electrical
Manufacturers Association (NEMA) for its standards publications relating
to digital communications of medical information. DICOMweb is a trademark
of NEMA. Clarus is not affiliated with or endorsed by NEMA.
