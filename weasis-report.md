# Weasis DICOM® Export (STOW-RS): field findings against a DICOMweb™ PACS

**Date:** 2026-08-22
**Client:** Weasis 4.7.2 (Windows, bundled JRE 26.0.1)
**Server:** Clarus PACS (DICOMweb: QIDO-RS / WADO-RS / STOW-RS)
**Environment:** loopback and LAN field tests, CT studies of 432 and 626 instances (~930 KB/slice, Explicit VR Little Endian)
**Scope:** findings below are reproducible observations from server logs and a packet-level test harness. No patient data is included.

---

## 1. MAIN FINDING (actionable): default `UrlReadTimeout=15000` aborts large STOW uploads mid-body

### Observation

Uploading a study larger than ~15 seconds of client-side send time fails reliably:

- 432-slice study: client aborted at slice 232, server logged
  `STOW 400: part stream (body ended before Content-Length satisfied) got=307 study=...` with `us=14174086` (14.2 s).
- After setting `java-options=-DUrlReadTimeout=120000` in `Weasis.cfg`, the same study completed:
  `ACCESS status=200 ... us=28455342` (28.5 s), all 432 slices stored.
- The failure count tracks the deadline exactly: `232/432 = throughput * 15 s`. Small studies (a few seconds) always succeed.

### Mechanism

`NetworkUtil.getUrlReadTimeout()` reads the `UrlReadTimeout` system property
(default 15000 ms) and the DICOM Export path applies it as
`HttpRequest.timeout(...)` on the whole STOW exchange: **the deadline covers
the entire multipart POST, from the first header to the server response.**

A STOW-RS server cannot respond before the last multipart part arrives, so for
any study whose send takes longer than the deadline, the client cancels its own
upload mid-body. The server correctly reports 400 (PS3.18 10.5.3.1 bad syntax).

### Why server speed does not help

The exchange duration is bounded by the *client's own send rate* (local disk
read + Java pipeline). Our measurements: the same client sends a ~400 MB study
in ~93 s to any server, fast or slow. A faster server cannot shorten the
client's send time, so the 15 s default breaks large-study export to **any**
DICOMweb server, not just ours.

### Suggestion

1. Raise the default (or make it adaptive, e.g. based on the payload size), or
2. Document `-DUrlReadTimeout=120000` prominently for DICOM Export users, or
3. Apply the timeout per-part / as an idle timeout instead of a whole-exchange
   deadline.

Workaround for end users today: add `java-options=-DUrlReadTimeout=120000`
to `Weasis.cfg` (plain ASCII, no BOM).

---

## 2. HISTORICAL (for the record): JDK 26 HttpClient h2c probe broke direct STOW

On JDK 26, `java.net.http.HttpClient` sends an empty HTTP/2-cleartext probe
(`POST` + `Upgrade: h2c`, `Content-Length: 0`, no `Content-Type`) before the
real DICOM body and, regardless of the server response, **never sends the body
over HTTP/1.1**.

We tested 9 server responses (400, 200, RST, malformed 101, and a fully
correct RFC 7540 3.2 h2c handshake). After a *correct* handshake the client
sent `SETTINGS_ACK=0, HEADERS=0, DATA=0` and disconnected. No test succeeded.

### Verification against the shipped 4.7.2 binaries

Weasis has no HTTP/2 implementation of its own: the STOW path is the JDK
`java.net.http.HttpClient` (no Apache HttpClient, no OkHttp anywhere in the
installed bundles). What changed is the version pin:

- The installer bundles a jlinked JRE **26.0.1** (module `java.net.http`
included), so the Java side did not change meaningfully.
- In the shipped `weasis-dicom-codec` bundle, `DicomStowConfig` declares
  `DEFAULT_HTTP_VERSION = HttpClient.Version.HTTP_1_1` and `DicomStowRS`
  applies it through `HttpClient.Builder.version(...)` when it builds the
  STOW client. On the STOW path the upgrade probe is therefore impossible:
  the client is pinned to HTTP/1.1.
- The shared client used by the other DICOMweb paths
  (`HttpUtils.buildHttpClient`) carries no version pin, so the JDK 26
  default (HTTP/2 with the h2c upgrade probe on plain `http://`) remains
  observable on those paths.

Conclusion: the probe is JDK 26 behavior, but whether it fires is the
caller's choice. The current build does not show it on STOW because Weasis
pins HTTP/1.1 there, not because the JDK was replaced. Large exports
complete today for a second, independent reason: the `UrlReadTimeout`
workaround from finding #1 is applied to the test configuration.

---

## 3. UNVERIFIED (question, observed once): re-export sends an empty body

Re-exporting a study that had already been exported once produced three
consecutive `POST /dicomweb/studies` requests with a valid
`multipart/related; boundary=...` header and an **empty body**
(`Content-Length: 0`), server answered 400. A fresh Weasis session exported the
same study normally (200). Hypothesis: a client-side "already sent" state
suppresses the body. We could not verify the mechanism; flagging in case it
rings a bell.

---

## 4. Transparency: our own conformance bugs found in the same field testing

While debugging the above, we also caught and fixed three of our own bugs
(each now has a regression test):

- QIDO emitted `PatientName` as a plain string instead of an object
  (`{"Alphabetic": ...}`) - non-conformant to PS3.18 F.2, broke Weasis'
  patient tree.
- Study-level QIDO emitted `DA`/`TM` as numbers instead of strings - dcm4che
  `JSONReader` rejects numeric tokens and drops the whole result set.
- URL percent-decoder treated each decoded byte as a Latin-1 character,
  breaking Cyrillic/CJK search values.

We mention these so the report is complete: two of the three Weasis-vs-server
issues historically reported were actually ours.

Prepared by the Clarus PACS team.

## Trademarks

DICOM® is the registered trademark of the National Electrical Manufacturers
Association (NEMA) for its standards publications relating to digital
communications of medical information. DICOMweb™ is a trademark of NEMA.
Clarus is not affiliated with or endorsed by NEMA.


