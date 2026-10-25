# Contributing to Clarus

Thank you for your interest in Clarus! We value your time and expertise.

> **Note on this public repository.** The documentation, conformance
> statements, demos, and release binaries live here. A few components
> referenced below - the DIMSE bridge (`clbridge`), the anonymizer
> (`cltranscode`), `docs/unsafe-audit.md`, and the `scripts/` probe
> helpers - are part of the closed preview: they ship with the release
> binaries and will be published with the source under LGPL-3.0 when the
> preview ends.

## Our Policy on Code Contributions

We genuinely want your bug reports, logs, DICOMweb conformance findings, and feedback — they make the project better, and confirmed reporters are credited in the release notes.

We do not accept **external code contributions** (pull requests from third parties) at this time. This is a temporary policy while we finalize the contribution licensing (CLA) and the chain-of-ownership process our regulated, certified deployments require. Confirmed issues are fixed by the maintainers and land through regular pull requests, so a reported bug is closed with a PR link you can follow.

The project is licensed under LGPL-3.0-or-later: you are free to use, modify, and distribute it under the LGPL terms, and nothing in this document restricts that.

The reasons below are what this policy protects. The first three are specific to the Rust core; the fourth is the stack philosophy. They apply to the entire repository, including the Python DIMSE bridge (`clbridge`).

### Why

1. **Strict `unsafe` discipline.** `unsafe` is allowed only at the OS boundary (syscalls, FFI, `memmap2`) — no pointer arithmetic, no manual memory management, no `unsafe` in business logic, ever. Crate roots are under `deny(unsafe_code)`; the only exceptions are explicit `allow` markers on the audited OS-boundary sites, so new `unsafe` outside the audit is a compile error. Every `unsafe` block carries a `SAFETY:` comment, and every unsafe site is audited and documented in [`docs/unsafe-audit.md`](docs/unsafe-audit.md). Auditing external code to this standard takes longer than writing it ourselves — a statement about our review process, not about the quality of outside code.

