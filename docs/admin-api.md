# Clarus admin API (/admin/v1)

This document is the operation manual for the Clarus administrative surface:
what each endpoint does and how to call it. Response bodies are described by
purpose; the exact JSON shapes live in the open source
(`clarus/src/handlers/admin/`). The invisible marker below is consumed by the
internal doc<->code consistency gate and is not part of the prose.

<!-- claim:admin_api endpoints="GET /admin/v1/status GET /admin/v1/health GET /admin/v1/version GET /admin/v1/dashboard GET /admin/v1/audit-log GET /admin/v1/access-log GET /admin/v1/storage GET /admin/v1/logs/stream GET /admin/v1/metrics/history GET /admin/v1/instances/search GET /admin/v1/instances/hidden GET /admin/v1/instances/raw GET /admin/v1/storage/s3-validation GET /admin/v1/storage/audit-copies GET /admin/v1/storage/cas-audit/status GET /admin/v1/recovery/rebuild/status POST /admin/v1/maintenance POST /admin/v1/storage/migrate POST /admin/v1/storage/cas-audit POST /admin/v1/instances/delete POST /admin/v1/instances/restore POST /admin/v1/recovery/rebuild" codes="200 202 400 403 404 409 413 500 502 503" -->

## Access model

- The admin surface is reachable only from the local machine, or from a
  client inside the configured `[server] admin_cidrs` networks. With no
  `admin_cidrs` configured the policy is exactly loopback-only.
  (source: `clarus/src/config.rs:1534`, `clarus/src/serve.rs:212`).
- The policy is decided on the resolved client address, never on the raw
  socket peer: behind a reverse proxy the server trusts the client address
  reported by the proxy, and only when that proxy is inside
  `[server] trusted_proxies` (`clarus/src/serve.rs:357-370`).
- Identity headers honoured from a trusted proxy: `X-Real-IP`,
  `X-Forwarded-For` (first element) and `X-Authenticated-User`
  (`crates/clarus-core/src/http/auth.rs:48-62`). From an untrusted peer they
  are ignored. A trusted proxy that sends identity headers but no
  authenticated user is rejected with 502 when `[server]
  require_proxy_identity = true` (`crates/clarus-core/src/http/auth.rs`,
  `clarus/src/config.rs:1542`).
- Authentication itself is not Clarus's job: passwords / client certs are
  checked by nginx in front (`clarus.conf:386-390`, the "nginx in front"
  block). The server only enforces the network allowlist.
- `/dicomweb/patients` (patient-level QIDO) shares the same admin policy
  (`crates/clarus-core/src/http/router.rs:189-192`).

## Endpoints

All routes below require the admin access policy above; an off-machine client
gets 403.

### GET (read)

| Method | Path | What it does | How to call |
|---|---|---|---|
| GET | `/admin/v1/status` | Uptime, RSS, storage tiers, NTP, study/instance/CAS counters | no parameters |
| GET | `/admin/v1/health` | Liveness probe: status, version, uptime | no parameters |
| GET | `/admin/v1/version` | API and core version numbers | no parameters |
| GET | `/admin/v1/dashboard` | Storage, alerts, current maintenance state | no parameters |
| GET | `/admin/v1/audit-log` | Delete/audit trail (the evidence journal) | no query filters today - defaults apply |
| GET | `/admin/v1/access-log` | Access journal | no query filters today - defaults apply |
| GET | `/admin/v1/storage` | Dedup ratio, tiers, disk usage | no parameters |
| GET | `/admin/v1/logs/stream` | Server log stream (Server-Sent Events) | `?level=` `?component=` `?tags=` |
| GET | `/admin/v1/metrics/history` | Metrics ring-buffer history | no query filters today - default range |
| GET | `/admin/v1/instances/search` | Instance search by manifest; patient names are always masked | no query filters today - defaults apply |
| GET | `/admin/v1/instances/hidden` | Sequestered instances, with the reason code that hid each one | `?study_uid=` `?series_uid=` `?sop_instance_uid=` `?reason=113001` filters; `?limit=` `?offset=` page |
| GET | `/admin/v1/instances/raw` | The stored bytes of one instance, hidden or not | `?sop_instance_uid=` (required); returns `application/dicom` |
| GET | `/admin/v1/storage/s3-validation` | S3 storage status - a stub today (S3 is on demand: built when a commercial customer needs it); reports `s3_feature_not_compiled` / `s3_not_configured`, not a live probe | no parameters |
| GET | `/admin/v1/storage/audit-copies` | Sidecar copy-audit snapshot | no parameters |
| GET | `/admin/v1/storage/cas-audit/status` | CAS integrity-audit progress | no parameters |
| GET | `/admin/v1/recovery/rebuild/status` | Manifest rebuild progress | no parameters |

