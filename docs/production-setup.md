# Clarus + clbridge: production setup

This document takes a fresh machine to a running Clarus PACS: the DICOMweb
server and the DIMSE bridge that scanners talk to. It is a shortest path, not
a reference. Hardening follows the order you would do it on a live system:
run it, put nginx in front, lock it down, then back it up.

The two shipped configs are the real manual. Every key is commented in place,
with the reasoning next to the value: `clarus.conf` for the server (about a
thousand lines) and `clbridge/clbridge.conf` for the bridge. When this page and
a config comment disagree, the config wins - and file a bug, because so would
we.

## Architecture in one picture

```
  Scanner (modality)
        |  DIMSE - the classic DICOM network protocol scanners use
        v
  clbridge            :8104   AE (Application Entity) title CLBRIDGE
        |  STOW-RS over HTTP (the DICOMweb "store this study" request)
        v
  clarus              :8024   DICOMweb, viewer, /health, /metrics
        |                          |
        |                          +--> nginx --> viewers (browser, OHIF, Weasis)
        |
        +--> optional: second archive (best-effort mirror, off by default)
```

The bridge is the only component a scanner ever sees. It speaks DIMSE, then
forwards each study to Clarus over DICOMweb. Clarus stores everything on local
disk and serves it to viewers through nginx. The second archive is optional
and disabled by default.

## Ports

| Port | Component | Protocol | Notes |
| ---- | --------- | -------- | ----- |
| 8024 | clarus | HTTP (DICOMweb, viewer, probes, metrics) | `[server] port = 8024` |
| 8104 | clbridge | DIMSE | `[server] port = 8104`, AE title `CLBRIDGE` |
| 8105 | clbridge | HTTP (`/health`, `/metrics`) | bound to `127.0.0.1` |

8104 is the bridge's default. Some scanners insist on the classic DICOM port
104. If yours does, set `[server] port = 104` in `clbridge.conf` and run the
bridge elevated - ports below 1024 are privileged on both Linux
(`CAP_NET_BIND_SERVICE` or root) and Windows (administrator).

## Quick start (works out of the box)

Two ways to get the server binary. Prefer the first:

- **Prebuilt binary (no build):** download `clarus` for your platform from
  the Releases page, then `chmod +x clarus`. Use it in place of
  `./target/release/clarus` in the commands below. Published with the first
  binary release; until then, use the build below.
- **Build from source:** Clarus is LGPL-3.0, so the source is published and
  you are free to build it yourself. From the repository root (see
  "Production install on Linux" for the minimal clone that avoids pulling the
  whole repository).

Nothing needs editing to get a working system.

```
cargo build --release -p clarus
chmod 600 clarus.conf
./target/release/clarus --config clarus.conf
```

Clarus is up when the banner prints the listen address. Leave it running and
open a second terminal.

```
python clbridge/clbridge.py --config clbridge/clbridge.conf
```

The bridge is up when it logs that it is listening on 8104 with AE title
CLBRIDGE. It reads `clarus_url = http://127.0.0.1:8024/dicomweb` from its own
config, so it finds the server without further setup.

Open three URLs: `http://localhost:8024/` is the embedded viewer, and it is
your visual ping - the header gauges (manifest/database, storage, free disk,
memory) only show numbers when the server is genuinely healthy.
`http://localhost:8024/health` gives `200` healthy or `503` degraded, and
`http://localhost:8024/metrics` is the Prometheus scrape endpoint.

Now point your scanner's destination at the bridge: AE title `CLBRIDGE`, port
`8104`, host as appropriate for your network. To test without a scanner, send
one file:

```
curl -X POST http://127.0.0.1:8024/dicomweb/studies -H "Content-Type: application/dicom" --data-binary @file.dcm
```

Then reload the viewer at `http://localhost:8024/` - the study appears in the
patient list. That is the whole loop working end to end.

## Pointing at your own config file, and checking it

Both binaries take a config path. Leave the flag off and each looks for its
config next to itself: `clarus` reads `clarus.conf` from the directory that
holds the binary, and `clbridge.py` reads `clbridge.conf` from the script's
directory. Pass `--config <path>` (the server also accepts `-c`) to use any
other file, so you can keep several configs for different machines or ports.

Before you start the server, check the file you just edited:

```
./target/release/clarus -t -c clarus.conf
```

