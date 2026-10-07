# DICOMweb interoperability: Clarus vs dcm4chee-arc

Clarus PACS compared against [dcm4chee-arc](https://github.com/dcm4chee/dcm4chee-arc)
5.35.2, measured 2026-10-01. Every claim is reproduced by the universal test
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

## UPS-RS (PS3.18 Worklist Service)

| Check | Clarus | dcm4chee-arc |
|---|---|---|
| Enabled by default | yes | no (per-AE config) |
| Create: SOP Common in body | 400 | 400 |
| Create: conformant body | 201 | 201 + Location |
| Search: required keys | filters | filters |
| Search: StudyInstanceUID (optional key) | filters | ignored |
| ChangeState `?requester=` | not required | required |
| Update query-param name | `TransactionUid` (strict) | both spellings |
| Update attribute set | narrow (AIW-I report) | general N-SET |

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

## Note on over-matching

dcm4chee-arc's phonetic fuzzy matching over-matches non-Latin names: every
non-Latin name collapses to one empty code, so a single `fuzzymatching=true`
query in a non-Latin script returns every non-Latin patient in the archive
(Latin-named patients keep distinct codes and are not returned). Clarus'
Unicode-aware Levenshtein-1 does not exhibit this. This is a search-quality
defect with a privacy implication, not an access-control bypass.

## Throughput (container-vs-container, measured 2026-10-07)

Both servers run in Docker on the same host, loopback, identical corpus
(631 files / 514 MB / 2 studies / 4 series), each started from empty storage.
Measured with [`tools/ab_test.py`](tools/ab_test.py) (STOW-RS upload, then
WADO-RS read). Both serve the objects from their own local storage, so there
is no DICOMweb-to-DIMSE hop on either side.

| Phase | Clarus | dcm4chee-arc |
|---|---|---|
| STOW-RS write | 32.1 MB/s | 19.6 MB/s |
| WADO-RS read, sequential (series) | 54.1 MB/s | 50.2 MB/s |
| WADO-RS read, parallel (631 instances, adaptive window) | 50.3 MB/s (window 5, 16.2 ms/instance) | 26.0 MB/s (window 7, 31.4 ms/instance) |

## Where the wall is

The point of the read lines above is not the absolute ~50 MB/s - it is that
the two servers land on the *same* ~50 MB/s. That agreement is two different
engines pressing against the ceiling of the bench they were measured on, not a
property of either server.

The bench is a VMware VM whose two disks are virtual disks on spinning HDD,
with Docker Desktop on Windows putting every published port behind a NAT
loopback. A single HDD does 100-200 MB/s sequential in the best case; stack
the Docker NAT and a client writing the download back to the same HDD on top,
and a single stream settles around 50 MB/s. Three observations pin the wall to
the environment rather than to Clarus:

1. Clarus cannot exceed ~50 MB/s even with five concurrent reads (window 5
   gives the same 50.3 MB/s). A server limited by its own code would drift
   below the shared ceiling as load rises; it would not sit exactly on it.
2. The same Clarus WADO path, same code, on real hardware does not see this
   wall: ~990 MB/s warm single-stream and 244-356 MB/s cold on a Raspberry
   Pi 5 (Cortex-A76, 2 GB RAM, NVMe over PCIe 2.0 x1, bare metal, 2026-09-01),
   and 344-428 MB/s cold / ~1.1 GB/s warm on an Intel i7 + NVMe box
   ([`benchmark-headroom.md`](benchmark-headroom.md)).
3. Where the engines actually differ, the gap is structural: 1.64x on write,
   and ~2x lower per-instance read latency (16.2 vs 31.4 ms). That is the
   difference between a server with no SQL transaction on its write path and
   no per-request database lookup on its read path, and a server with both.

In short: read parity on this page is a property of the measuring environment.
The numbers that carry information about the two engines are the write
throughput and the per-instance read latency.
