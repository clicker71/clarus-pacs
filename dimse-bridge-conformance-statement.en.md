# DICOM Conformance Statement - Clarus DIMSE Bridge (clbridge)

**Product:** Clarus bridge (DIMSE gateway)
**Version:** 0.3.0-alpha
**Date:** 2026-09-08
**Standards:** DICOM PS3.2 (Conformance, statement structure), PS3.7
(DIMSE), PS3.8 (ACSE); DICOMweb PS3.18 (user-agent side; per PS3.18 6 the
proxy functionality between DIMSE and the equivalent Web Services is
described below)

The DICOMweb origin server side has its own statement:
[dicomweb-conformance-statement.md](./dicomweb-conformance-statement.md).

---

## 1. Introduction

The bridge is a DIMSE-to-DICOMweb gateway: it receives classic DIMSE
traffic from modalities and workstations and translates it into DICOMweb
calls to the configured DICOMweb origin server (any DICOMweb-compatible
server; Clarus is the reference target). It is not an origin server
itself.

## 2. Implementation Model

- **Roles:** DIMSE **SCP** (receives C-ECHO / C-STORE / C-FIND / C-MOVE /
  N-ACTION), DIMSE **SCU** (C-STORE outbound for C-MOVE destinations and
  for one optional configured forwarding destination, 5.6), DICOMweb
  **user agent** (QIDO-RS / WADO-RS / STOW-RS towards the configured
  DICOMweb origin server).
- **Real-world activity:** modalities send studies via C-STORE;
  workstations query via C-FIND and retrieve via C-MOVE; imaging devices
  request Storage Commitment via N-ACTION. The bridge translates each
  request to the corresponding DICOMweb transaction and returns standard
  DIMSE responses.

## 3. AE Specification

- **AE Title (SCP):** `CLBRIDGE` (default, configurable).
- **Port:** `8104` default (configurable).
- **Calling AE:** accepted from any AE; association accepted
  unconditionally (result 0x0000). AE titles longer than 16 chars are
  truncated in logs.
- **Timers:** inbound ACSE 15 s, inbound DIMSE 60 s (configurable);
  inbound network timeout disabled (SCP waits). Outbound (C-MOVE
  delivery) associations use the configured ACSE timeout for the main
  association and a fixed 5 s ACSE timeout for fallback associations.
- **User identity negotiation:** none (not offered, not requested).
- **Maximum PDU:** pynetdicom default (16382 bytes).

## 4. Presentation Contexts (SCP)

| Abstract syntax | Transfer syntax(es) proposed | Notes |
|---|---|---|
| Verification (1.2.840.10008.1.1) | Explicit VR LE, Implicit VR LE | C-ECHO |
| Storage SOP Classes | Explicit VR LE, Implicit VR LE, JPEG Lossless SV1 (4.70), JPEG-LS Lossless (4.80) | C-STORE; registry-driven (`sop_classes.yaml`, 71 SOP classes in the field build), fallback: built-in SOP classes |
| Patient Root Q/R FIND (5.1.4.1.2.1.1) | Explicit VR LE, Implicit VR LE | C-FIND |
| Study Root Q/R FIND (5.1.4.1.2.2.1) | Explicit VR LE, Implicit VR LE | C-FIND |
| Modality Worklist FIND (5.1.4.1.2.1.31) | Explicit VR LE, Implicit VR LE | C-FIND (MWL) |
| Patient/Study Root Q/R MOVE | Explicit VR LE, Implicit VR LE | C-MOVE |
| Storage Commitment Push Model (5.1.4.1.1.1) | Explicit VR LE, Implicit VR LE | N-ACTION (SCP) and N-EVENT-REPORT (SCU) |

Uncompressed transfer syntaxes are proposed first (Explicit VR Little
Endian before Implicit VR Little Endian). Not offered: C-GET (rejected by
design - use C-MOVE), N-SET.

## 5. Supported DIMSE Services

### 5.1 C-ECHO (SCP)

Answers `0x0000`. A peer that has sent C-ECHO (or C-STORE) becomes an
auto-registered C-MOVE destination.

### 5.2 C-STORE (SCP)

Each received instance is forwarded to STOW-RS on the configured DICOMweb
origin server. Responses:

| Status | Meaning |
|---|---|
| 0x0000 | success |
| 0x0122 | SOP Class not supported (PS3.4 Table B.2-1) |
| 0xA700 | out of resources (server unreachable or in-memory queue near full) |
| 0xC000 | hard rejection (queue >= 95%) |

Queue and worker sizing are automatic (RAM-based), tunable via config.
Batch delivery: multiple instances are committed to the origin server in
single batch STOW-RS requests.

### 5.3 C-FIND (SCP)

Queries are translated to QIDO-RS. Patient/study-level queries target
`GET /dicomweb/studies`; series-level queries use a two-step flow
(studies by study keys, then per-study series with series keys and
Modality). C-FIND responses are stamped with the QueryRetrieveLevel echo
and the bridge Retrieve AE Title. Time values are normalized to the
canonical `HHMMSS` form (PS3.5 6.2) before being returned. The
`SpecificCharacterSet` value received from the origin server (ISO_IR 192)
is honored. Optional deviation: `fuzzymatch_override` in the server config
forces Levenshtein-1 matching on person-name keys even when the device
does not send `fuzzymatching=true` (non-standard convenience extension).

### 5.4 C-MOVE (SCP + SCU)

Retrieval is performed via WADO-RS against the configured DICOMweb origin
server and forwarded to the move destination with C-STORE as SCU.

- **Stored-first passthrough:** the study is fetched once without a
  `transferSyntax` parameter; each instance's actual transfer syntax is
  read from the multipart part header (`transfer-syntax=` parameter) and,
  as a last resort, from the file meta `(0002,0010)` of the received
  bytes. Instances are delivered as stored, with no re-encoding, whenever
  the negotiated context permits.