If the file parses, this prints `Config OK: ...` followed by the values the
server will actually use (host and port, storage directory, size caps,
timeouts, workflow, transcode). If it does not parse, it prints
`[FATAL] Error parsing ...` with the reason and exits non-zero - fix that and
re-run. The long forms are `--test-config` and `--config`.

The bridge has no separate check flag: run it once in the foreground and it
validates the config at startup, either logging that it is listening on 8104
or exiting with a list of the offending keys and their allowed ranges.

Note: `clarus --help` is not a flag - it prints the usage line as a side
effect and exits non-zero. Use `--test-config` to see what the server actually
read.

## Production install on Linux (`/opt/clarus`)

This layout is the field-verified layout from a running deployment, not a file
in the repository: the repo ships no installer and no service unit.

Get the source without pulling the whole repository - a partial clone plus a
sparse checkout of just the build files:

```
git clone --depth 1 --filter=blob:none https://github.com/clicker71/clarus-pacs.git
cd clarus-pacs
git sparse-checkout init --cone
git sparse-checkout set crates clarus clbridge Cargo.toml Cargo.lock rust-toolchain.toml .cargo clarus.conf
```

That checks out only what the build needs: the server crates (`crates/` and
`clarus/`) plus the workspace files, and the bridge (`clbridge/`). Build on
the target machine (or cross-build a static binary):

```
cargo build --release -p clarus
```

For a statically linked binary that runs on any Linux distribution:

```
cargo build --target x86_64-unknown-linux-musl --release
```

For the last few percent of speed, build on the machine that will run the
server and let the compiler target that exact CPU:

```
RUSTFLAGS="-C target-cpu=native" cargo build --release -p clarus
```

This binary will not run on other or older CPUs, so do not move it to a
different machine. The release binaries stay universal (they run on any
x86_64); this is only for a binary you build for your own machine. The stock
`--release` profile is already aggressive (link-time optimization, one codegen
unit, panic=abort), so native is a marginal gain, not a different league.

If your archive holds JPEG 2000 or JPEG-LS studies that must be decoded
server-side, build with the transcode feature instead:

```
cargo build --release --features transcode -p clarus
```

**Warning - the transcode build widens the attack surface.** Transcoding means
decoding attacker-supplied compressed images, which is the classic vector for
decompression bombs, and it raises memory use. Mitigations that ship in the
build: a global bounded pool caps how many transcodes run at once, and each
instance's decoded size is capped before decode, so worst case CPU and memory
are both hard-bounded. When the pool is saturated the server answers an honest
`503` with `Retry-After: 5` rather than queueing or pegging every core. Keep
such clients behind nginx and restrict their list hard. If you do not need
transcoding, build without the feature - that is the safer default.

Copy the pieces into place:

```
sudo mkdir -p /opt/clarus
sudo cp target/release/clarus /opt/clarus/clarus
sudo cp clarus.conf /opt/clarus/clarus.conf
sudo cp clbridge/clbridge.py /opt/clarus/clbridge.py
sudo cp clbridge/clbridge.conf /opt/clarus/clbridge.conf
sudo cp clbridge/peers.conf /opt/clarus/peers.conf
sudo chmod 600 /opt/clarus/clarus.conf /opt/clarus/clbridge.conf
```

The bridge is Python, shipped as a single generated file: `clbridge.py` is
assembled by `clbridge/bundle.py` from the `clean_code/` package, so the
runtime file is one file even though the source is a package. It still needs
its third-party libraries, so give it a virtual environment and install them:

```
sudo python3 -m venv /opt/clarus/venv
sudo /opt/clarus/venv/bin/pip install -r clbridge/requirements.txt
```

Without this `systemctl start clbridge` fails with `ModuleNotFoundError` in
the journal. The file pins the five packages the bridge needs (`pynetdicom`,
`pydicom`, `requests`, `httpx`, `prometheus_client`).

For a fully self-contained, no-Python, air-gapped install, use the
PyInstaller one-file build instead - the release ships `clbridge.exe`
(Windows; about 36 MB) with Python and every dependency bundled, so no venv
and no `pip` are needed. On Linux the release ships the Python bundle above.

Clarus refuses to start on a world-readable config (it holds S3 keys). The
server self-fixes the mode to `600` and continues; it only refuses if the fix
fails on a read-only mount.

First run, in the foreground, so you see any error before it becomes a service:

```
cd /opt/clarus && ./clarus --config /opt/clarus/clarus.conf
```