2. **No-allocation hot paths.** The query core is proven allocation-free under `TestAllocator` (QIDO study scan), and the rest of the code follows a strict no-heap-in-hot-path policy. One accidental allocation in a hot loop destroys P99 latency, and verifying that on external code is not feasible at review time. This discipline is not home-grown folklore — it comes from our [AP-101B toolkit](https://github.com/clicker71/ap101) (zero-heap enforcer, cache-line audit of hot records, SEU fault injection), which also powers the `ap101` gates inside this repository.

3. **Performance-first builds.** We build release binaries with `-O3` plus LTO and `codegen-units=1`; this profile is benchmarked (criterion) and is part of the acceptance gates. External code would need the same scrutiny to keep the numbers honest.

4. **A deliberately thin stack.** We practice a strict crate diet: the core keeps only a handful of tiny, audited dependencies, and there is no embedded SQL engine at all — the filesystem itself is our database (CBOR manifests and B-tree indexes), at the OS level. The server runs without an async runtime of our own: thread-per-connection hands scheduling to the OS kernel. None of this means outside code is bad — it means these invariants are expensive to verify in code we did not write, and we are protective of our hot path, not suspicious of your code. Put plainly: we can answer for code we wrote; we cannot answer for code we did not write, so the crate diet keeps every dependency we ship small enough to have been read and vouched for.

5. **The cost of verification is not zero.** Every change to the hot path — QIDO, WADO, STOW, UPS, the CAS writer — triggers the full gate cascade: `ap101` zero-alloc and cache-line gates, codec parity (424/424 byte-identical), criterion benchmarks on the affected paths, the DIMSE wire regression tests, and the conformance statements must still hold. That is roughly **1,500 tests** and **2–3 hours of compute and review per change**, and it is not optional. It is the reason the engine has not regressed in the paths that matter.

   External code would need the same scrutiny. A well-written patch that "looks correct" is not enough — it must survive the gates, and the gates take hours. We cannot run that cost for every contributor, and we will not run it for code we did not write ourselves. This is not a judgment on your code. It is a statement about our budget.

6. **We follow principles, not preferences.** Read our README. Seven principles govern this project. Three of them matter here:

   - **No optimisation without a measurement (Pike).** A change that "looks faster" is not a change. It is a hypothesis. We do not merge hypotheses. Example, measured 2026-10: replacing the `png` crate's DEFLATE chain with a hand-rolled stored-block encoder was a hypothesis until the number came in — `/rendered` 512x512 RGB8 incompressible noise (N=30) went from 87.1 ms to 49.2 ms median, wire size unchanged. The number made it a change; no number, no merge.
   - **The fastest code never executes (Galanakis).** The best way to make the hot path faster is to remove work from it, not to add code. External code adds work — review, audit, gates, maintenance. It is the opposite of the principle.
   - **Simplicity is when you understand the system (Thompson).** We understand this code because we wrote it, line by line, and because it is small enough to hold in one head.

   **Nobody sharpens `nano` or `scp` with a file.** They work. They do one thing. They do it well. And they have not changed in decades because the problem they solve has not changed. `nano` and `scp` are not "closed" — they are **finished**. There is nothing to add that would not make them worse.

   This engine is the same. DICOM PS3.18 is a stable standard. The problems it solves — store, query, retrieve, commit — are stable problems. The code that solves them does not need a new feature every month. It needs to keep working.

   **"Don't touch the equipment. Don't interfere with the work."** That is the first rule of a service engineer. The Clarus team includes engineers who spent years in the field, assembling and repairing X-ray and CT machines. A machine that works is not a machine that needs your improvement. A machine that works is a machine that **works**. You do not open it because you have a better idea. You open it because it is broken, and you close it the moment it works again.

   The same rule applies to this code. We do not accept external code because the code is not broken. When it is broken — a real bug, a real non-conformance, a real crash on real hardware — we fix it ourselves, and we fix it fast (typically within 5 business days for critical bugs). But we do not open the machine because someone has a better idea.

**Commercial and regulatory context.** Clarus is commercially backed and intended for regulated, certified environments (medical imaging, audit trails, certified deployments). The exclusive rights to the codebase are held by a single commercial rights holder, and every line must keep a clean, auditable chain of ownership — a requirement that external contributions, however well-written, would complicate. This is not a judgment on open-source; it is a structural necessity for the markets we serve.

## How You Can Help

Your contributions in other — often more valuable — forms are very welcome. By submitting bug reports, logs, or test data, you agree that we may use these materials to improve the software.

### 1. Test on Real Data

Run Clarus against your DICOM archives and report what you find.

Before sharing logs, DICOM headers, or `.dcm` files in a public issue, make sure the material is fully anonymized per your local regulations (e.g. 152-FZ "On Personal Data", GDPR, HIPAA):

- **Patient names:** our own logs mask names automatically (e.g. `Иванов → И***в`), but logs you collect yourself may not — redact them.
- **Medical UIDs (Study/Series/SOP Instance):** treat them as personal data. For public issues, replace real UIDs with synthetic, well-formed test values before pasting — keep the structure intact so we can reproduce.
- **Never upload raw clinical images** containing burned-in patient text or pixel data of real patients.

Include the relevant log excerpt in your report.

DIMSE devices (modalities, workstations): point them at the bridge and send us the association and command logs. Captures from real devices are the only way we can validate protocol corners — this is the kind of contribution we value most.

### 2. Test DIMSE Devices Against the Bridge

DICOM conformance varies wildly between vendors — DIMSE interoperability is where decades of quirks live: association negotiation, unusual transfer syntaxes, partial C-FIND responses, storage commitment semantics. `pynetdicom` carries the protocol basics for us, but every real device adds its own layer of bugs, and we cannot buy every device. Your hardware is the test bench we do not have.

What to do:

1. Run the bridge: `pip install pynetdicom pydicom requests`, download the latest release, edit `clbridge.conf`, run `python clbridge.py`.
2. Point your device at AE Title `CLBRIDGE`, port `8104` (or as configured in `clbridge.conf`).
3. Exercise the workflows: C-ECHO, C-STORE, C-FIND, C-MOVE, N-ACTION (storage commitment).
4. Open an Issue titled `[DIMSE] <Vendor> <Device>` with:
   - The bridge log excerpt (names are masked, UIDs remain).
   - Device vendor, model, and software version.
   - What you sent, what the device answered, and what you expected.

We fix the protocol corner case, add the transfer syntax, or extend vendor-specific handling — and the reporter is credited in the release notes.

### 3. Help Write DICOM Print Drivers (C-PRINT)

DICOM Print runs over DIMSE (N-CREATE/N-SET/N-ACTION), so it lives in the bridge. The generic driver (PS3.4 §H) is implemented in `clbridge/drivers/generic_dicom.py` and works with any standard printer. Vendor-specific handling (film sizes, LUTs, DPI presets, media IDs) needs a diagnostic dump from the real device — that is where we need you (Fujifilm, Carestream, Agfa, Konica Minolta, Codonics, …). No coding required — about 10 minutes.

What to do:

1. Note the printer's IP address and AE Title (port is usually 104).
2. Run our probe script against it:
   - Windows PowerShell: `irm https://raw.githubusercontent.com/clicker71/clarus-pacs/main/scripts/cprint-probe.ps1 | iex -args PRINTER_IP PRINTER_PORT`
   - Linux/macOS: `curl -sL https://raw.githubusercontent.com/clicker71/clarus-pacs/main/scripts/cprint-probe.sh | bash -s PRINTER_IP PRINTER_PORT`
   - Manual alternative with DCMTK: `echoscu -d PRINTER_IP PRINTER_PORT` to verify connectivity, then `dcmprintscu --dump PRINTER_IP PRINTER_PORT` to query capabilities (film sizes, layouts, media types, DPI presets).
3. Open an Issue titled `[C-PRINT] <Vendor> <Model>` and paste the probe output.

What happens next:

- We study the dump and write the driver ourselves — all driver code is written by the Clarus team to keep the copyright chain clean.
- You test the driver against your printer.
- Testers are credited in the release notes.

### 4. Report DICOMweb Non-conformance

If a response deviates from PS3.18, open an Issue with:

- The exact request (a copy-pasteable `curl` command).
- The exact response you received.
- The behavior PS3.18 requires (quote the section).

### 5. Report Bugs

Open an Issue with:

- Clarus version: `clarus --version` (short form: `clarus -v`).
- Operating system and hardware (e.g. Raspberry Pi 5, x86_64).
- Steps to reproduce.
- Log excerpts (they are already anonymized, see above).
- Expected vs actual behavior.

For configuration problems, `clarus --test-config` (short form: `clarus -t`) validates `clarus.conf` and reports the offending line — include its output.

### 6. Inspecting DICOM Files

If a bug depends on the contents of a particular DICOM file, do not send the file or a full dump — it may contain PHI.

Use DCMTK's `dcmdump` (the reference implementation's dump tool — we deliberately do not reinvent it):

