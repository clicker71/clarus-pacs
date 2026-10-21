# Clarus audit log format

Clarus PACS writes two append-only JSON-lines journals under the data root.
This document is the normative description of the file format and of the
event classification, for anyone who must parse, forward, or audit the logs:
SIEM operators, auditors, DICOMweb tooling authors.

The journal is "ATNA-lite": event IDs are the DICOM PS3.15 audit-message
codes, and action codes follow GOST R ISO 27789-2016. Clarus does NOT claim
DICOM PS3.15 Annex A.5 (ATNA) conformance. The journals are the only audit
output, and their JSON-lines format is the contract - not the DICOM XML
audit message.

## 1. The two journals

| Journal | Directory | Content | Durability | Retention |
|---|---|---|---|---|
| Delete trail | `audit/YYYYMMDD.log` | destructive admin actions (delete, delete_preview, erase_complete, permission anomalies) | fsync per record | never pruned by Clarus |
| Access journal | `access/YYYYMMDD.log` | every access/query event on protected data | batch flush (<= 1 s window) | pruned, default 1095 days |

Directory locations: `audit_dir` and `access_dir` under `[btree]` in
`clarus.conf` (default `./data/audit` and `./data/access`).

One JSON object per line, newline-terminated (LF). Files rotate at UTC
midnight (`YYYYMMDD`); a file for a past day is never rewritten.

## 2. Access journal line schema

Field order is part of the contract (byte-exact). Every line:

```
{"event":"access","ts":"...","user":"...","role":"","src":"clarus-server","src_ip":"...","via":"...","action":"...","atna":"...","status":200,"study":"...","series":"...","sop":"...","patient":"...","obj_type":"...","q":"...","count":1}
```

Attribute-update events (`action` = `"U"`) append `changed_attrs`,
`bytes_replaced`, and optionally `reason` at the end.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | `"access"` (request/query) or `"attribute_update"` (U) |
| `ts` | string | UTC ISO-8601 second resolution, e.g. `2026-08-28T00:17:12Z` |
| `user` | string | effective user: `X-Authenticated-User`, or `(miniviewer)` / `(loopback)` / `anonymous` |
| `role` | string | reserved, empty |
| `src` | string | `"clarus-server"` |
| `src_ip` | string | client IP |
| `via` | string | `"nginx"` \| `"loopback"` \| `"bridge"` |
| `action` | string | GOST 27789 action: `R` read, `C` create, `U` update (`D`/`E` reserved) |
| `atna` | string | DICOM PS3.15 EventID (see section 4) |
| `status` | number | HTTP status of the response |
| `study`, `series`, `sop` | string | Study/Series/SOP Instance UID captured from the path; `""` when absent |
| `patient` | string | PatientID only; `""` when the request carries none |
| `obj_type` | string | ParticipantObjectIDTypeCode: `"110180"` (DICOM UID) or `"2"` (PatientID) or `""` |
| `q` | string | query text, verbatim, capped 1024 bytes; `""` otherwise |
| `count` | number | instances in a STOW batch (1 otherwise) |
| `changed_attrs` | number | U only: count of (0400,0561) items embedded (0 = pixel-only replacement) |
| `bytes_replaced` | number | U only: 1 when the stored bytes were replaced, else 0 |
| `reason` | string | U only: change reason (`"coerce_insert"`, `"hidden_phi_excised"`, or `""`) |

Minimization: identifiers only - user, IPs, UIDs, PatientID, query text.
Never names, never clinical content, never pixel metadata.

## 3. Event classification

The access journal maps each route to `(action, EventID, obj_type)`.
Deletes and erases are not here; they live in the delete trail.