## Run as a service

Two units, named `clarus` and `clbridge`. The unit names matter: the bridge
unit is `clbridge`, never `clarus-bridge`. Both run as an unprivileged
service account:

```
sudo useradd --system --no-create-home --shell /usr/sbin/nologin clarus
sudo chown -R clarus: /opt/clarus
```

`/etc/systemd/system/clarus.service`:

```
[Unit]
Description=Clarus PACS
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=clarus
ExecStart=/opt/clarus/clarus --config /opt/clarus/clarus.conf
WorkingDirectory=/opt/clarus
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

`/etc/systemd/system/clbridge.service`:

```
[Unit]
Description=Clarus DIMSE bridge
After=network-online.target clarus.service
Wants=network-online.target
Requires=clarus.service

[Service]
Type=simple
User=clarus
ExecStart=/opt/clarus/venv/bin/python /opt/clarus/clbridge.py --config /opt/clarus/clbridge.conf
WorkingDirectory=/opt/clarus
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

Enable and start them, then read the journal:

```
sudo systemctl daemon-reload
sudo systemctl enable --now clarus clbridge
sudo journalctl -u clarus -u clbridge -f
```

Both units log to journald and restart on failure. The health signal to wire
into monitoring is `GET /health`: `200` means healthy, `503` means degraded -
the server is up but something it depends on is not.

On Windows, use NSSM (the Non-Sucking Service Manager) for the same shape:

```
nssm install clarus "C:\clarus\clarus.exe" --config "C:\clarus\clarus.conf"
nssm set clarus AppExit Default Restart
```

**Update rule, hard:** stop the service, copy the new binary, start it again.
Never copy over a running service. The running process holds the old image and
you get a mixed state that is very hard to diagnose.

```
sudo systemctl stop clarus
sudo cp target/release/clarus /opt/clarus/clarus
sudo systemctl start clarus
```

## nginx in front

nginx owns TLS and authentication. Clarus never checks a password; that is
deliberate, and it is why the proxy headers below are not optional.

```nginx
server {
    listen 443 ssl;
    server_name pacs.example.com;

    ssl_certificate     /etc/nginx/tls/pacs.crt;
    ssl_certificate_key /etc/nginx/tls/pacs.key;

    # Scanners: admitted by network address, no user identity.
    location /dicomweb/ {
        allow 192.168.1.0/24;
        deny all;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_pass http://127.0.0.1:8024;
    }

    # Viewers: real user authentication.
    location / {
        auth_basic           "Clarus";
        auth_basic_user_file /etc/nginx/.htpasswd;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Authenticated-User $remote_user;
        proxy_pass http://127.0.0.1:8024;
    }
}
```

The example assumes you already have a certificate and key at those paths.
For a public deployment, get one from Let's Encrypt (`certbot`) or from your
hospital's internal CA. For an internal-only PACS, a private CA or self-signed
certificate is enough, since only your scanners and viewers need to trust it.

`proxy_set_header X-Real-IP $remote_addr;` is mandatory in every location that
proxies to Clarus. Without it the server sees only the socket peer - nginx
itself, on loopback - so every remote client looks local and every address
policy is bypassed. This is the most common way to get the setup wrong.

The split above is the whole access model: scanners are identified by network
address because they have no user identity, viewers by a real login. Keep
`/health` and `/metrics` open on 8024 - the supervisor and Prometheus need
them without credentials. If you proxy them too, do not put `auth_basic` on
those two locations. Either way, bind 8024 to loopback or a private network
and never expose it directly to the internet; only nginx should face outside.

nginx's own address must be in `trusted_proxies` (default
`127.0.0.0/8, ::1/128` covers a same-host nginx). If nginx runs elsewhere or
in a container, add its address, for example
`trusted_proxies = 127.0.0.0/8, ::1/128, 172.18.0.1`.

## Access control

Four mechanisms, each answering a different question.

`trusted_proxies` - which addresses are allowed to tell Clarus who the client
is. Default: loopback only. Add your proxy here or the identity headers are
ignored.

`require_proxy_identity` - default `false`. When `true`, a trusted proxy that
forwards proxy headers but no `X-Authenticated-User` is denied with `502`. A
trusted peer sending no proxy headers at all (local `curl`, the embedded
viewer) is still allowed. Honest boundary: the server sees only whether an
identity header arrived, never whether nginx enforced `auth_basic`. If nginx
sets the header unconditionally, Clarus believes it. Authentication is only
as strong as your nginx config.