- Download: <https://dicom.offis.de/dcmtk.php.en>
- Debian/Ubuntu: `apt install dcmtk`
- macOS: `brew install dcmtk`

Run `dcmdump file.dcm` to inspect the metadata. If you need to filter specific tags, `dcmdump` supports options like `--search` — see the dcmdump manual.

Do not attach the raw DICOM file or the full `dcmdump` output. Attach only the lines relevant to the issue.

### 7. Suggest Features

We apply the "do one thing, do it better than anyone" philosophy:

- If a problem is already solved well by an external tool (nginx, DCMTK, the OS kernel), we delegate to it.
- If it is a DICOMweb-specific problem we can solve better than anyone, we will consider it.

We reply to every suggestion — if we decline, we explain why and point to alternatives.

### 8. Improve Documentation

Found a typo or an unclear section? Open an Issue. We will fix it.

## Process

1. **Search existing Issues** — the problem may already be reported.
2. **Open a new Issue** with a descriptive title and the checklists above.
3. We aim to triage within **48 hours** — usually faster.
4. Critical bugs (data loss, security, standards non-conformance) are our top priority and are typically fixed within **5 business days**. This is a goal, not an SLA.
5. Contributors who report confirmed bugs are credited in the release notes.

## Privacy

Never send patient data. Our server logs are anonymized, but logs you collect yourself (proxy logs, client logs) may not be — redact names and identifying attributes before posting.

If you must share a DICOM file, anonymize it first: Clarus includes a PS3.15 Basic Profile anonymizer (5 profiles) in `cltranscode`, or use DCMTK's `dcmodify`.

## Thank You

Your testing, validation, and reporting make Clarus better for everyone.
