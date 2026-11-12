# cltranscode — DICOM pipeline (transcode + anonymize + proxy)

- **4 input/output combinations:** DIMSE↔DIMSE, DIMSE↔DICOMweb
- **PS3.15 anonymization:** 5 profiles (Basic, RetainLongitudinal, RetainDevice, RetainInstitution, Custom)
- **Transcoding:** JPEG-LS, JPEG 2000, JPEG lossless/baseline, RLE
- **File-based outbox** with exponential backoff (1s → 2s → 4s → … → 60s max)
- **AWS SIGV4 + GCP OAuth2** cloud auth for DICOMweb output
- **(0400,0561) audit trail** — append, never overwrite. ModifyingSystem = "CLTRANSCODE"
- **Prometheus metrics** + `/health` endpoint (k8s-ready)

## Quick start

1. Edit `cltranscode.conf`
2. `./cltranscode --config cltranscode.conf`
3. Send DICOM to port 8106 (DIMSE) or 8107 (DICOMweb)

## Ports

| Port | Service |
|------|---------|
| 8106 | DIMSE SCP (modalities send here) |
| 8107 | DICOMweb HTTP (POST studies here) |
| 8108 | `/health` + `/metrics` (Prometheus) |

## Configuration

See `cltranscode.conf` for all options with comments.

### 5 use-case examples

#### 1. modality → transcode → clbridge → Clarus (default)
```ini
[input]
mode = dimse
dimse_port = 8106

[output]
mode = dimse
dimse_host = 127.0.0.1
dimse_port = 8104

[transcoding]
enabled = true
target_ts = JPEG_LOSSLESS
```

#### 2. modality → anonymize → transcode → Clarus (research export)
```ini
[input]
mode = dimse
dimse_port = 8106

[output]
mode = dicomweb
dicomweb_url = http://127.0.0.1:8024/dicomweb

[anonymizer]
profile = Basic
salt = my-research-salt-2026

[transcoding]
enabled = true
target_ts = JPEG_LOSSLESS
```

#### 3. DICOMweb → anonymize → DICOMweb (cloud anonymization proxy)
```ini
[input]
mode = dicomweb
dicomweb_port = 8107

[output]
mode = dicomweb
dicomweb_url = https://cloud-pacs.example.com/dicomweb
dicomweb_auth = aws

[anonymizer]
profile = Basic
salt = cloud-proxy-salt
```

#### 4. modality → transcode → AWS S3 (cloud archive)
```ini
[input]
mode = dimse
dimse_port = 8106

[output]
mode = dicomweb
dicomweb_url = https://s3.amazonaws.com/dicom-bucket/dicomweb
dicomweb_auth = aws

[transcoding]
enabled = true
target_ts = JPEG2000_LOSSLESS
```

#### 5. DICOMweb → transcode → DIMSE (PACS migration)
```ini
[input]
mode = dicomweb
dicomweb_port = 8107

[output]
mode = dimse
dimse_host = 192.168.1.100
dimse_port = 104
dimse_ae_title = MIGRATION

[transcoding]
enabled = true
target_ts = JPEG_LOSSLESS
```

## Multi-instance deployment

Run several `cltranscode` instances side-by-side — each with its own config,
ports, and outbox directory. nginx-way: one process, one job.

**Port allocation rule:** every instance needs 3 unique ports:
| Port | Config key | Example instance 1 | Example instance 2 |
|------|-----------|-------------------|-------------------|
| DIMSE input | `[input] dimse_port` | 8106 | 8116 |
| DICOMweb input | `[input] dicomweb_port` | 8107 | 8117 |
| Health/metrics | `[health] port` | 8108 | 8118 |

(!!!) **Each instance MUST have a unique `[health] port`.** The health HTTP
server binds to this port — two instances on the same port will fail to start.

**Graceful shutdown on port conflict:** if any port is already in use,
cltranscode logs the conflict and exits with code 1 (no panic, no retry loop).
Check logs for `address already in use` to identify which port conflicts.

### Example topology

```
                    ┌──────────────────────────────────┐
  Modality ──→ :8106│ cltranscode (tc-1)               │──→ :8104 clbridge → Clarus
  (CT/MR)           │ transcode-only                   │    (JPEG_LS → JPEG Lossless)
                    │ outbox: ./tc-outbox/             │
                    │ health: :8108                    │
                    └──────────────────────────────────┘

                    ┌──────────────────────────────────┐
  Modality ──→ :8116│ cltranscode (anon-1)             │──→ :8024 Clarus DICOMweb
  (Research)        │ anonymize-only                   │    (Basic profile)
                    │ outbox: ./anon-outbox/           │
                    │ health: :8118  ← DIFFERENT port  │
                    └──────────────────────────────────┘

                    ┌──────────────────────┐
  DICOMweb ──→ :8127│ cltranscode          │──→ :8124 legacy PACS
  (Cloud)           │ transcode + anon     │    (JPEG2000 → JPEG Baseline)
                    │ outbox: ./cloud-outbox/
                    │ health: :8108        │
                    └──────────────────────┘
```