`anyclass_enable` - default `false`. The opt-in for IOCM rejection notes that
hide **diagnostic** images (`113037`, `113038`, `113039`). Quality rejections
(`113001`) are not affected by it: they can only hide AI results, so they
always work. See "Rejections that hide diagnostic images" for when it is safe
to turn on.

`admin_cidrs` - the administrative surface. Absent, the default is the local
machine only (`127.0.0.1`, `::1`) and everything else gets `403`. This covers
`/admin/v1/*` and `/dicomweb/patients`, the patient registry the embedded
viewer reads. The viewer page stays reachable from everywhere; only its data
requests are gated. On a flat clinic LAN, do not set `admin_cidrs` to that
whole subnet - every computer on it could delete studies and rebuild storage.
Give management its own subnet, or use the tunnel below.

SSH tunnel - the default answer for remote administration, needing no config
on the server at all:

```
ssh -L 8024:127.0.0.1:8024 user@SERVER
```

Then open `http://127.0.0.1:8024/` on your own machine. A remote browser
getting `403` for the patient list is not an error; it is the policy working.
One caveat in Docker: traffic to a published port arrives from the Docker
bridge gateway (for example `172.18.0.1`), not from `127.0.0.1`, so the tunnel
above still gets `403`. Reach those endpoints with
`docker exec clarus curl ...`, or set `admin_cidrs = 172.18.0.0/16`, or put
nginx in front.

### Reviewing rejected AI results (quality assurance)

When an AI result is rejected - for example a poor-quality segmentation
reviewed and rejected by a radiologist - Clarus keeps it but stops showing it.
It disappears from searches and a direct link answers `404`, exactly like an
instance that never existed. This is what the rejecting user intended: the
result is out of the clinical view.

Quality assurance still needs to inspect those results, so there is a separate,
deliberately narrow way back in: the server publishes a **second DICOMweb
endpoint** for QA, at the base URL `/dicomweb_qa`. It answers the same
resources as the ordinary endpoint, read-only, and includes instances hidden by
a Quality rejection.

It does **not** reveal instances hidden for patient safety or an incorrect
worklist entry - those stay hidden everywhere (see "Rejections that hide
diagnostic images" below).

To use it from a DICOMweb viewer, point that viewer at the QA base URL instead:

```
http://SERVER:8024/dicomweb_qa
```

This is a separate endpoint rather than a flag on the ordinary one, so a
viewer can be configured for QA without any special quirk, and the ordinary
endpoint stays hidden-by-default for everyone else.

Access to `/dicomweb_qa` is controlled separately from the rest of the
server:

`expose_rejected_cidrs` - which addresses may use the QA endpoint. Default:
empty, which means the local machine only. Everything else is refused with
`403`. This is **not** `admin_cidrs` and the two are not interchangeable:
`admin_cidrs` grants the power to change the server, while this grants only the
ability to read results that are hidden from everyone else. A QA workstation
needs the second and should not have the first.

From an address outside the list the QA endpoint refuses the request - a
deliberate choice, so that an auditor can tell "you are not allowed" apart from
"that instance is gone".

**DIMSE viewers** (workstations that query and retrieve over DIMSE rather than
DICOMweb) have no way to point at a different base URL per query, so the bridge
offers the same capability as a **second AE Title** whose queries go to the QA
endpoint. In `clbridge.conf`:

```
[server]
expose_port = 8106
expose_ae_title = CLBRIDGE_QA

[dicomweb]
qa_url = http://127.0.0.1:8024/dicomweb_qa
```

`qa_url` is the QA base URL from above. Point the QA workstation at the
`CLBRIDGE_QA` AE and its port; the ordinary `CLBRIDGE` AE keeps querying the
normal endpoint and keeps hiding rejected results.

**Both keys are required.** With either one missing the bridge logs a warning
and does not start the QA AE - a deliberately loud failure, because an AE that
silently returned the ordinary (hidden) view to a QA user would be worse than
no QA AE at all. Leaving both unset - the default - keeps the bridge as a
single-AE gateway.

If the QA endpoint is access-controlled, the bridge's address must also be in
the server's `expose_rejected_cidrs`. In a co-located deployment (both on one
host) loopback already satisfies this, so only a bridge on a separate machine
needs the entry.

Two related facts worth knowing when planning a deployment:

