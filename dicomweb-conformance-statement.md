# DICOMweb™ Conformance Statement — Clarus

**Product:** Clarus (origin server) · **Version:** 0.3.0-alpha · **Date:** 2026-08-27
**Standard:** DICOM® PS3.18 (Web Services), 2026c edition · **Related:** DICOM PS3.19 (XML — QIDO responses)

## Why this document exists

DICOM PS3.18 Section 6 (Conformance) requires that an implementation
claiming conformance to this Part of the Standard function per all its
mandatory sections and publish a Conformance Statement. The structure of
Conformance Statements is specified in PS3.2. Per-service clauses
(PS3.18 §10.4.5, §10.5.5, §10.6.5) additionally require declaring the
supported roles, resources, media types, optional query parameters and
search behaviors — including fuzzy matching, which PS3.18 §8.3.4.2
requires to be documented in the Conformance Statement and in the
Retrieve Capabilities response.

---

## 1. Introduction

Clarus is a high-performance DICOMweb **origin server** for medical imaging
archives. This statement describes the DICOMweb conformance of the Clarus
server binary (`clarus`), compiled from the Rust workspace at the stated
version.

## 2. Implementation Model

- **Roles:** origin server for QIDO-RS, WADO-RS, STOW-RS, UPS-RS, WADO-URI.
- **Real-world activities:** receives instances via STOW-RS (directly or
  through the Clarus DIMSE bridge, whose conformance statement is
  [dimse-bridge-conformance-statement.md](./dimse-bridge-conformance-statement.md)),
  stores them in a content-addressed archive (filesystem as database:
  per-study CBOR manifests + B-tree indexes), and serves queries and
  retrievals to DICOMweb user agents (viewers, workstations).
- **Concurrency model:** thread-per-connection, no async runtime; scheduling
  is delegated to the OS kernel.
- **Storage:** no embedded SQL database; manifests and indexes are read via
  memory-mapped files (zero-copy).

## 3. DICOMweb Services

### 3.1 QIDO-RS (Search)

**Resources (all media types supported):**

| Resource | Supported |
|---|---|
| `/studies` | yes |
| `/series` | yes (global and study-scoped) |
| `/instances` | yes (global, study- and series-scoped) |

**Media types:** `application/dicom+json` (default) and
`application/dicom+xml` (negotiated on `Accept`, per PS3.19).

**Query parameters:** `PatientName`, `PatientID`, `AccessionNumber`,
`StudyDescription`, `ReferringPhysicianName`, `PerformingPhysicianName`,
`NameOfPhysiciansReadingStudy`, `StudyInstanceUID`, `StudyID`, `StudyTime`,
`PatientBirthDate`, `PatientSex`, `ModalitiesInStudy`, `SeriesInstanceUID`,
`SeriesNumber`, `Modality`, `StudyDate` (range), plus `limit`, `offset`,
`includefield`, `fuzzymatching` (person-name keys, Levenshtein-1),
`case_sensitive_pn`.

**Behavior:** unknown query keys are rejected with `400`.

**Fuzzy matching (documented per the requirement of PS3.18 §8.3.4.2):** the
`fuzzymatching` parameter is optional; when absent it is `false` and literal
matching is performed. When `true` (or when the `fuzzymatch_override`
extension is active, see §6), person-name keys (`PatientName`,
`ReferringPhysicianName`, `PerformingPhysicianName`,
`NameOfPhysiciansReadingStudy`) are matched with **Levenshtein-1 typo
tolerance** — a single insertion, deletion, or substitution — by a
**bit-parallel Bitap (Shift-Or) matcher over stack-allocated masks (zero
heap allocation)**. The matcher is Unicode-safe and case-insensitive.

The match does not scan every manifest:

1. an **n-gram candidate index** yields a candidate superset;
2. the **Bitap matcher is the final judge** for each candidate;
3. multi-criterion queries combine sorted candidate streams with a
   **K-way merge**.

Cost is O(candidates), not O(number of studies). Fuzzy matching applies only
to patterns without wildcards; a pattern containing `*` or `?` is matched
literally per standard wildcard semantics. Wildcard-free fuzzy comparison is
case-insensitive; wildcard patterns follow `case_sensitive_pn`.

### 3.2 WADO-RS (Retrieve)

| Resource | Media type returned |
|---|---|
| `/studies/{study}` | `multipart/related; type="application/dicom"` |
| `/series/{series}` | `multipart/related; type="application/dicom"` |
| `/instances/{sop}` | `application/dicom` (supports `Range` / partial content) |
| `/instances/{sop}/metadata` | `application/dicom+json` (default); `application/dicom+xml` on negotiation (Required media type, PS3.18 §10.4.4) |
| `/instances/{sop}/frames/{n}` | `multipart/related; type="application/octet-stream"` |
| `/instances/{sop}/thumbnail` | `image/jpeg` (requires `transcode` build feature) |
| bulkdata URIs (`/bulkdata/{tag}`) | `application/octet-stream` |