Example:

    curl -s http://127.0.0.1:8024/admin/v1/status

### POST (mutate / trigger)

| Method | Path | What it does | How to call (JSON body unless noted) |
|---|---|---|---|
| POST | `/admin/v1/maintenance` | Enable/disable maintenance mode | `{"enable":true,"message":"...","eta_minutes":30}`; `?dry_run=true` previews |
| POST | `/admin/v1/storage/migrate` | Validate a tier-migration request | `{"reason":"...","target_tier":"cold","study_uid":"..."}`; `target_tier` is `hot` or `cold` |
| POST | `/admin/v1/storage/cas-audit` | Start a CAS integrity audit | no body; `?dry_run=true` previews; `/start` is an alias; 202 started, 409 if already running |
| POST | `/admin/v1/instances/delete` | Delete a study / series / instance list | `{"reason":"...","study_uid":"..."}` or `"series_uid"` or `"sop_instance_uids":[...]` - exactly one, max 500 UIDs |
| POST | `/admin/v1/instances/restore` | Clear the rejection reason on named instances, re-exposing them | `{"reason":"...","sop_instance_uids":[...]}` - both required, max 500 UIDs; idempotent |
| POST | `/admin/v1/recovery/rebuild` | Rebuild manifests from CAS sources | `{"sources":["hot","cold","s3"]}` - non-empty; 202 started, 409 if already running |

Example:

    curl -s -X POST 'http://127.0.0.1:8024/admin/v1/maintenance?dry_run=true' -d '{"enable":true,"message":"upgrade","eta_minutes":15}'

All POST bodies are capped by `[server] max_json_body` (default 1 MiB,
`clarus/src/config.rs:1523`): an oversized body is 413, an unparseable body
is 400 (`clarus/src/dispatch.rs:960-1010`).

## Error codes

| Code | Meaning | Returned when |
|---|---|---|
| 200 | OK | successful read / accepted mutation |
| 202 | Accepted | a background job (CAS audit, rebuild) started |
| 400 | Bad Request | body not a JSON object; required field missing (`reason`); invalid `target_tier`; invalid `sources`; wrong selector count; more than 500 SOP UIDs; restore without `reason` or `sop_instance_uids` |
| 403 | Forbidden | client not in the admin network allowlist |
| 404 | Not Found | delete target study absent (no manifest and no tombstone); unknown SOP Instance UID on `instances/raw` |
| 409 | Conflict | CAS audit or rebuild already running |
| 413 | Payload Too Large | body larger than `max_json_body` |
| 500 | Internal Server Error | audit append failed; tombstone rename failed |
| 502 | Bad Gateway | strict `require_proxy_identity` and a trusted proxy sent identity headers without a user |
| 503 | Service Unavailable | migrate to `cold` with no cold/S3 backend configured |

Error bodies use the shape `{"error":"<short>","message":"<detail>"}`
(`clarus/src/handlers/admin/pure.rs:1806`).

## Reverse proxy (nginx) layout

Clarus does not terminate TLS and does not check passwords; nginx owns the
edge. The server accepts a proxied request's client address only when the
proxy is in `trusted_proxies`, and the admin allowlist is then judged on that
reported address. The mandatory header for this is
`proxy_set_header X-Real-IP $remote_addr;` - without it the server sees the
proxy as the client and the admin gate is effectively bypassed
(`clarus.conf:75-84`). Authentication (basic auth or mTLS) is configured on
the nginx location (`clarus.conf:386-390`, `clarus.conf:1001-1008`).

## Response bodies

JSON shapes are not spelled out here: responses are described by purpose and
the exact shapes live in the open source (`clarus/src/handlers/admin/`). The
one invariant worth knowing is that patient names returned by
`instances/search` are always masked.