- Rejected instances are still counted in storage and still backed up. They
  are hidden from view, not deleted.
- There is **no standard way to un-reject** an instance. DICOM and IHE define
  how to hide, not how to restore, and Clarus does not adopt a vendor-private
  restore code. Treat a rejection as permanent when planning: the only paths
  back are the QA endpoint (for Quality rejections) and the administrative
  restore API (for any of them) described below.

### Restoring a sequestered instance (administrator)

Because no standard un-reject exists, Clarus adds one as an explicit
administrative extension. It is reachable only from the networks in
`[server] admin_cidrs`, never through `/dicomweb` or `/dicomweb_qa`, and every
call writes an audit row:

| Request | Purpose |
| ------- | ------- |
| `GET /admin/v1/instances/hidden` | What is currently sequestered, with the reason code that hid each instance |
| `GET /admin/v1/instances/raw?sop_instance_uid=...` | The stored bytes of one instance, hidden or not |
| `POST /admin/v1/instances/restore` | Clear the sequester |

The list takes `study_uid`, `series_uid`, `sop_instance_uid` and `reason` as
optional filters and pages with `limit` (default 100, maximum 500) and
`offset`. Filtering by `reason=113038` answers "what did the last worklist
correction hide":

```
curl "http://127.0.0.1:8024/admin/v1/instances/hidden?reason=113038"
```

A restore takes up to 500 SOP Instance UIDs and a **mandatory** reason:

```
curl -X POST -H "Content-Type: application/json" \
  -d '{"sop_instance_uids":["1.2.840.10008.5.1.4.1.1.88.22.3"],
       "reason":"worklist corrected, images belong to this patient"}' \
  http://127.0.0.1:8024/admin/v1/instances/restore
```

The response reports how many instances actually changed and lists any UID
that could not be restored. Restoring is safe to repeat: an instance that is
already visible is reported as "nothing to do" rather than counted.

Two things to understand before using it. The reason is mandatory and is
recorded before the visibility change, so a crash between the two leaves the
intent on disk rather than an unexplained restoration - which is why a restore
without a reason is refused with `400`. And a restored instance is
indistinguishable from one that was never hidden: it becomes visible on the
ordinary base URL, `/dicomweb_qa` stops reporting it, and a later store of it
is accepted again. The rejection note itself stays in the archive as the
historical record of what happened.

Monitor `clarus_restored_total{reason=...}`. `clarus_rejected_instances_total`
is not decremented by a restore, so the number currently hidden is
`clarus_rejected_instances_total - sum(clarus_restored_total)`.

### Rejections that hide diagnostic images

Beyond the quality-review case, the server also understands the three IOCM
reasons that hide images of any type - including diagnostic CT and MR, which a
Quality rejection can never touch:

| Reason | Code | What it means |
|---|---|---|
| Patient safety | `113037` | Wrong patient - the images must not be seen |
| Incorrect worklist entry | `113038` | Acquired against the wrong order |
| Retention policy expired | `113039` | Past the data-retention window |

These are fundamentally different from a Quality rejection: they exist to hide
diagnostic CT and MR, and **no QA endpoint can bring them back** - not even
`/dicomweb_qa`. Reversing one is a deliberate administrative act through
`/admin/v1/instances/restore`, with a recorded reason. They are also permanent
in a second sense: a later store of an instance hidden by `113037` or `113038`
is refused, as the IOCM profile requires, until such a restore clears the
sequester. `113039` is a lifecycle marker and does not block a re-store.

Because a rejection note is just a DICOM object that any client able to reach
STOW can send, these reasons are **off unless you turn them on** explicitly:

```
[server]
anyclass_enable = true
```

Enable it only when the STOW surface is restricted to trusted modalities - an
nginx ACL, or a network only the modalities can reach. On an open STOW
surface, anyone who can store an image can also send a note that hides one.

With it `false` - the default - a `113037`/`113038`/`113039` note is still
stored (it is a valid object) but it hides nothing. The server logs an
`IOCM_ANYCLASS_IGNORED` warning and increments
`clarus_iocm_anyclass_ignored_total{cause="disabled"}`. That counter is the
operator's signal: a persistent non-zero value means clients are sending these
notes and the feature is not enabled, either because you have not decided yet
or because an unauthorised client is trying to hide images.

Quality rejections (`113001`) are unaffected by this setting: they can only
hide AI-result objects, so they are honoured either way.

