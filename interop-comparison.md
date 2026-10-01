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
