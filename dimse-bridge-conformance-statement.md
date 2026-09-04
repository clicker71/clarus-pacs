# DICOM® Conformance Statement — Clarus DIMSE Bridge (clbridge)

**Product:** Clarus bridge (DIMSE gateway) · **Version:** 0.3.0-alpha · **Date:** 2026-08-27
**Standards:** DICOM PS3.2 (Conformance, statement structure), PS3.7 (DIMSE), PS3.8 (ACSE); DICOMweb PS3.18 (user-agent side, per PS3.18 §6 the proxy functionality between DIMSE and the equivalent Web Services is described below)

Re-checked against the bridge implementation on 2026-08-27 (SOP-class
registry, C-FIND dispatch incl. MWL, `0xA700` backpressure, N-ACTION /
N-EVENT-REPORT storage commitment, C-GET rejection).

---

## 1. Introduction

The bridge is a DIMSE-to-DICOMweb gateway: it receives classic DIMSE traffic
from modalities and workstations and translates it into DICOMweb calls to
the configured DICOMweb origin server (any DICOMweb-compatible server;
Clarus is the reference target). It is not an origin server itself.

## 2. Implementation Model

- **Roles:** DIMSE **SCP** (receives C-ECHO / C-STORE / C-FIND / C-MOVE /
  N-ACTION), DIMSE **SCU** (C-STORE outbound for C-MOVE destinations),
  DICOMweb **user agent** (QIDO-RS / WADO-RS / STOW-RS towards the
  configured DICOMweb origin server).
- **Real-world activity:** modalities send studies via C-STORE; workstations
  query via C-FIND and retrieve via C-MOVE; imaging devices request Storage
  Commitment via N-ACTION. The bridge translates each request to the
  corresponding DICOMweb transaction and returns standard DIMSE responses.

## 3. AE Specification

- **AE Title (SCP):** `CLBRIDGE` (default, configurable).
- **Port:** `8104` default (configurable).
- **Calling AE:** accepted from any AE; association accepted unconditionally
  (result 0x0000). AE titles longer than 16 chars are truncated in logs.
- **Timers:** ACSE 15 s, DIMSE 60 s (configurable); inbound network timeout
  disabled (SCP waits).
- **User identity negotiation:** none (not offered, not requested).
- **Maximum PDU:** pynetdicom default (16382 bytes).

## 4. Presentation Contexts (SCP)

| Abstract syntax | Transfer syntax(es) | Notes |
|---|---|---|
| Verification (1.2.840.10008.1.1) | Implicit VR LE | C-ECHO |
| Storage SOP Classes | per `sop_classes.yaml` | C-STORE; registry-driven (default `sop_classes.yaml`, 71 SOP classes in the field build), fallback: 11 built-in SOP classes (8 active + 3 retired) |
| Patient Root Q/R FIND (…5.1.4.1.2.1.1) | Implicit VR LE | C-FIND |
| Study Root Q/R FIND (…5.1.4.1.2.2.1) | Implicit VR LE | C-FIND |
| Modality Worklist FIND (…5.1.4.1.2.1.31) | Implicit VR LE | C-FIND (MWL) |
| Patient/Study Root Q/R MOVE | Implicit VR LE | C-MOVE |
| Storage Commitment Push Model (…5.1.4.1.1.1) | Implicit VR LE | N-ACTION (SCP) and N-EVENT-REPORT (SCU) |

Not offered: C-GET (rejected by design — use C-MOVE), N-SET.

## 5. Supported DIMSE Services

### 5.1 C-ECHO
SCP: answers `0x0000`. Used also for auto-registering move destinations.

### 5.2 C-STORE (SCP)
Each instance is forwarded to STOW-RS on the configured DICOMweb origin
server. Responses:
success `0x0000`. Backpressure: the bridge returns `0xA700` (Out of
Resources) when the server is unreachable or the in-memory queue is near
full, and `0xC000` on hard rejection (queue ≥ 95%). Queue and worker sizing
are automatic (RAM-based), tunable via config.

### 5.3 C-FIND (SCP)
Patient/Study-level queries are translated to QIDO-RS `GET /dicomweb/studies`.
Optional deviation: `fuzzymatch_override` in the server config forces
Levenshtein-1 matching on person-name keys even when the device does not
send `fuzzymatching=true` (non-standard convenience extension, opt-in).

### 5.4 C-MOVE (SCP + SCU)
Retrieval is performed via WADO-RS against the configured DICOMweb origin
server and forwarded to the move destination with C-STORE as SCU. **Allowed destinations:** any peer that has
sent C-ECHO or C-STORE is auto-registered as a move destination; additional
destinations are configured in `peers.conf` as `AE_TITLE IP PORT` lines (the
port may also follow a colon), where the address may be exact (`127.0.0.1`)
or a subnet (`192.168.1.0/24`, or full mask `192.168.1.0 255.255.255.0`) so
an entire network may receive moves. Statuses: standard C-MOVE
progress/final responses; `0xA700` on resource exhaustion.

### 5.5 N-ACTION / N-EVENT-REPORT — Storage Commitment (SCP + SCU)

Storage Commitment Push Model (PS3.4 Annex J): the modality sends N-ACTION;
the bridge verifies in the background and delivers the result as
N-EVENT-REPORT.

- **N-ACTION-RSP** is a pure acknowledgement: `0000` (accepted for
  processing) or a generic refusal (0x0122 / 0x0213 / 0x0124 on malformed
  requests). No service-class-specific statuses are returned in the RSP
  (J.3.2.1.4); per-SOP outcomes are reported only in the N-EVENT-REPORT.
- **Verification** runs in a background worker (WADO-RS against the
  archive), bounded by a 300 s deadline per transaction.
- **One N-EVENT-REPORT per transaction** (J.3.3.1.2) with the complete SOP
  set:
  - Event Type ID 1 — success, Referenced SOP Sequence (0008,1199);
  - Event Type ID 2 — Referenced SOP Sequence + Failed SOP Sequence
    (0008,1198) + Failure Reason (0008,1197): `0110H` processing failure,
    `0111H` no such object instance. Unanswered SOPs after the 300 s
    deadline are reported as `0110H`.
- **Delivery order:** (1) on the held association via `send_n_event_report`;
  (2) if released — reconnect to the configured commitment peer with the
  called AE = the tracked calling AE of the original association and our AE
  title unchanged (J.3.3.1.3 Notes 1-2); (3) undeliverable — WARN and drop
  (the SCU re-requests per its own timeout, J.3.3.1.2).

## 6. DICOMweb User Agent Conformance

Towards the configured DICOMweb origin server the bridge uses: QIDO-RS
(search), STOW-RS (store), WADO-RS (retrieve). See
[dicomweb-conformance-statement.md](./dicomweb-conformance-statement.md) for
the origin-server side. The bridge does not expose DICOMweb services itself.

## 7. Known Deviations and Limitations

| # | Item | Status |
|---|---|---|
| 1 | C-GET | deliberately rejected — use C-MOVE |
| 2 | `fuzzymatch_override` | non-standard opt-in extension |
| 3 | C-MOVE per-instance tracking is in-memory by default | persistent index via `sop_index_db` |
| 4 | DIMSE runs over plain TCP | TLS/Authentication is the responsibility of network/OS layer |
| 5 | Patient names in logs | masked (first/last letter), UIDs remain |

## 8. Trademarks

DICOM® is the registered trademark of the National Electrical Manufacturers
Association (NEMA) for its standards publications relating to digital
communications of medical information. DICOMweb™ is a trademark of NEMA.
Clarus is not affiliated with or endorsed by NEMA.