A hide is recorded in the immutable audit trail as a `reject` event, naming
the operator, the reason code, the study and the note that justified it - so
an image that disappears from every search can always be traced to a decision
and a person.

## Backups

The delete trail is the one thing you cannot reconstruct. `audit_dir` is the
only evidence that a study was deleted and by whom - treat it as the
mandatory backup target, and put it on protected storage.

| What | Key | Why |
| ---- | --- | --- |
| `audit_dir` | `[btree] audit_dir = ./data/audit` | **Mandatory.** Only evidence of deletions. |
| `access_dir` | `[btree] access_dir = ./data/access` | Access journal; who read what. |
| `manifest_dir` | `[btree] manifest_dir = ./data/manifests` | Study/series/instance index. |
| `idx_dir` | `[btree] idx_dir = ./data/idx` | Daily index files, query speed. |
| `storage_dir` | `[storage] storage_dir = ./data` | The image data itself (`root = ./data`). |
| `log_dir` | `[logging] log_dir = ./logs` | Server log. |
| Cold tier | `[storage_tiers] cold_path` | If configured, the older studies. |
| S3 | on demand | If configured, lives outside your host. |

Tools that work: `robocopy` or `rsync` for the file trees, DFS-R if you
already run it. For the data volume prefer snapshots over file copies -
Windows VSS or ZFS give a consistent point-in-time without walking a million
small files. Encrypt the media: BitLocker on Windows, LUKS on Linux.

`access_retention_days = 1095` keeps the access journal for three years by
default. That is a retention floor, not a substitute for backups - a journal
on a failed disk is still gone.

## Weekly maintenance

Run the CAS audit weekly. It verifies that the content-addressed storage
(CAS - files named by the hash of their own contents) still matches the
manifests, and it writes nothing:

```
curl -s -X POST -H "Content-Type: application/json" \
  -d '{"sources":["hot"]}' \
  "http://127.0.0.1:8024/admin/v1/recovery/rebuild?dry_run"
```

Schedule it from cron on Linux:

```
0 3 * * 0  curl -s -X POST -H "Content-Type: application/json" -d '{"sources":["hot"]}' "http://127.0.0.1:8024/admin/v1/recovery/rebuild?dry_run"
```

On Windows, `schtasks` does the same:

```
schtasks /create /tn "ClarusWeeklyScan" /tr "curl.exe -s -X POST -H \"Content-Type: application/json\" -d \"{\\\"sources\\\":[\\\"hot\\\"]}\" \"http://127.0.0.1:8024/admin/v1/recovery/rebuild?dry_run\"" /sc weekly /d SUN /st 03:00
```

List only the tiers you have actually configured in `sources`.

Antivirus exclusions. Real-time scanning of an ingest directory is expensive:
every stored instance creates two or three new files. Measured on a
1063-instance, 987 MB corpus over loopback, excluding the data directory took
batches from 5.0-7.1 s to 1.3-1.5 s, and the worst batch from 59.7 s to
0.17 s. Exclude `storage_dir`, the `[btree]` root (if it differs), and the
bridge `outbox_dir`. WADO reads (retrieving images for a viewer) are not
scanned and need no exclusion.

```
Add-MpPreference -ExclusionPath "D:\\path\\to\\data"
Add-MpPreference -ExclusionPath "D:\\path\\to\\clbridge\\outbox"
```

Firewall: open 8024 to nginx only, 8104 to the scanner subnet only, and leave
8105 on loopback. With the tunnel model, 8024 need not be open at all.

Logs live in `log_dir` (default `./logs`) for the server and
`[logging] log_file` (`./logs/bridge.log`) for the bridge. Retention knobs
worth reviewing: `[audit] access_retention_days = 1095`,
`[ups] final_state_retention_days = 7`, `[commitment] result_retention_days = 30`,
and `[workflow] retention_days = 7` if you enable the workflow surface.

## Storage tiers: hot, cold, S3

Three tiers, used in that order. Hot is `storage_dir`. Cold is a mounted
volume - NAS or a big local disk - configured in `[storage_tiers]`:

```
cold_path = /mnt/nas/archive
cold_schedule = 0 */6 * * *
cold_min_age_days = 90
```

The schedule is a cron expression; the example moves studies older than 90
days every six hours. Set the cold path in one place only - `[storage_tiers]`
is the canonical location, and the `[storage]` copy exists so the free-space
guard can find the same volume.