- **Transfer syntax negotiation:** each outbound C-STORE association
  proposes, for the instance's SOP Class, Explicit VR Little Endian,
  Implicit VR Little Endian, JPEG Lossless SV1 (4.70) and JPEG-LS
  Lossless (4.80). Negotiation is acceptor-driven (PS3.8).
- **Refetch on rejection:** if the destination association rejects the
  stored transfer syntax, the bridge refetches the instance from the
  origin server in a transfer syntax the destination accepted and
  delivers that. Per-job memory of rejected (SOP Class, transfer syntax)
  pairs skips further doomed fallback associations for the rest of the
  job. Refetched or transcoded instances are counted and reported in the
  job log (`passthrough=`, `transcoded=`, `refetched=`).
- **Destinations:** any peer that has sent C-ECHO or C-STORE is
  auto-registered as a move destination; additional destinations are
  configured in `peers.conf` as `AE_TITLE IP PORT` lines (exact addresses
  or subnets). The destination address is resolved from the configured
  peer list by AE Title, not from the C-MOVE request. An unknown Move
  Destination AE Title is answered with `0xA801` (PS3.4 C.4.2.2, Table
  C.4-2).
- **Final statuses:** successful delivery is `0x0000`. A job with zero
  delivered instances returns `0xC000` with an Error Comment (0000,0902).
  Failed SOP Instance UID List (0008,0058) is attached to Cancel,
  Failure and Warning final responses (PS3.4 C.4.2.1.4.2).
- **Pacing and retries:** jobs to one peer are serialized with a
  configurable cooldown (`outbound_assoc_gap`) anchored at the end of the
  previous job. The main delivery association is retried once after a
  10 s delay when it was not established; fallback associations are not
  retried. `0xA700` is returned on resource exhaustion.

### 5.5 N-ACTION / N-EVENT-REPORT - Storage Commitment (SCP + SCU)

Storage Commitment Push Model (PS3.4 Annex J): the modality sends
N-ACTION; the bridge verifies in the background and delivers the result
as N-EVENT-REPORT.

- **N-ACTION-RSP** is a pure acknowledgement: `0000` (accepted for
  processing) or a generic refusal (0x0122 / 0x0213 / 0x0124 on malformed
  requests). No service-class-specific statuses are returned in the RSP
  (J.3.2.1.4); per-SOP outcomes are reported only in the N-EVENT-REPORT.
- **Verification** runs in a background worker (WADO-RS against the
  archive), bounded by a 300 s deadline per transaction.
- **One N-EVENT-REPORT per transaction** (J.3.3.1.2) with the complete
  SOP set:
  - Event Type ID 1 - success, Referenced SOP Sequence (0008,1199);
  - Event Type ID 2 - Referenced SOP Sequence + Failed SOP Sequence
    (0008,1198) + Failure Reason (0008,1197): `0110H` processing failure,
    `0111H` no such object instance. Unanswered SOPs after the 300 s
    deadline are reported as `0110H`.
- **Delivery order:** (1) on the held association via
  `send_n_event_report`; (2) if released - reconnect to the configured
  commitment peer with the called AE = the tracked calling AE of the
  original association and our AE title unchanged (J.3.3.1.3 Notes 1-2);
  (3) undeliverable - WARN and drop (the SCU re-requests per its own
  timeout, J.3.3.1.2).

### 5.6 Forwarding to a second (non-authoritative) archive (SCU, optional)

Configuration: `[forward]` block in `clbridge.conf`; disabled by default
(`enabled = false`). When enabled, the bridge additionally sends every
received SOP Instance as C-STORE SCU to one configured destination
(AE title + host + port).

- **Best-effort:** the status returned to the originating modality is
  decided by the primary store-and-forward path only; forwarding failures
  are logged and counted and never change that status.
- **Sent as stored:** instances are forwarded in their stored transfer
  syntax; no conversion is performed.
- **Retries:** failed deliveries are retried (default 10 attempts,
  exponential backoff) and then kept in an undelivered directory for
  operator action; queue overflow drops the newest item and is counted.
- **Storage Commitment is not involved:** the second archive is
  non-authoritative. The bridge sends it no N-ACTION and expects no
  N-EVENT-REPORT from it; per PS3.4 J.3.1 Storage Commitment is a
  two-party contract between the requesting SCU and the storing SCP.
- **No query/retrieve with the second archive:** it is a C-STORE
  receiver only; no C-FIND / C-MOVE is offered to or accepted from it.

## 6. DICOMweb User Agent Conformance

Towards the configured DICOMweb origin server the bridge uses QIDO-RS
(search), STOW-RS (store, including batched multi-instance requests) and
WADO-RS (retrieve, including study-level multipart with per-part
transfer-syntax parsing). HTTP connections are keep-alive; the origin
server's transfer-syntax capability list is fetched once and cached
(120 s). The bridge does not expose DICOMweb services itself.

## 7. Known Deviations and Limitations

| # | Item | Status |
|---|---|---|
| 1 | C-GET | deliberately rejected - use C-MOVE |
| 2 | `fuzzymatch_override` | non-standard convenience extension (5.3) |
| 3 | C-MOVE per-instance tracking is in-memory by default | persistent index via `sop_index_db` |
| 4 | DIMSE runs over plain TCP | TLS/Authentication is the responsibility of the network/OS layer |
| 5 | Patient names in logs | masked (first/last letter), UIDs remain |

## 8. Trademarks

DICOM is the registered trademark of the National Electrical
Manufacturers Association (NEMA) for its standards publications relating
to digital communications of medical information. DICOMweb is a trademark
of NEMA. Clarus is not affiliated with or endorsed by NEMA.
