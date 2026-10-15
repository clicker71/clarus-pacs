# DICOMweb interoperability: Clarus vs dcm4chee-arc

Clarus PACS compared against [dcm4chee-arc](https://github.com/dcm4chee/dcm4chee-arc)
5.35.2, measured 2026-10-01, updated 2026-10-14. Every claim is reproduced by the universal test
tool in this repository, [`tools/dicomweb_interop.py`](tools/dicomweb_interop.py):
the same DICOM corpus is seeded to both servers, then identical requests are
sent to each and the answers are shown side by side.

Run it (Python stdlib only, no config files):

```
python tools/dicomweb_interop.py \
  --a http://127.0.0.1:8024/dicomweb \
  --b http://127.0.0.1:8080/dcm4chee-arc/aets/DCM4CHEE/rs \
  --seed-dir /path/to/corpus --suite all --format text
```

Arguments: `--a`/`--b` server base URLs, `--a-ae`/`--b-ae` AE titles,
`--suite ups|fuzzy|commit|all`, `--seed-dir` (recursive STOW of `*.dcm` to
both), `--patient-name`/`--variants` (fuzzy queries), `--sop-class`/
`--sop-instance` (Storage Commitment references), `--format text|json`,
`--timeout <seconds>`. Exit code 0 = all checks passed, 1 = any FAIL.

## Reproduce

- dcm4chee-arc 5.35.2 (Docker `dcm4che/dcm4chee-arc-psql:5.35.2`), device
  `dcm4chee-arc`, AE title `DCM4CHEE`. UPS-RS is disabled by default and was
  enabled by adding `dcmWebServiceClass: UPS_RS` to the Web Application LDAP
  entry. Fuzzy algorithm = LDAP attribute
  `dcmFuzzyAlgorithmClass: org.dcm4che3.soundex.ESoundex`.
- Clarus on `127.0.0.1:8024`, DICOMweb base `/dicomweb`.
- Corpus: the same set of DICOM files STOWed to both servers (synthetic CT
  series plus CBCT; contains Latin-named and Cyrillic-named patients, so the
  fuzzy contrast covers both scripts).

## PS3.18 conformance (2026-10-14, PS3.18 2026d)

Measured on the same host, PS3.18 2026d, with a 32-assert gate of MANDATORY
PS3.18 facts run as a black box against each origin: Clarus 32/32,
dcm4chee-arc 13/32; a black-box red-team pass over the same surface found no
remaining non-conformance in Clarus. One table for the whole comparison; the
Conformant column names the origin that follows the standard for that row:

| Check | PS3.18 anchor | Clarus | dcm4chee-arc | Conformant |
|---|---|---|---|---|
| Capabilities | 8.9 | WADL | 404 (no transaction) | Clarus |
| QIDO limit=0 | 8.3.4.4.1 | 200 + empty result | 400 | Clarus |
| QIDO truncated page | 8.3.4.4.1 | Warning 299 | no Warning | Clarus |
| QIDO legal matching key (0008,0090) | 10.6.1-5 | accepted | 400 | Clarus |
| Thumbnail default media type | 8.7.4-1 | image/jpeg | image/png | Clarus |
| STOW single-part application/dicom | 8.7.3.1 | 200 | 415 | Clarus |
| Rendered resource | 10.4.3.3.3 | rendering | rendering | both |
| Storage Commitment | 13 | yes | 404 | Clarus |
| MWL | 14 | yes | 404 | Clarus |
| MPPS | 15 | yes | 404 | Clarus |

Notes:

- dcm4chee-arc strictness, not non-conformances: UPS-RS Create validates the
  full CC.2.5-3 Create IOD (an empty 400 on a minimal body) and state/cancel
  requires `?requester=` (PS3.18 11.7.2.1-1 makes it optional).
- The Storage Commitment / MWL / MPPS "404" for dcm4chee-arc means "not on the
  standard DICOMweb resources": it serves commitment over DIMSE and MWL/MPPS
  via proprietary endpoints - see the sections below.

## UPS-RS (PS3.18 Worklist Service)

| Check | Clarus | dcm4chee-arc | Conformant |
|---|---|---|---|
| Enabled by default | yes | no (per-AE config) | both (deployment choice) |
| Create: SOP Common in body | 400 | 400 | both |
| Create: conformant body | 201 + Location | 201 + Location | both |
| Search: required keys | filters | filters | both |
| Search: keyword spelling (PS3.18 8.3.4) | filters | filters | both |
| Search: wrong-case / unknown key | 400 | 400 | both |
| Search: StudyInstanceUID (optional key) | filters | ignored | both (optional key) |
| ChangeState `?requester=` | not required | required | Clarus (11.7.2.1-1: optional) |
| Update query-param name | `TransactionUid` (strict) | both spellings | both |
| Update attribute set | narrow (AIW-I report) | general N-SET | dcm4chee (general N-SET) |

Both origins pass the full Task-Requester cycle (see the Weasis section
below). The diverging rows are deployment or generality differences, not clean
non-conformances; Clarus's narrow Update attribute set is declared in its
conformance statement.

## Weasis as a UPS-RS User Agent (Task Requestor)

Weasis phase 1 (`UpsService` + `$ups:` commands, headless) was driven through
the same five-step requester cycle against both origins by `UpsE2eTest.java`
in the Weasis fork (JUnit, tagged `e2e` and property-gated so CI is untouched;
2026-10-10). Both origins pass the full cycle:

| Step | Clarus | dcm4chee-arc |
|---|---|---|
| Create-if-none (11.4 / RAD-80) | 201 | 201 |
| Search (11.9 / RAD-81) | SCHEDULED + code | SCHEDULED + code |
| Create again - idempotent | same UID, nothing minted | same UID, nothing minted |
| Cancel (11.8) | 202, then CANCELED | 202 (accepted) |
| Search after cancel | CANCELED | no longer SCHEDULED |

The cycle is the Task-Requester surface: Weasis creates and watches, and never
re-addresses or steps a workitem (that is the Task Manager's role). The run
surfaced and closed three defects:

- client: `run` minted no workitem UID and sent a bare `POST /workitems`;
  fixed to `POST /workitems?workitem={uid}` (PS3.18 11.4.1.1).
- Clarus server: Create rejected the PS3.18 F.2 array body (`obj_get`
  understood only the object form); fixed to unwrap the single-dataset array.
- Clarus server: the search control parameters (`includefield`, `fuzzymatching`,
  `emptyvaluematching`, `multiplevaluematching`) were refused although
  PS3.18 Table 8.3.4-1 marks them mandatory for the Origin Server; fixed to
  accept them.

## Fuzzy matching (QIDO-RS PatientName)

| Query, `fuzzymatching=true` | dcm4chee-arc (ESoundex) | Clarus (Levenshtein-1) |
|---|---|---|
| stored name, exact | match | match |
| 1-edit variant | match | match |
| 2-edit variant | match | no match |
| unrelated name, same script | match | no match |
| Latin name vs non-Latin patient | no match | no match |

dcm4chee-arc fuzzy search is phonetic-code matching. The default algorithm is
ESoundex (configurable via `dcmFuzzyAlgorithmClass`; Soundex, ESoundex9,
KPhonetik, Metaphone and Phonem are also shipped). All of them map only the
Latin alphabet, so ANY name in ANY non-Latin script (Cyrillic, Greek, Han/CJK,
Katakana, Arabic, digits) collapses to the same empty code, and any such query
matches every stored non-Latin patient - verified live: a Greek, Han, Katakana,
Arabic or digits-only query each returned both stored Cyrillic patients, while
a Latin query matched only its own Latin patient. Even unrelated same-script
names match. Clarus uses Unicode-aware Levenshtein-1 and distinguishes one edit
from an unrelated name.

Latency (200 iterations per query, median / p99):

| Query | Clarus | dcm4chee-arc |
|---|---|---|
| exact | 10.8 / 30.8 ms | 30.6 / 225.7 ms |
| 1-edit fuzzy | 7.2 / 25.5 ms | 28.7 / 74.4 ms |

## Storage Commitment over DICOMweb (PS3.18 13)

| Check | Clarus | dcm4chee-arc |
|---|---|---|
| POST `/commitment-requests/{uid}` | 200 + verdict | 404 |
| GET `/commitment-requests/{uid}` | 200 + verdict | 404 |

Clarus serves the standard DICOMweb commitment resource (conformant);
dcm4chee-arc answers 404 on it and does commitment over DIMSE instead.

## MWL / MPPS over DICOMweb (PS3.18 14/15)

dcm4chee-arc gates every DICOMweb service behind the Web Application LDAP
attribute `dcmWebServiceClass`: the base `DCM4CHEE` app ships with only
`WADO_RS`/`STOW_RS`/`QIDO_RS`, and the worklist classes are added by versioned
migrations (UPS_RS + MWL_RS in 5.22.0, MPPS_RS in 5.29.1, plus a separate
`WORKLIST` app since 5.29.1). Measured on 5.35.2:

| Resource | PS3.18 resource | Clarus | dcm4chee-arc | Conformant |
|---|---|---|---|---|
| MWL search | `GET /modality-scheduled-procedure-steps` | yes | 404 on `DCM4CHEE`; served at `/mwlitems` on the separate `WORKLIST` app only | Clarus |
| MPPS | `/modality-performed-procedure-steps/{mppsUID}` | yes | 404; served at `/mpps` | Clarus |

dcm4chee-arc does NOT expose the standard PS3.18 14/15 resources: it serves
proprietary QIDO-style `/mwlitems` and `/mpps` endpoints, and MWL-RS lives on
the separate `WORKLIST` web app (`/aets/WORKLIST/rs`), not on `DCM4CHEE`. A
standard DICOMweb MWL/MPPS client gets 404 against a default dcm4chee-arc even
when the service classes are enabled. Clarus serves the standard resources at
the standard paths.

## Note on over-matching

dcm4chee-arc's phonetic fuzzy matching over-matches non-Latin names: every
non-Latin name collapses to one empty code, so a single `fuzzymatching=true`
query in a non-Latin script returns every non-Latin patient in the archive
(Latin-named patients keep distinct codes and are not returned). Clarus'
Unicode-aware Levenshtein-1 does not exhibit this. This is a search-quality
defect with a privacy implication, not an access-control bypass.

## Throughput (container-vs-container, measured 2026-10-07)

Host: VMware VM, 6 vCPU (2 sockets × 3 cores, Intel Xeon E5-2690 v4 @ 2.60 GHz), 32 GB RAM,
storage = two VMware virtual disks on HDD (SAS). Both servers in Docker on
the same host, loopback, identical corpus (631 files / 514 MB / 2 studies /
4 series), each started from empty storage. Measured with
[`tools/ab_test.py`](tools/ab_test.py) (STOW-RS upload, then WADO-RS read).
Both serve the objects from their own local storage - there is no
DICOMweb-to-DIMSE hop on either side, so this is apples-to-apples.

| Phase | Clarus | dcm4chee-arc |
|---|---|---|
| STOW-RS write | 32.1 MB/s | 19.6 MB/s |
| WADO-RS read, sequential (series) | 54.1 MB/s | 50.2 MB/s |
| WADO-RS read, parallel (631 instances, adaptive window) | 50.3 MB/s (window 5, 16.2 ms/instance) | 26.0 MB/s (window 7, 31.4 ms/instance) |

A single bulk series read is I/O-bound and lands on the same ~50 MB/s ceiling
for both. The gap appears on many small parallel requests: Clarus holds the
ceiling (16.2 ms/instance) while dcm4chee-arc drops to 31.4 ms/instance,
consistent with a heavier per-request lookup path (PostgreSQL/LDAP) at
instance granularity.

Where the wall is (and is not):

- The ~50 MB/s read ceiling is **this machine's HDD + Docker Desktop
  loopback** (VM NAT bridge), not the server. Both servers hit the same wall,
  which is why sequential bulk reads are parity.
- On NVMe the same Clarus WADO path is far faster: 344-428 MB/s cold and
  ~1.1 GB/s warm, and 602 MiB/s in a release-profile bench run (measured
  2026-08-26).
- The field Raspberry Pi 5 (Cortex-A76, 2 GB RAM, NVMe over PCIe 2.0 x1)
  measures far higher on the same WADO path: warm ~990 MB/s single stream,
  4.0 GB/s four-stream aggregate; cold 244-356 MB/s single, ~896 MB/s
  aggregate (2026-09-01, bare-metal native build, loopback curl, 636 MB
  corpus). So the field box is NVMe-bound, not HDD-bound as in this VM.