Emergency migration is the safety valve: when free space drops below
`emergency_free_gb`, the server migrates oldest-first to free space without
waiting for the schedule. Default `0` means off. Turn it on if you would
rather lose tiering policy than lose the ability to accept a study.

S3 is a third tier and is **on demand only**: there is no `[s3]` section in
the default config, and using one requires a build with `--features s3`. Reads
fall back transparently - hot, then `cold_path`, then S3 - so a viewer never
needs to know which tier a study sits on. Configure it in `[s3]` if you need
it; the block in the config documents every key, including the
`127.0.0.1`-not-`localhost` warning that saves you ten seconds on every first
request.

Related knobs: `max_instance_size_mb = 16384`, and the free-space floor
`min_free_disk_mb = 5000` / `min_free_disk_pct = 5.0`.

## MWL/MPPS (workflow)

Modality Worklist (the list of scheduled procedures a scanner pulls) and
MPPS - Modality Performed Procedure Step, the scanner reporting back what it
actually did - are served over DICOMweb and are **off by default**. A
deployment that has not opted in answers `404` for these two resources.

Enable it in `[workflow]`:

```
[workflow]
enabled = true
retention_days = 7
mwl_default_limit = 100
mwl_max_responses = 200
```

`retention_days` is how long a worklist row and a finished MPPS row are kept;
an in-progress step is never swept, because a step that never finished is a
question for the operator, not for retention. `0` keeps records forever.
`mwl_default_limit` is the response size when the client sends no limit, and
`mwl_max_responses` is the hard ceiling that stops one request pulling a whole
site's worklist in a single answer. Records are mutable and short-lived, so
they live under the workflow directory, not in the immutable archive.

## Optional resilience

Both features default to off. Turn them on deliberately.

**Forward to a second archive.** A best-effort DIMSE mirror: every study
accepted here is also sent as a C-STORE to another archive.

```
[forward]
enabled = true
dest_ae = REGIONAL_ARCHIVE
dest_host = 192.168.1.20
dest_port = 104
forward_max_queue = 20000
```

It never blocks acquisition. Deliveries are queued, and when the destination
is down the queue caps at `forward_max_queue` and drops the newest entries
rather than growing without bound or stalling the scanner. Treat the second
archive as a convenience copy, not a replica: there is no reconciliation
after a long outage.

**Store and forward.** Keep accepting studies while Clarus itself is down:

```
[outbox]
store_forward = true
```

With this on, the bridge accepts the C-STORE, keeps the study on disk under
`outbox_dir`, and delivers it when the server comes back. With it off (the
default), the C-STORE is rejected while Clarus is down and the sending device
must retry later. Disk-full protection stays active in both modes - it will
not fill your disk to honour a promise. `max_retries = 10` bounds how long a
queued study keeps being retried before it lands in `undelivered/` for manual
review.

## Docker alternative

If you would rather not install anything on the host:

```
docker compose up -d --build
```

The stack builds Clarus with the transcode feature, publishes 8024 and 8104,
keeps data in the `clarus-data` and `bridge-outbox` volumes, and health-checks
via `/health/status`. Inside the container `/data` holds the archive and index
root, `/data/logs` the logs. For a dedicated PACS disk, replace the named
volume with a bind mount - otherwise the free-disk percentage reports the
host's Docker data root.

Published ports arrive from the Docker bridge gateway, not from `127.0.0.1`.
The shipped `docker/clarus.conf` already sets `admin_cidrs = 172.19.0.0/16`
to cover that gateway, but compose picks a subnet automatically, so if the
patient list returns `403`, find the real subnet with
`docker network inspect` and update `admin_cidrs` to match.

## Where to read more

- `clarus.conf` and `clbridge/clbridge.conf` - the shipped configs, every key
  commented. Start here for anything this page skips.
- `README.md` - project overview and the current build state.
- `dicomweb-conformance-statement.md` and `aiw-i-conformance-statement.md` -
  exactly which DICOMweb transactions are implemented and which are not.
  Read these before promising a viewer that a feature exists.
- `interop-comparison.md` - how Clarus behaves against other clients in
  practice.
- `docs/bridge.md` - the DIMSE bridge in depth.
- `public-docs/` - the rest of the public documentation set, including the
  admin API surface in `public-docs/admin-api.md`.
- `docs/nginx-dicomweb.conf.example` - a fuller nginx example to adapt.