| Route family | action | EventID (name) | obj_type |
|---|---|---|---|
| WADO instance / uri / frame / thumbnail / bulkdata / series / study / metadata / rendered | R | 110103 DICOM Instances Accessed | 110180 |
| QIDO patients | R | 110110 Patient Record | "" |
| QIDO studies / series / instances | R | 110112 Query | "" |
| MWL search | R | 110112 Query | "" |
| STOW | C | 110104 DICOM Instances Transferred | "" |
| MPPS create | C | 110111 Procedure Record | 110180 |
| MPPS update | U | 110111 Procedure Record | 110180 |
| MPPS retrieve | R | 110111 Procedure Record | 110180 |
| admin audit-log read (self-audit) | R | 110101 Audit Log Used | "" |
| admin access-log read (self-audit) | R | 110101 Audit Log Used | "" |
| attribute update (in-place replace) | U | 110103 DICOM Instances Accessed | 2 (PatientID) |

## 4. Code tables

ATNA EventID (DICOM PS3.15):

- `110101` Audit Log Used
- `110103` DICOM Instances Accessed
- `110104` DICOM Instances Transferred
- `110110` Patient Record
- `110111` Procedure Record
- `110112` Query

GOST R ISO 27789-2016 action codes: `C` create, `R` read, `U` update,
`D` delete, `E` execute.

ParticipantObjectIDTypeCode in use:

- `"2"` - patient identifier
- `"110180"` - DICOM Study/Series/SOP Instance UID
- `""` - no participant object

## 5. Documented GOST vs ATNA divergence

The action code for Query events (QIDO and MWL) is `R` - GOST 27789
"read of the registry". DICOM PS3.15 mandates EventActionCode `E`
(execute) for the Query event (110112). This is a deliberate GOST
alignment; Clarus does not claim ATNA conformance and does not remap it.

## 6. Syslog / ATNA XML forwarding

A reference converter (in the Clarus repository, `tools/`) reads
`access/YYYYMMDD.log` and renders one PS3.15 A.5-shaped XML AuditMessage
per line, framed as an RFC 5424 syslog message and forwarded over UDP or
TCP (RFC 6587 framing). It is an integration aid and a reference
implementation of this format; it does not make Clarus ATNA-conformant and
does not change the journal's retention or loss semantics.

## 7. Delete trail (audit/)

One JSON line per destructive event: `delete`, `delete_preview`,
`erase_complete`, `reject`, `restore`, `security_perm_fix`. Identity fields:
`ts`, `user`, target UID (study/series/instance), `patient_id`, `count`,
`bytes_to_free`, `reason` (free text). The delete trail is the only evidence
of deletions; back it up off-box.

Two of these record the two halves of a clinical sequester, and they are
deliberately not symmetric in shape:

| Event | Fields | Written |
| ----- | ------ | ------- |
| `reject` | `user`, `reason_code`, `study`, `note_uid`, `count`, `ts` | before the instances are hidden |
| `restore` | `batch_id`, `user`, `reason`, `reason_code`, `target`, `ts`, `patient_id`, `count` | before the instances become visible again |

`reject` is one row per (study, reason) group, not per instance: a Key Object
Selection note may name thousands of instances, and the referenced UIDs stay
queryable through the still-stored note. Its `count` is therefore the number
of instances the note NAMED in that study - the request's intent - while the
number actually flipped is carried by
`clarus_rejected_instances_total{reason=...}`. `note_uid` is the SOP Instance
UID of the KOS note that carried the rejection; it is omitted when the batch
did not record one, rather than written as an empty string.

`reason_code` on a `reject` row is the code applied (`113037` patient safety,
`113038` incorrect worklist entry, `113039` retention expired). On a
`restore` row it is the code being CLEARED. An auditor reading the trail for
one instance therefore sees which clinical decision was made, and later
whether and why it was reversed.

## 8. Operating notes

- Unix: `audit/` and `access/` directories are enforced 0700 at startup,
  files 0600; a permission anomaly is itself recorded
  (`security_perm_fix`). Windows: NTFS ACLs (see `clarus.conf`).
- The access journal is bounded-loss by design (at most the last ~1 s
  unflushed batch can be lost on a crash); the delete trail is
  fsync-per-record.
- PHI in query text: PatientName-family values are masked to `***`;
  PatientID and AccessionNumber stay visible.
- Self-audit: reading either journal through the admin API
  (`GET /admin/v1/audit-log`, `GET /admin/v1/access-log`) writes a 110101
  row into the access journal.
