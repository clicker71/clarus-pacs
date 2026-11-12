# cltranscode — parked findings (25.09.2026 principles audit)

Source: `analysis/2509_architect_principles_audit_REPORT.md` (findings F1, F7;
cross-refs to F6). **No action until the owner decides.** This file is the
backlog; the report remains the canonical fact base.

## F1 — dead-code cluster (rule 4: dead code under the knife)

All entries re-verified against the source on 25.09.2026:

| Item | Location | Note |
|------|----------|------|
| `fn has_undefined_length_pixeldata` | `src/adapters/dimse_scp.rs:581` | `#[allow(dead_code)]`; the audit report called it `has_pixel_data` — the real name is `has_undefined_length_pixeldata` |
| `fn is_compressed_ts_uid` | `src/adapters/dimse_scp.rs:662` | `#[allow(dead_code)]`; used ONLY inside `mod tests` (`:954-972`). Fix = move into `mod tests` (then no `allow`), not deletion |
| `fn convert_explicit_to_implicit` | `src/adapters/dimse_scp.rs:722` | `#[allow(dead_code)]` |
| struct fields `ae_title`, `port` | `src/adapters/dimse_scp.rs:28-31` | `#[allow(dead_code)]` on `DimseScp` |
| `fn build_c_store_rq` | `src/adapters/dimse_scu.rs:414` | `#[allow(dead_code)]` wrapper; re-verified 25.09: the `:185/:558` hits are substrings of `build_c_store_rq_with_id` — truly dead |
| field `profile` (unread) | `src/stages/anonymizer.rs:89-90` | from report, not spot-checked |
| `#[allow(unused_assignments)]` | `src/stages/transcoder.rs:716` | from report, not spot-checked |

Removal is mechanical (rule 4). The three `dimse_scp.rs` helpers duplicate
logic already exercised elsewhere; deleting them should not touch behavior.

## F7 — two DICOM object models in one workspace

- Own parser: `dicom_rs/parser.rs` (3811 lines).
- External stack: `dicom-object` / `dicom-core` 0.10 (see
  `crates/cltranscode/Cargo.toml`), used by the standalone anonymize/
  transcode pipeline.
- Mitigated: cltranscode is excluded from default-members; the external
  stack does not leak into clarus-server builds.

Decision needed: keep both (current) or collapse into one. Not urgent.

## Related findings outside this crate

- F6 giant functions in `crates/clarus-adapters/src/transcode.rs`:
  `transcode_dicom` 241 lines, `update_dicom_metadata` 311 lines.
  No refactor until the owner approves the threshold rule (~200 lines /
  function without written justification).
- F2/F3/F4/F5/F8 (server/clinfer) — see the report and the repo-memory
  cleanup TODO.

## 10.10.2026 — standard-compliance findings (string padding + undefined-length)

Source: `analysis/1010_architect_string_padding_audit_REPORT.md` and
`analysis/1010_architect_undefined_length_audit_REPORT.md`. Parked here:
cltranscode is a separate product and is fixed on its own schedule, not in
the Clarus server sprints. No action taken.

### P1 — inject_transfer_syntax_uid: inverted padding condition (BLOCKER)

- `src/stages/transcoder.rs:372-375`:
  `if val.len().is_multiple_of(2) { val.push(0x00) }`.
  The condition is inverted: it pads even-length TS (making them odd) and
  leaves odd-length TS (the common 17/19-char UIDs) UNPADDED. (0002,0010)
  is written with an odd length, so the next element is read at a wrong
  offset. Affects DIMSE outbound transcode today.
- Fix: `if !val.len().is_multiple_of(2) { val.push(0x00); }`
  (PS3.5 6.2: UI pads NUL to even length).

### P2 — write_dicom_element pads non-CS strings with NUL (LATENT)

- `src/dicom_utils.rs:400-401`:
  `buf.push(if vr == b"CS" { b' ' } else { 0x00 })`.
  PS3.5 6.2: every string VR except UI pads with SPACE. LO/PN/SH/DA/TM/DT
  currently get NUL, e.g. (0400,0563) LO "CLTRANSCODE" (11 chars) gets a NUL.
- Fix: SPACE for all string VRs except UI; NUL only for UI.

### P3 — write_padded_str pads pseudonymized UI with SPACE (LATENT)

- `src/stages/anonymizer.rs:356-363`: pads/truncates to `width` with `b' '`.
  For a UI target, space is not in the UI charset.
- Fix: NUL for UI, SPACE otherwise (pass the VR to the helper, or choose
  the byte at the call site).

### U1 — build_explicit_output: no undefined-length skip (LATENT)

- `src/stages/transcoder.rs:284` (walker at :317, `pos += 8 + vlen` at
  :336/:340, `break` at :346 when `val_end > original_bytes.len()`). An
  undefined-length non-PixelData element (vlen == 0xFFFFFFFF) truncates the
  output; no `group == 0xFFFE` branch.
- Fix: on vlen == 0xFFFFFFFF skip to the item/sequence delimiter
  (reuse the item/delimiter walk already present at :430/:569), and add
  `group == 0xFFFE -> 8 bytes`.
