# Security Policy

## Supported versions

- The latest stable release receives security fixes.
- One LTS line per year receives backported security fixes for its
  support period (see docs/oem/versioning-policy.md).
- PATCH releases contain security fixes only: DICOM behavior, wire
  format, config keys and storage layout never change in a PATCH.

## Reporting a vulnerability

DO NOT open a public issue for a security problem. Use the private
channel:

- Email: security@pragmaonce.pro (PGP key published on the release
  page when available)
- Or GitHub private vulnerability reporting (Security tab ->
  "Report a vulnerability"), if enabled for this repository.

What to include:

- affected component (core / bridge / cltranscode) and version;
- a minimal reproduction or a precise description;
- impact assessment (what an attacker gains);
- whether you plan to disclose publicly, and when.

What happens next:

- acknowledgement within 72 hours;
- triage and confirmation within 7 days;
- fix in the next PATCH release, backported to the supported LTS
  line;
- coordinated public disclosure: 90 days after the fix ships, or
  earlier by mutual agreement; a CVE is requested when warranted;
- reporters are credited in the release notes (or kept anonymous on
  request).

## Scope

In scope: the Clarus DICOMweb server (clarus), the DIMSE bridge
(clbridge), cltranscode, and the configuration surface they expose
over the network.

Out of scope: deployments running default credentials on an open
network, issues caused by third-party dependencies (reported to their
upstreams), OS-level and hardware issues, and anything documented as
trusted-LAN-only in docs/security.md.

## Notes

- Clarus is experimental software, NOT a registered medical device
  (see README).
- No bug bounty program is offered at this time.
- This repository does not accept external code contributions (see
  CONTRIBUTING.md); security reports are the exception and are
  welcome through the channel above.

---

RU (кратко): об уязвимостях сообщать ТОЛЬКО на
security@pragmaonce.pro или через приватный канал GitHub
(Security -> Report a vulnerability), НЕ публичным issue.
Подтверждение -- 72 часа, фикс -- в ближайшем PATCH с бэкпортом в
LTS-линию, скоординированное раскрытие -- 90 дней после фикса.