**Transfer syntax negotiation:** `?transferSyntax=` query parameter per
PS3.18 §10.4.2. Instances are stored in their received transfer syntax and
served as stored by default; on-the-fly transcoding (JPEG 2000, JPEG-LS) is
a **compile-time feature** `transcode` (`--features transcode`). When the
requested transfer syntax cannot be produced, the server returns `406`.

**Other optional query parameters** (viewport, windowing, annotation,
quality, `charset`, `anonymize`): **not supported**.

**Composite SOP classes:** no fixed allow-list — any well-formed Composite
SOP Instance (Image Storage SOP Classes) is stored and served in its
received transfer syntax. Rendered Presentation States: not supported.

**Thumbnails:** supported with the `transcode` build feature; the thumbnail
is a JPEG rendered from the requested instance's pixel data by the in-tree
codec chain, with dimensions derived from the source instance (no
user-selected sizes).

### 3.3 STOW-RS (Store)

- **Resource:** `/studies`; request body
  `multipart/related; type="application/dicom"`.
- **Response:** `application/dicom+json`, with the per-instance
  `ReferencedSOPSequence`.
- **Status codes:** `200` — all instances stored (including exact-duplicate
  idempotent re-stores); `202` — stored with warnings (e.g. coerced Type 2
  elements, reason `(0008,1196) = 45056`) or partial failures; `409` —
  conflict (SOP-level); `400` — nothing stored. Exact duplicates are **not**
  conflicts: per PS3.18 §10.5.3.1 a conflict is e.g. an unsupported SOP Class
  or Study Instance UID mismatch, and §10.5.2 allows adding instances to
  existing Studies; duplicate re-STOW is therefore a successful `200`.
- **UID grammar gate:** instances whose Study/Series/SOP Instance UIDs
  violate the DICOM UID grammar are refused per instance BEFORE any CAS
  write — a stored instance must stay addressable by WADO/QIDO forever.
- **Progress visibility:** `STOW_STUDY` progress lines are emitted every 50
  slices; the final access-log line of a STOW request is deferred to the
  next request (deliberate; see the `[logging]` notes in the server config).

### 3.4 UPS-RS (Unified Worklist)

- **Resources:** `/workitems` — search (GET and POST), create, get, update,
  delete; `/workitems/{uid}/events` — SSE event subscription. State changes
  (SCHEDULED → IN PROGRESS → COMPLETED/CANCELED/SUSPENDED) are driven through
  Update Workitem (PS3.18 §11.6); the non-standard `/claim` resource is no
  longer exposed.
- **Media type:** `application/dicom+json` (Default). The Required XML media
  type (`multipart/related; type="application/dicom+xml"`, PS3.18 §11.1.3)
  is **not supported** — see §6.
- **Search keys:** Workitem UID (`0020000D`), Modality (`00080060`),
  Scheduled Station AE Title (`00400001`), Patient ID (`00100020`).
- **Optimistic locking:** updates require the current Transaction UID; a
  mismatch returns a conflict.
- **Subscription** (PS3.18 §11.10): **not supported** — see §6. The dedicated
  Change Workitem State (`/state`, §11.7) and Request Cancellation
  (`/cancelrequest`, §11.8) resources are **not exposed**; state changes are
  performed through the Update Workitem transaction.

### 3.5 WADO-URI

- **Resource:** `/wadouri` — single-instance retrieval by UID query
  parameters.
- **Supported query parameters:** the mandatory set — `requestType`
  (=WADO), `studyUID`, `seriesUID`, `objectUID`. Optional parameters of
  PS3.18 §9.1.2.2 (rendering, `charset`, `anonymize`, transfer syntax) are
  **not supported**; a request for `requestType != WADO` is rejected with
  `400`.

### 3.6 Capabilities

- **Resource:** `/dicomweb/` — returns the service capabilities document
  (`application/dicom+json`).

## 4. Transfer Syntaxes

- **Ingest:** instances are accepted and stored in any transfer syntax
  present in the STOW payload (no re-encoding on store). Ingest is
  store-as-received: no transfer syntax is rejected. (Instances whose UIDs
  violate the DICOM UID grammar are refused per instance before any CAS
  write — see §3.3.)
- **Retrieve:** instances are served in the stored transfer syntax; a
  requested transfer syntax via `?transferSyntax=` requires the `transcode`
  build feature.

### 4.1 Transcode support (`transcode` build)

**Decoders** (stored transfer syntax → pixels): Implicit VR Little Endian,
Explicit VR Little Endian, Explicit VR Big Endian, RLE Lossless, JPEG
Baseline (…4.50), JPEG Lossless P14 (…4.57 / …4.70), JPEG-LS Lossless /
Near-Lossless (…4.80 / …4.81), JPEG 2000 Lossless / Lossy (…4.90 / …4.91).