### Instance 1: transcode-only (JPEG-LS → JPEG Lossless)

```ini
# tc-only.conf
[input]
mode = dimse
dimse_port = 8106

[output]
mode = dimse
dimse_host = 127.0.0.1
dimse_port = 8104

[transcoding]
enabled = true
target_ts = JPEG_LOSSLESS

[outbox]
dir = ./tc-outbox
max_pending = 500

[logging]
level = INFO
log_dir = ./tc-logs
```

```bash
./cltranscode tc-only.conf
# → receives DICOM on :8106, transcodes, forwards to clbridge :8104
# → health/metrics on :8108
# → no anonymizer — salt left empty (default)
```

### Instance 2: anonymize-only (Basic profile, research export)

```ini
# anon-only.conf
[input]
mode = dimse
dimse_port = 8116

[output]
mode = dicomweb
dicomweb_url = http://127.0.0.1:8024/dicomweb

[anonymizer]
profile = Basic
salt = research-salt-2026

[transcoding]
enabled = false

[outbox]
dir = ./anon-outbox
max_pending = 1000

[logging]
level = INFO
log_dir = ./anon-logs
```

```bash
./cltranscode anon-only.conf
# → receives DICOM on :8116, anonymizes, forwards to Clarus :8024
# → health/metrics on :8108
# → transcoding disabled — pixels pass through as-is
```

### Instance 3: transcode + anonymize (cloud proxy)

```ini
# cloud-proxy.conf
[input]
mode = dicomweb
dicomweb_port = 8127

[output]
mode = dimse
dimse_host = 192.168.1.100
dimse_port = 8124
dimse_ae_title = CLOUD2PACS

[anonymizer]
profile = RetainLongitudinal
salt = cloud-salt-2026

[transcoding]
enabled = true
target_ts = JPEG_BASELINE

[outbox]
dir = ./cloud-outbox
max_pending = 2000

[logging]
level = DEBUG
log_dir = ./cloud-logs
```

```bash
./cltranscode cloud-proxy.conf
# → receives DICOMweb on :8127, anonymizes, transcodes to JPEG Baseline
# → forwards to legacy PACS :8124
# → health/metrics on :8108
# → RetainLongitudinal — keeps PatientBirthDate for longitudinal studies
```

### Running all three

```bash
# Three independent processes, three configs, three port sets
./cltranscode tc-only.conf &
./cltranscode anon-only.conf &
./cltranscode cloud-proxy.conf &
```

### Prometheus scrape

> **⚠️ Current limitation:** health/metrics port is hard-coded to 8108.
> Three instances on one host → Prometheus can only scrape the first.
> **P30 (planned):** configurable health port (`health_port = 8108`).
>
> **Workaround for now:** run instances on different hosts/containers,
> or scrape metrics through an external aggregator (Telegraf, Vector).

## Health & Metrics

```bash
# Liveness (process alive)
curl http://localhost:8108/health
# → 200 OK

# Readiness (input + output reachable)
curl http://localhost:8108/health?readiness=true
# → 200 OK or 503 Service Unavailable

# Prometheus metrics
curl http://localhost:8108/metrics
# → text/plain; version=0.0.4
```

### Key metrics
```
cltranscode_input_total{protocol="dimse"}
cltranscode_input_total{protocol="dicomweb"}
cltranscode_output_total{protocol="dimse",status="ok"}
cltranscode_output_total{protocol="dimse",status="fail"}
cltranscode_output_total{protocol="dicomweb",status="ok"}
cltranscode_output_total{protocol="dicomweb",status="fail"}
cltranscode_transcode_total
cltranscode_transcode_fallback_total
cltranscode_anonymize_total
cltranscode_outbox_pending
cltranscode_outbox_failed_total
cltranscode_uptime_seconds
```

## Backpressure

When outbox `pending/` reaches `max_pending` (default: 1000):
- **DIMSE:** new C-STORE receives `0xA700` (out of resources) — modality retries
- **DICOMweb:** new POST receives HTTP 503 + `Retry-After`

Downstream 429/503 is translated to DIMSE `0xA700` for transparent backpressure propagation.

## Logging

- **stderr:** compact, human-readable, timestamped — always
- **File:** JSON + daily rotation — when `log_dir` is set
- **One INFO line per study:** `study accepted`, `study transcoded`, `study anonymized`, `study delivered`
- **DEBUG:** PDU fragments, DICOMweb boundaries, codec details

## Building

```bash
cargo build -p cltranscode --release
```

With JPEG/JPEG2000 codec support:
```bash
cargo build -p cltranscode --release --features transcode
```

## DISCLAIMER

⚠️ **NOT a medical device.** Validation required before clinical use.
This software is provided under LGPLv3. See LICENSE.