**Encoders** (pixels → requested transfer syntax): Explicit VR Little Endian,
JPEG Baseline, JPEG Lossless, JPEG-LS Lossless / Near-Lossless, JPEG 2000
Lossless / Lossy.

**JPEG 2000 component policy:**
- 1 or 3 components, 8/16-bit: fully supported.
- N-component (≠ 1,3) codestreams (real-world: 5-component CBCT): MONOCHROME
  images are transcoded from component 0; color images with N ≠ 3 components
  are refused with `406`.
- Signed codestream samples are preserved bit-exactly; the output instance
  carries the `PixelRepresentation` (0028,0103) of the stored instance.

**Not supported:** High-Throughput JPEG 2000 (…4.201–…4.203), JPIP, JPX,
JPEG XL, MPEG/HEVC, Deflated Explicit VR LE. Such instances are stored and
served as-is; a transcode request for them returns `406`.

**Failure behavior:** when a supported target cannot be produced for a
specific instance (codec error), the server falls back to the original bytes
and logs a WARN that includes the codec error.

- **Compile-time features:** `transcode` (JPEG 2000 + JPEG-LS on-the-fly),
  `s3` (cold-tier fallback), `cjk-charsets` (CJK decoding),
  `china_crypto` (planned: SM3 content-addressing instead of BLAKE3).

## 5. Security

- Authentication and TLS termination are **delegated** to the reverse proxy
  (nginx). The server trusts the proxy's identity headers
  (`X-Authenticated-User`, `X-Real-IP` / `X-Forwarded-For`) only from
  configured `trusted_proxies`; spoofed headers from untrusted peers are
  ignored. Optional strict mode `require_proxy_identity = true` rejects
  authenticated-proxy requests without an identity with `502`.
- An optional `X-Clarus-Role` header controls auditor masking in
  administrative responses.
- Logs mask patient names (first/last letter pattern); audit trails are
  retained in the journal.
- Encryption at rest is the OS-level mount encryption (BitLocker / LUKS /
  EFS); the server binary contains no symmetric encryption of its own.

## 6. Known Deviations and Extensions

| # | Item | Status |
|---|---|---|
| 1 | UPS-RS Subscription resource (PS3.18 §11.10, WebSocket-based notifications per §8.10) | not supported; the embedded viewer uses a private in-process SSE fan-out, which is a different mechanism |
| 2 | `fuzzymatch_override` (forced Levenshtein-1 on name keys) | non-standard extension, **enabled in the shipped server config** (conscious deviation: real clients such as Weasis never send `fuzzymatching=true`); set `fuzzymatch_override = false` to restore strict standard behavior |
| 3 | STOW exact duplicates | deliberately `200` (idempotent), see §3.3 |
| 4 | Access-log final STOW line deferred to next request | documented log behavior, not a protocol deviation |
| 5 | UPS-RS XML media type (`multipart/related; type="application/dicom+xml"`, Required, PS3.18 §11.1.3) | not supported — JSON only |
| 6 | Workitem State resource (§11.7) and Request Cancellation resource (§11.8) | not exposed — state changes go through Update Workitem |
| 7 | WADO-RS / WADO-URI rendered-resource and optional query parameters | not supported (viewport, windowing, annotation, quality, `charset`, `anonymize`) |
| 8 | JPEG 2000 with N ≠ 1,3 components | MONOCHROME images are transcoded from component 0; color images → `406` |
| 9 | High-Throughput JPEG 2000 (…4.201–…4.203) | not supported — stored and served as-is |

## 7. Character Sets

- The DICOM default character repertoire and Latin-1 pass through natively;
  UTF-8 (ISO_IR 192) is handled in JSON/XML responses.
- CJK character sets (ISO 2022 IR 13/87 — Japanese, KS X 1001 — Korean,
  GB18030 — Chinese) are decoded by the DICOM parser only when the binary is
  built with the `cjk-charsets` feature (`encoding_rs`); without it, CJK text
  is not decoded. The shipped default build includes the feature.

## 8. Configuration

- Server: nginx-style config file, validated by `--test-config`.
- Communication: HTTP/1.1 over TCP, keep-alive, thread-per-connection; TLS is
  terminated by the reverse proxy (see §5).
- Compile-time features: `transcode`, `s3`, `cjk-charsets`; `china_crypto`
  (planned: SM3 content-addressing instead of BLAKE3).

## 9. Trademarks

DICOM® is the registered trademark of the National Electrical Manufacturers
Association (NEMA) for its standards publications relating to digital
communications of medical information. DICOMweb™ is a trademark of NEMA.
Clarus is not affiliated with or endorsed by NEMA.
