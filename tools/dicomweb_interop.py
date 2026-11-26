#!/usr/bin/env python3
# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
"""Universal DICOMweb interoperability comparison tool.

Runs one identical set of checks against TWO DICOMweb servers and prints a
line-by-line PASS/FAIL/INFO report. Seven suites:

  ups     UPS-RS CRUD parity (Create / Create with SOP Common / Search /
          ChangeState / Update), including the two honest wire differences:
          dcm4chee requires ?requester={AE} on ChangeState and spells the
          Update parameter "transaction-uid" (hyphenated).
  upsfilter UPS-RS subscriptions (PS3.18 11.10/11.11): a Filtered Worklist
          subscription MUST carry `filter` (Table 11.10.1-2 marks it C -
          conditional on the target resource), the Global Worklist one must
          still accept none, and a wildcard filter is exercised end to end
          against a LOCAL event sink (see "Local sink" below).
  fuzzy   QIDO-RS study search with fuzzymatching=true. Clarus uses a bounded
          Levenshtein (1 edit); dcm4chee-arc uses Soundex (1918), which
          degenerates on Cyrillic. The tool prints counts, not verdicts.
  commit  Storage Commitment over DICOMweb (PS3.18 13.4/13.5). Clarus serves
          it; dcm4chee-arc has no route (404/400). Printed as a fact.
  syntax  Transfer-syntax round-trip: STOWs an Implicit VR Little Endian file
          and asserts the instance (and one frame) come back re-encoded as
          Explicit VR Little Endian, because PS3.18 8.7.3 forbids IVRLE over
          DICOMweb. Metadata 0002,0010 and single-part Accept are facts. Also
          asserts that a Content-Type whose subtype merely STARTS with
          multipart/related (10.5.3-1) is refused with 415.
  wado    WADO-RS diff on the seeded corpus: /metadata and /frames/1 fetched
          back from both servers; transfer-syntax agreement is the diff.
  frames  Frame-list diff (PS3.18 10.4.1.1.6): /frames/1 is asserted, while
          /frames/1,2 is a FACT on both sides - the standard defines no
          grammar for the `{frames}` value, so neither server can be called
          wrong. Same shape as `wado ts agree` / `wado ts DIFFER`.

The tool is an external client: it only creates the state its own checks need.
It never deletes or mutates anything beyond that. Every subscription it
creates is deleted again (PS3.18 11.11) before the suite moves on.

Local sink: the `upsfilter` wildcard check is the ONE place this tool stops
being a pure client. A UPS-RS event is an HTTP POST from the server to the
subscriber's endpoint (PS3.18 11.10.4), so measuring whether a filter ever
fires needs a receiver. One `ThreadingHTTPServer` on 127.0.0.1 with an
ephemeral port is started for the duration of that check and shut down after
it; it is ours, it is loopback, and it stores nothing on disk.

Only the Python standard library is used, by design (KISS / nginx-way).
No patient names are hard-coded anywhere: names are operator arguments.
"""

import argparse
# Aliased: the request helper below is named http(), and a module-level
# binding of that name would shadow the stdlib package the event sink needs.
import http.server as http_server
import json
import os
import re
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

# Wire constants verified on 2026-10-01 against both servers, by running the
# transactions of PS3.18 11.4 (Create), 11.6 (Update) and 11.7 (Change State).
UPS_PUSH_SOP_CLASS = "1.2.840.10008.5.1.4.34.6.1"
DEFAULT_SOP_CLASS = "1.2.840.10008.5.1.4.1.1.2"  # CT Image Storage
DICOM_JSON = "application/dicom+json"
IVRLE_TS = "1.2.840.10008.1.2"    # Implicit VR Little Endian
EVRLE_TS = "1.2.840.10008.1.2.1"  # Explicit VR Little Endian

# The Update query parameter (PS3.18 11.6) is spelled differently by the two
# servers, verified live 01.10:
#   Clarus   accepts ONLY "TransactionUid" (case-sensitive; the hyphenated
#            spelling is answered 400 "Missing required query parameter").
#   dcm4chee accepts either spelling.
# The tool drives each server with the spelling that server accepts. This is
# a wire fact, not a preference: PS3.18 11.6.1.2 leaves the parameter NAME to
# the origin server's conformance statement, so the spelling is discovered per
# server rather than assumed.
UPDATE_TXN_PARAM = {"a": "TransactionUid", "b": "transaction-uid"}

# A server needs the "requester" AE Title on ChangeState. Clarus does not,
# and sending it there is harmless, but the check is more informative when
# each server is driven the way it is meant to be driven.
CHANGESTATE_NEEDS_REQUESTER = {"a": False, "b": True}

# Whether Update (PS3.18 11.6) requires a prior explicit claim.
#   Clarus   yes: Update on an unclaimed workitem is 400 "Workitem has no
#            Transaction UID". The claim is a ChangeState that carries the
#            Transaction UID the Update will reuse.
#   dcm4chee no: its Update is a self-claiming N-SET, and a preceding
#            ChangeState consumes the Transaction UID, so pre-claiming makes
#            the subsequent Update a 409. Verified live 01.10.
UPDATE_NEEDS_CLAIM = {"a": True, "b": False}


def http(method, url, body=None, headers=None, timeout=30):
    """One HTTP request. Never raises: every failure becomes a result.

    Returns (status, headers_dict, body_bytes). On a transport failure the
    status is 0 and the body carries the reason as ASCII text, so the caller
    reports "[FAIL] ... (reason)" instead of a traceback.
    """
    req = urllib.request.Request(url, data=body, method=method)
    for key, value in (headers or {}).items():
        req.add_header(key, value)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, dict(resp.headers), resp.read()
    except urllib.error.HTTPError as exc:
        # A 4xx/5xx is an ANSWER, not a transport fault: return it as-is so a
        # check can assert on it (the SOP Common rejection is exactly this).
        try:
            payload = exc.read()
        except Exception:
            payload = b""
        return exc.code, dict(exc.headers or {}), payload
    except urllib.error.URLError as exc:
        return 0, {}, ("URLError: %s" % describe_reason(exc.reason)).encode("ascii", "replace")
    except (OSError, ValueError) as exc:
        return 0, {}, ("%s: %s" % (type(exc).__name__, describe_reason(exc))).encode(
            "ascii", "replace"
        )


def describe_reason(reason):
    """A stable ASCII description of a socket failure.

    `str(OSError)` is LOCALISED: on a Russian Windows it comes back in
    Cyrillic, which would break the ASCII-only output contract. The errno and
    the exception class are language-neutral, so they are what gets printed.
    """
    errno = getattr(reason, "errno", None)
    name = getattr(reason, "strerror", None)
    if errno is not None:
        # strerror is localised too; keep it only if it is already ASCII.
        if name and all(ord(ch) < 128 for ch in name):
            return "%s (errno %d)" % (name, errno)
        return "%s (errno %d)" % (type(reason).__name__, errno)
    return to_ascii(str(reason))


def ascii_text(raw):
    """Decode a body for printing without ever failing or leaking PHI.

    Non-ASCII is escaped as \\uXXXX, which keeps the whole stdout ASCII even
    when a server answers with a Cyrillic patient name.
    """
    return raw.decode("utf-8", "replace").encode("ascii", "backslashreplace").decode("ascii")


def to_ascii(text):
    """Escape any non-ASCII character as \\uXXXX.

    The report contract is ASCII-only stdout. A patient name is operator
    input and may be Cyrillic; escaping it keeps the output copy-pasteable
    into any terminal while still showing which value was sent.
    """
    return text.encode("ascii", "backslashreplace").decode("ascii")


def dicom_json_body(elements):
    """Build a DICOM JSON body from {tag: (vr, value)} pairs.

    `value` is a string or a list of strings; list values render as string
    arrays, which is the shape both servers accept for scalar VRs.
    """
    obj = {}
    for tag, (vr, value) in elements.items():
        values = value if isinstance(value, list) else [value]
        obj[tag] = {"vr": vr, "Value": values}
    return json.dumps(obj).encode("utf-8")


# -- Result collection ---------------------------------------------------

class Report:
    """Collects checks and renders them as text or JSON."""

    def __init__(self, base_a, base_b):
        self.base_a = base_a
        self.base_b = base_b
        self.suites = {}
        self._current = None

    def suite(self, name):
        self._current = name
        self.suites.setdefault(name, [])

    def add(self, check, status, detail=""):
        self.suites[self._current].append(
            {"check": check, "status": status, "detail": detail}
        )
        tag = {"PASS": "[PASS]", "FAIL": "[FAIL]", "INFO": "[INFO]"}[status]
        line = "%s %s" % (tag, to_ascii(check))
        if detail:
            line += " - " + to_ascii(detail)
        print(line)

    def pass_(self, check, detail=""):
        self.add(check, "PASS", detail)

    def fail(self, check, detail=""):
        self.add(check, "FAIL", detail)

    def info(self, check, detail=""):
        self.add(check, "INFO", detail)

    def counts(self):
        total = passed = failed = 0
        for checks in self.suites.values():
            for check in checks:
                total += 1
                if check["status"] == "PASS":
                    passed += 1
                elif check["status"] == "FAIL":
                    failed += 1
        return total, passed, failed

    def summary_line(self):
        total, passed, failed = self.counts()
        return "summary: total=%d pass=%d fail=%d" % (total, passed, failed)

    def as_json(self):
        return {
            "server_a": self.base_a,
            "server_b": self.base_b,
            "suites": self.suites,
        }


# -- Shared helpers ------------------------------------------------------

def new_uid():
    """A UUID-derived DICOM UID (2.25 root), unique per run."""
    return "2.25." + str(uuid.uuid4().int)


def query_param(url, **params):
    """Append query parameters to a URL, preserving any it already carries."""
    parts = urllib.parse.urlsplit(url)
    pairs = urllib.parse.parse_qsl(parts.query, keep_blank_values=True)
    pairs.extend((k, v) for k, v in params.items() if v is not None)
    return urllib.parse.urlunsplit(
        (parts.scheme, parts.netloc, parts.path, urllib.parse.urlencode(pairs), parts.fragment)
    )


def short_uid(uid, keep=24):
    """Trim a UID for a log line; the tail is what distinguishes instances."""
    return uid if len(uid) <= keep else "..." + uid[-keep:]


# How much of a file read_dicom_fields primes its table walk with. The wanted
# elements all live in the first elements of a dataset, and a Pixel Data
# element is skipped by its declared length without being read, so nothing
# past the primed head is fetched. 1 MiB covers a dataset whose leading
# sequences are large while still refusing to buffer a multi-gigabyte
# whole-slide image in memory.
_HEAD_BYTES = 1024 * 1024


def read_dicom_fields(path):
    """Extract a few well-known elements from a DICOM Part 10 file.

    Stdlib only: walks the file meta group (always Explicit VR Little
    Endian) for the Transfer Syntax UID (0002,0010), then the dataset as
    implicit or explicit VR per that UID for the Study/Series/Instance UIDs
    and Number of Frames. Returns a dict (``ts``, ``sop_class``,
    ``instance``, ``study``, ``series``, ``frames``) or None when the file
    is not DICOM Part 10.

    Only the head of the file is read, plus the value bytes of the elements
    actually wanted: the file size is unbounded (a whole-slide image runs to
    tens of gigabytes) and the only field this needs from the tail of a
    dataset is nothing at all.
    """
    with open(path, "rb") as handle:
        head = handle.read(_HEAD_BYTES)

        def value_bytes(offset, length):
            """`length` bytes at `offset`, fetched from disk past the head.

            The wanted values are short, but they can sit past the primed
            read when a long sequence precedes them, so the fallback is a
            positional read rather than an empty slice.
            """
            if offset + length <= len(head):
                return head[offset : offset + length]
            handle.seek(offset)
            return handle.read(length)

        return _walk_dicom(head, value_bytes)


def _walk_dicom(data, value_bytes):
    """Element walk behind read_dicom_fields; `value_bytes` does the I/O."""
    if len(data) < 136 or data[128:132] != b"DICM":
        return None

    def u16(off):
        return int.from_bytes(data[off : off + 2], "little")

    def u32(off):
        return int.from_bytes(data[off : off + 4], "little")

    long_vr = {"OB", "OW", "OF", "SQ", "UT", "UN"}
    ts = None
    # File meta (group 0002): always Explicit VR Little Endian, bounded by
    # (0002,0000) which PS3.10 7.1 Table 7.1-1 defines as "the number of bytes
    # following the Value Field of this element to the end of the last of the
    # group 0002 elements".
    #
    # The elements are located by TAG within that bound rather than by stepping
    # element to element. Writers exist that declare (0002,0001) File Meta
    # Information Version as two bytes and then emit FOUR (00 00 00 01): a
    # positional walk is misaligned by that padding for every element after it
    # and reports no transfer syntax at all, even though the group length names
    # exactly where the group ends. Only these two elements are needed, so the
    # scan looks for each in turn and needs no length to be correct.
    meta_end = len(data)
    if u16(132) == 0x0002 and u16(134) == 0x0000:
        meta_end = min(len(data), 132 + 12 + u32(140))
    for elem in (0x0010, 0x0002):
        cursor = 132
        while cursor + 8 <= meta_end:
            if u16(cursor) != 0x0002 or u16(cursor + 2) != elem:
                cursor += 2
                continue
            vr = data[cursor + 4 : cursor + 6].decode("ascii", "replace")
            if vr in long_vr:
                # Explicit VR long form: tag(4) VR(2) reserved(2) length(4),
                # so the length sits at +8 and the value starts at +12 (PS3.5
                # 7.1.2 Table 7.1-1).
                length, value_pos = u32(cursor + 8), cursor + 12
            else:
                length, value_pos = u16(cursor + 6), cursor + 8
            if value_pos + length > meta_end:
                break
            value = value_bytes(value_pos, length).decode("ascii", "replace").strip(
                "\x00 "
            )
            if elem == 0x0010:
                ts = value
            break
        if ts is not None:
            break
    pos = meta_end

    # Dataset: implicit or explicit VR little endian.
    implicit = ts == IVRLE_TS
    wanted = {
        (0x0008, 0x0016): "sop_class",
        (0x0008, 0x0018): "instance",
        (0x0020, 0x000D): "study",
        (0x0020, 0x000E): "series",
        (0x0028, 0x0008): "frames",
    }
    out = {}
    while len(out) < len(wanted):
        if pos + 8 > len(data):
            # The walk has stepped past the primed head. Header bytes are read
            # from disk so the elements that follow a long sequence can still
            # be found; only their short values are ever kept.
            head = value_bytes(pos, 8)
            if len(head) < 8:
                break
        else:
            head = data[pos : pos + 8]
        group = int.from_bytes(head[0:2], "little")
        elem = int.from_bytes(head[2:4], "little")
        if implicit:
            length, value_pos = u32(pos + 4), pos + 8
        else:
            vr = head[4:6].decode("ascii", "replace")
            if vr in long_vr:
                # Explicit VR long form: tag(4) VR(2) reserved(2) length(4)
                # (PS3.5 7.1.2 Table 7.1-1).
                length = int.from_bytes(value_bytes(pos + 8, 4), "little")
                value_pos = pos + 12
            else:
                length = int.from_bytes(head[6:8], "little")
                value_pos = pos + 8
        if length == 0xFFFFFFFF:
            # Undefined length: the extent is only known by walking the item
            # stream, and every wanted element precedes the first such sequence
            # in the corpora this reads.
            break
        key = (group, elem)
        if key in wanted and length:
            # Only the WANTED values are fetched, and the head may not hold
            # them: a sequence longer than the primed read can sit between
            # this element and the ones after it, so the walk steps past it by
            # its declared length rather than stopping.
            out[wanted[key]] = (
                value_bytes(value_pos, length).decode("ascii", "replace").strip("\x00 ")
            )
        pos = value_pos + length
        if len(out) == len(wanted):
            break

    return {"ts": ts, **out}


# -- Seeding -------------------------------------------------------------

def seed_files(seed_dir, seed_glob):
    """Every *.dcm under seed_dir, sorted, so both servers get the same order."""
    found = []
    for dirpath, _dirs, names in os.walk(seed_dir):
        for name in names:
            if name.lower().endswith(seed_glob.lower().lstrip("*")):
                found.append(os.path.join(dirpath, name))
    found.sort()
    return found


# Past this size a STOW body is refused instead of buffered. Chosen well above
# any ordinary instance and well below a whole-slide image, so an accidentally
# oversized corpus file reports its size instead of exhausting memory mid-run.
_STOW_MAX_BYTES = 1024 * 1024 * 1024


def _read_for_stow(path):
    """Whole file bytes for a STOW body, or None when it is too large.

    The body IS the file, so it is read whole - but a corpus may contain a
    whole-slide image of tens of gigabytes, and buffering one would spend the
    machine's memory on a single request. The ceiling is about the CLIENT: past
    it the caller reports the size instead of attempting the POST.
    """
    if os.path.getsize(path) > _STOW_MAX_BYTES:
        return None
    with open(path, "rb") as handle:
        return handle.read()


def stow_one(base, path, timeout):
    """STOW one file as a single-part multipart/related body.

    multipart/related is used rather than a bare `application/dicom` part
    because it is the ONE media type both servers accept: Clarus takes
    either, dcm4chee-arc answers a single-part POST with 415. The file bytes
    are sent verbatim - nothing is parsed on this side.
    """
    blob = _read_for_stow(path)
    if blob is None:
        return 0, {}, (
            "file is %d bytes, above the %d-byte client ceiling"
            % (os.path.getsize(path), _STOW_MAX_BYTES)
        ).encode("ascii", "replace")
    boundary = "dicomweb-interop-%s" % uuid.uuid4().hex
    body = bytearray()
    body += ("--%s\r\nContent-Type: application/dicom\r\n\r\n" % boundary).encode("ascii")
    body += blob
    body += ("\r\n--%s--\r\n" % boundary).encode("ascii")
    return http(
        "POST",
        base + "/studies",
        body=bytes(body),
        headers={
            "Content-Type": 'multipart/related; type="application/dicom"; boundary=%s'
            % boundary
        },
        timeout=timeout,
    )


def run_seed(report, args):
    """STOW the same corpus to both servers.

    A file the server refuses is a FAIL with the count and the first reason:
    the operator needs to know whether the corpus or the server is at fault.
    A corpus may legitimately contain a non-instance object (a DICOMDIR media
    directory has no PatientID/StudyInstanceUID), which a conformant server
    answers 400 for - the count is what makes that visible.
    """
    report.suite("seed")
    files = seed_files(args.seed_dir, args.seed_glob)
    if not files:
        report.info("seed", "no matching files under %s" % args.seed_dir)
        return
    for label, base in (("A", args.a), ("B", args.b)):
        if not base:
            report.info("seed %s" % label, "no base URL given")
            continue
        ok = 0
        first_error = ""
        started = time.time()
        for path in files:
            status, _headers, body = stow_one(base, path, args.timeout)
            if status in (200, 201, 202, 204):
                ok += 1
            elif not first_error:
                first_error = "HTTP %d %s" % (status, ascii_text(body[:120]))
        detail = "%d/%d files in %.1fs" % (ok, len(files), time.time() - started)
        if ok == len(files):
            report.pass_("seed %s" % label, detail)
        else:
            report.fail("seed %s" % label, "%s; first error: %s" % (detail, first_error))


# -- Suite: ups ----------------------------------------------------------

def ups_create_body(ae_title, with_sop_common):
    """UPS Create body (PS3.18 11.4.1.1).

    Carries the type-1 attributes both servers demand: Procedure Step State
    (0074,1000) SCHEDULED, Scheduled Procedure Step Priority (0074,1200),
    Procedure Step Label (0074,1204), Scheduled Procedure Step Start
    DateTime (0040,4005), Input Readiness State (0040,4041), the Scheduled
    Station AE Title (0040,0001) and the Scheduled Workitem Code Sequence
    (0040,4018). SOP Common is added only for the negative check, which
    asserts both servers reject it (PS3.4 Table CC.2.5-3 "Not allowed" /
    "set by the SCP").
    """
    body = {
        "00404021": {
            "vr": "SQ",
            "Value": [
                {"0020000D": {"vr": "UI", "Value": [new_uid()]}},
            ],
        },
        "00741000": {"vr": "CS", "Value": ["SCHEDULED"]},
        "00741200": {"vr": "CS", "Value": ["MEDIUM"]},
        "00741204": {"vr": "LO", "Value": ["INTEROP"]},
        "00404005": {"vr": "DT", "Value": ["20260101120000"]},
        "00404041": {"vr": "CS", "Value": ["READY"]},
        "00400001": {"vr": "AE", "Value": [ae_title]},
        "00404018": {
            "vr": "SQ",
            "Value": [
                {
                    "00080100": {"vr": "SH", "Value": ["INTEROP"]},
                    "00080102": {"vr": "SH", "Value": ["99CLARUS"]},
                    "00080104": {"vr": "LO", "Value": ["Interop check"]},
                }
            ],
        },
    }
    if with_sop_common:
        body["00080016"] = {"vr": "UI", "Value": [UPS_PUSH_SOP_CLASS]}
        body["00080018"] = {"vr": "UI", "Value": [new_uid()]}
    return json.dumps(body).encode("utf-8")


def check_create(report, label, base, ae_title, uid, with_sop_common, timeout, expect_reject):
    status, headers, body = http(
        "POST",
        query_param(base + "/workitems", workitem=uid),
        body=ups_create_body(ae_title, with_sop_common),
        headers={"Content-Type": DICOM_JSON},
        timeout=timeout,
    )
    name = "ups create%s %s" % (" (SOP Common)" if with_sop_common else "", label)
    if expect_reject:
        if status == 400:
            report.pass_(name, "rejected with 400 as PS3.4 CC.2.5-3 requires")
        else:
            report.fail(name, "expected 400, got %d %s" % (status, ascii_text(body[:160])))
        return
    if status in (200, 201):
        location = headers.get("Location", "")
        report.pass_(name, "HTTP %d%s" % (status, "; Location present" if location else ""))
    elif status == 0:
        report.fail(name, ascii_text(body[:160]))
    else:
        report.fail(name, "HTTP %d %s" % (status, ascii_text(body[:160])))


def check_search(report, label, base, ae_title, uid, timeout):
    """Search by Scheduled Station AE Title (0040,0001), PS3.18 11.5.

    The target workitem was created with `ae_title`. A DECOY workitem is
    created with a different AE Title; a conformant search must return the
    target and EXCLUDE the decoy. A server that ignores the filter and
    returns every workitem fails the negative half, so the check is not
    vacuous.
    """
    # Decoy: same body shape, a deliberately different Scheduled Station AE
    # Title, so it must not appear in a search for `ae_title`.
    decoy_uid = new_uid()
    decoy_ae = (ae_title or "AE") + "_DECOY"
    dstatus, _dheaders, _dbody = http(
        "POST",
        query_param(base + "/workitems", workitem=decoy_uid),
        body=ups_create_body(decoy_ae, False),
        headers={"Content-Type": DICOM_JSON},
        timeout=timeout,
    )
    if dstatus not in (200, 201):
        report.info(
            "ups search %s" % label,
            "decoy could not be staged (HTTP %d); negative half skipped" % dstatus,
        )
        decoy_uid = None

    status, _headers, body = http(
        "GET",
        query_param(base + "/workitems", **{"00400001": ae_title}),
        headers={"Accept": DICOM_JSON},
        timeout=timeout,
    )
    name = "ups search by station AE %s" % label
    if status != 200:
        report.fail(name, "HTTP %d %s" % (status, ascii_text(body[:160])))
        return
    try:
        items = json.loads(body)
    except ValueError as exc:
        report.fail(name, "response is not JSON: %s" % exc)
        return
    if not isinstance(items, list):
        report.fail(name, "response is not a JSON array")
        return
    ids = [
        item.get("00080018", {}).get("Value", [None])[0]
        for item in items
        if item.get("00080018", {}).get("Value")
    ]
    found = ids.count(uid)
    decoy_seen = decoy_uid is not None and decoy_uid in ids
    if found and not decoy_seen:
        report.pass_(name, "target found, decoy excluded (%d result(s))" % len(items))
    elif found and decoy_seen:
        report.fail(
            name,
            "target found but decoy NOT excluded - filter ignored (%d result(s))"
            % len(items),
        )
    else:
        report.fail(
            name, "target %s not present (%d result(s))" % (short_uid(uid), len(items))
        )


def check_change_state(report, label, base, ae_title, uid, txuid, timeout, needs_requester):
    """ChangeState (PS3.18 11.7): state and Transaction UID in the BODY.

    dcm4chee additionally requires the requester AE Title; Clarus does not.
    Each server is driven the way it is meant to be driven.
    """
    body = dicom_json_body(
        {
            "00741000": ("CS", "IN PROGRESS"),
            "00081195": ("UI", txuid),
        }
    )
    url = base + "/workitems/%s/state" % uid
    if needs_requester:
        url = query_param(url, requester=ae_title)
    status, _headers, payload = http(
        "PUT", url, body=body, headers={"Content-Type": DICOM_JSON}, timeout=timeout
    )
    name = "ups change-state %s" % label
    suffix = " (?requester=%s)" % ae_title if needs_requester else ""
    if status == 200:
        report.pass_(name + suffix, "HTTP 200")
    elif status == 0:
        report.fail(name + suffix, ascii_text(payload[:160]))
    else:
        report.fail(name + suffix, "HTTP %d %s" % (status, ascii_text(payload[:160])))


def check_complete(report, label, base, ae_title, uid, txuid, timeout, needs_requester):
    """Terminal state and lock (PS3.18 11.7).

    The workitem was claimed IN PROGRESS with `txuid`. ChangeState to
    COMPLETED with the SAME Transaction UID must succeed; a further
    ChangeState on the terminal workitem must be refused (409/400), which is
    what locks the lifecycle. The Transaction UID is stable across the
    workitem's transactions (it is not re-minted per request).
    """
    url = base + "/workitems/%s/state" % uid
    if needs_requester:
        url = query_param(url, requester=ae_title)
    completed = dicom_json_body(
        {"00741000": ("CS", "COMPLETED"), "00081195": ("UI", txuid)}
    )
    status, _headers, payload = http(
        "PUT", url, body=completed, headers={"Content-Type": DICOM_JSON}, timeout=timeout
    )
    name = "ups complete %s" % label
    if status != 200:
        report.fail(name, "HTTP %d %s" % (status, ascii_text(payload[:160])))
        return
    report.pass_(name, "HTTP 200 (COMPLETED)")

    again = dicom_json_body(
        {"00741000": ("CS", "IN PROGRESS"), "00081195": ("UI", txuid)}
    )
    status, _headers, payload = http(
        "PUT", url, body=again, headers={"Content-Type": DICOM_JSON}, timeout=timeout
    )
    name = "ups terminal lock %s" % label
    if status in (409, 400):
        report.pass_(name, "re-transition refused (%d)" % status)
    elif status == 0:
        report.fail(name, ascii_text(payload[:160]))
    else:
        report.fail(
            name, "expected 409/400 on a terminal workitem, got %d" % status
        )


def check_update(report, label, base, uid, txuid, timeout):
    """Update (PS3.18 11.6), on a workitem already claimed by its own ChangeState.

    The body carries the performer-report sequences Clarus' merge set accepts
    (it is deliberately narrow); dcm4chee accepts a general N-SET, so the same
    body is valid there too. Each sequence carries one item: an empty sequence
    is not an update of a sequence attribute.
    """
    performer_code = {
        "00080100": {"vr": "SH", "Value": ["INTEROP"]},
        "00080102": {"vr": "SH", "Value": ["99CLARUS"]},
        "00080104": {"vr": "LO", "Value": ["Interop check"]},
    }
    body = json.dumps(
        {
            "00404019": {"vr": "SQ", "Value": [performer_code]},
            "00404028": {"vr": "SQ", "Value": [performer_code]},
        }
    ).encode("utf-8")
    url = query_param(base + "/workitems/%s" % uid, **{UPDATE_TXN_PARAM[label.lower()]: txuid})
    status, _headers, payload = http(
        "POST", url, body=body, headers={"Content-Type": DICOM_JSON}, timeout=timeout
    )
    name = "ups update %s (?%s=...)" % (label, UPDATE_TXN_PARAM[label.lower()])
    if status == 200:
        report.pass_(name, "HTTP 200")
    elif status == 0:
        report.fail(name, ascii_text(payload[:160]))
    else:
        report.fail(name, "HTTP %d %s" % (status, ascii_text(payload[:160])))


def run_ups(report, args):
    report.suite("ups")
    if not (args.a and args.b):
        report.info("ups", "skipping comparison (need both bases)")
    for label, base, ae in (("A", args.a, args.a_ae), ("B", args.b, args.b_ae)):
        if not base:
            report.info("ups %s" % label, "no base URL given")
            continue
        # Each check gets its own workitem and transaction UID. A state change
        # consumes the Transaction UID it was given (the workitem is claimed),
        # so sharing one between ChangeState and Update would test the claim,
        # not the Update route.
        uid = new_uid()
        check_create(report, label, base, ae, uid, False, args.timeout, False)
        check_create(report, label, base, ae, new_uid(), True, args.timeout, True)
        check_search(report, label, base, ae, uid, args.timeout)
        # Lifecycle: SCHEDULED -> IN PROGRESS -> COMPLETED, then a refused
        # re-transition proves a terminal state is locked.
        txuid = new_uid()
        check_change_state(
            report,
            label,
            base,
            ae,
            uid,
            txuid,
            args.timeout,
            CHANGESTATE_NEEDS_REQUESTER[label.lower()],
        )
        check_complete(
            report,
            label,
            base,
            ae,
            uid,
            txuid,
            args.timeout,
            CHANGESTATE_NEEDS_REQUESTER[label.lower()],
        )

        # Update runs on its own workitem, whose claim UID is known exactly:
        # created, then claimed by a state change we do not need to assert on.
        upd_uid = new_uid()
        upd_tx = new_uid()
        status, _headers, body = http(
            "POST",
            query_param(base + "/workitems", workitem=upd_uid),
            body=ups_create_body(ae, False),
            headers={"Content-Type": DICOM_JSON},
            timeout=args.timeout,
        )
        if status not in (200, 201):
            report.fail(
                "ups update %s" % label,
                "could not stage a workitem (HTTP %d %s)" % (status, ascii_text(body[:120])),
            )
            continue
        state_url = base + "/workitems/%s/state" % upd_uid
        if CHANGESTATE_NEEDS_REQUESTER[label.lower()]:
            state_url = query_param(state_url, requester=ae)
        if UPDATE_NEEDS_CLAIM[label.lower()]:
            claim_status, _headers, claim_body = http(
                "PUT",
                state_url,
                body=dicom_json_body(
                    {"00741000": ("CS", "IN PROGRESS"), "00081195": ("UI", upd_tx)}
                ),
                headers={"Content-Type": DICOM_JSON},
                timeout=args.timeout,
            )
            if claim_status != 200:
                report.fail(
                    "ups update %s" % label,
                    "could not claim the workitem (HTTP %d %s)"
                    % (claim_status, ascii_text(claim_body[:120])),
                )
                continue
        check_update(report, label, base, upd_uid, upd_tx, args.timeout)


# -- Suite: upsfilter ----------------------------------------------------

# PS3.18 11.10.1.2 Table 11.10.1-2 lists two subscription targets: the Global
# Worklist (1.2.840.10008.5.1.4.34.5) and the Filtered Worklist
# (1.2.840.10008.5.1.4.34.5.1). The `filter` query parameter is typed C -
# "Required when the Target Resource is the Filtered Worklist" - so the two
# targets differ in kind, not in degree, and the checks below assert each
# against its own obligation.
UPS_WORKLIST_GLOBAL = "1.2.840.10008.5.1.4.34.5"
UPS_WORKLIST_FILTERED = "1.2.840.10008.5.1.4.34.5.1"

# The wildcard probe filters on Procedure Step Label (0074,1204), one of the
# type-1 Create attributes ups_create_body already fills in. A prefix filter
# ("TEST_*") is the shape a scheduler uses to namespace its own tasks.
FILTER_LABEL_TAG = "00741204"
FILTER_PREFIX = "TEST_"
FILTER_LABEL = "TEST_123"
FILTER_DECOY_LABEL = "OTHER_123"

# How long the event sink waits for the positive push, and how long it then
# listens for the negative-control push. The negative window is short on
# purpose: a server that pushes unrelated workitems does so immediately, so a
# 2 s silence is a far stronger statement than a 2 s wait is a burden.
SINK_POSITIVE_WAIT_S = 20.0
SINK_NEGATIVE_WINDOW_S = 2.0


class _EventSink:
    """A loopback HTTP receiver for UPS-RS event reports (PS3.18 11.10.4).

    The origin server delivers an event by POSTing to the subscriber endpoint,
    so "did this filter fire" can only be answered by a process that listens.
    This is the one place the tool is not a pure client: the listener is ours,
    bound to 127.0.0.1 on an ephemeral port, and it keeps nothing but the raw
    bodies of the requests it received.

    Every POST is answered 200 (the acknowledgement the server expects back);
    everything else is answered 405 so a stray GET does not look like a
    delivery.
    """

    def __init__(self):
        self.posts = []
        self._lock = threading.Lock()
        sink = self

        class _Handler(http_server.BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802 - http.server's spelling
                try:
                    length = int(self.headers.get("Content-Length") or 0)
                except (TypeError, ValueError):
                    length = 0
                body = self.rfile.read(length) if length > 0 else b""
                with sink._lock:
                    sink.posts.append(body)
                self.send_response(200)
                self.send_header("Content-Length", "0")
                self.end_headers()

            def log_message(self, fmt, *fmt_args):
                # The tool's own stdout is the report; http.server's access log
                # would interleave stderr into it and break the ASCII contract.
                pass

        self._server = http_server.ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
        self._server.daemon_threads = True
        self._thread = threading.Thread(
            target=self._server.serve_forever, name="ups-event-sink", daemon=True
        )

    @property
    def url(self):
        host, port = self._server.server_address[:2]
        return "http://%s:%d/ups-interop-sink" % (host, port)

    def start(self):
        self._thread.start()

    def count(self):
        with self._lock:
            return len(self.posts)

    def wait_for(self, wanted, timeout):
        """Block until `wanted` POSTs have arrived or `timeout` expires."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.count() >= wanted:
                return True
            time.sleep(0.1)
        return self.count() >= wanted

    def shutdown(self):
        # shutdown() stops serve_forever and server_close() releases the
        # socket; a leaked listener would keep the port and the thread alive
        # past the report, which is exactly what a one-shot CLI must not do.
        try:
            self._server.shutdown()
        finally:
            self._server.server_close()
            self._thread.join(timeout=5.0)


def subscribe(report, label, base, resource, ae, endpoint, timeout, filter_value=None):
    """POST a subscription (PS3.18 11.10) and return its HTTP status.

    A transport failure is reported once, here, because every caller would
    otherwise repeat the same `status == 0` branch.
    """
    url = query_param(
        base + "/workitems/%s/subscribers/%s" % (resource, ae), filter=filter_value
    )
    body = json.dumps({"endpoint": endpoint}).encode("utf-8")
    status, _headers, payload = http(
        "POST", url, body=body, headers={"Content-Type": DICOM_JSON}, timeout=timeout
    )
    if status == 0:
        report.fail("%s" % label, ascii_text(payload[:160]))
    return status


def unsubscribe(report, base, resource, ae, timeout):
    """DELETE a subscription (PS3.18 11.11) and report the status as a fact.

    Not a verdict: a server that never created the subscription cannot be
    asked to delete it, and the check it belonged to already carries the
    verdict. Leaving a subscription behind would make every later workitem of
    this run arrive at a dead endpoint, so the deletion is unconditional.
    """
    url = base + "/workitems/%s/subscribers/%s" % (resource, ae)
    status, _headers, payload = http("DELETE", url, timeout=timeout)
    report.info(
        "upsfilter unsubscribe %s" % resource.split(".")[-1],
        "DELETE -> HTTP %d" % status if status else ascii_text(payload[:120]),
    )


def create_workitem(base, ae_title, label, timeout):
    """Create one workitem carrying `label` as Procedure Step Label.

    Reuses ups_create_body rather than duplicating the element set: the same
    body the `ups` suite proved creatable is the one the filter is asked to
    match. A 409 means the UID is already taken; the caller retries once with
    a fresh one.
    """
    uid = new_uid()
    body = ups_create_body(ae_title, False)
    payload = json.loads(body)
    payload[FILTER_LABEL_TAG] = {"vr": "LO", "Value": [label]}
    return uid, http(
        "POST",
        query_param(base + "/workitems", workitem=uid),
        body=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": DICOM_JSON},
        timeout=timeout,
    )


def check_filtered_requires_filter(report, label, base, ae, endpoint, timeout):
    """A Filtered Worklist subscription with no `filter` must be refused.

    PS3.18 11.10.1.2 Table 11.10.1-2 types the parameter C, "Required when
    the Target Resource is the Filtered Worklist", so a 2xx here means the
    server created a subscription that matches EVERY workitem - the subscriber
    then receives the whole worklist, which is neither what the caller asked
    for nor what a filtered subscription is for.
    """
    name = "upsfilter filtered-without-filter %s" % label
    status = subscribe(
        report, name, base, UPS_WORKLIST_FILTERED, ae, endpoint, timeout, None
    )
    if status in (400, 422):
        report.pass_(name, "refused without filter (HTTP %d)" % status)
    elif status in (200, 201, 202, 204):
        report.fail(
            name,
            "accepted a Filtered subscription with no filter: HTTP %d - the "
            "subscription matches EVERY workitem (PS3.18 11.10.1.2 marks "
            "`filter` required for this target)" % status,
        )
    elif status == 0:
        pass  # reported by subscribe()
    else:
        report.fail(name, "unexpected HTTP %d" % status)


def check_unfiltered_accepts(report, label, base, ae, endpoint, timeout):
    """The Global Worklist subscription must still accept without a filter.

    The control for the check above: `filter` is optional on the unfiltered
    target, so a refusal here would mean the fix spread to the resource where
    the parameter is genuinely optional.
    """
    name = "upsfilter unfiltered-subscribe %s" % label
    status = subscribe(
        report, name, base, UPS_WORKLIST_GLOBAL, ae, endpoint, timeout, None
    )
    if status in (200, 201, 202, 204):
        report.pass_(name, "accepted (HTTP %d)" % status)
    elif status == 0:
        pass  # reported by subscribe()
    else:
        report.fail(
            name,
            "Global Worklist subscription refused with HTTP %d - `filter` is "
            "optional on this target" % status,
        )


def check_wildcard_delivery(report, label, base, ae, timeout):
    """Does a prefix filter deliver, and does it stay narrow?

    Two halves, because either alone is worthless: a server that pushes
    everything passes the first, and a server that pushes nothing fails it.
    The negative control creates a workitem the filter must NOT match and
    asserts the sink receives no further POST within a short window.
    """
    name = "upsfilter wildcard-filter %s" % label
    sink = _EventSink()
    created = []
    try:
        sink.start()
        status = subscribe(
            report,
            name,
            base,
            UPS_WORKLIST_FILTERED,
            ae,
            sink.url,
            timeout,
            "%s=%s*" % (FILTER_LABEL_TAG, FILTER_PREFIX),
        )
        if status == 0:
            return
        if status == 400:
            report.fail(
                name,
                "refused `%s=%s*` with 400 - the server rejects wildcard "
                "patterns outright, which is a different defect from failing "
                "to match them" % (FILTER_LABEL_TAG, FILTER_PREFIX),
            )
            return
        if status not in (200, 201, 202, 204):
            report.fail(name, "subscribe with wildcard filter -> HTTP %d" % status)
            return

        uid, (cstatus, _ch, cbody) = create_workitem(base, ae, FILTER_LABEL, timeout)
        if cstatus == 409:
            # The UID is server-chosen on this path only if a previous run
            # reused it; retry once exactly as the ups suite does.
            uid, (cstatus, _ch, cbody) = create_workitem(
                base, ae, FILTER_LABEL, timeout
            )
        if cstatus not in (200, 201):
            report.fail(
                name,
                "could not create %s (HTTP %d %s)"
                % (FILTER_LABEL, cstatus, ascii_text(cbody[:120])),
            )
            return
        created.append(uid)

        if sink.wait_for(1, SINK_POSITIVE_WAIT_S):
            report.pass_(name, "wildcard filter delivered %s" % FILTER_LABEL)
        else:
            report.fail(
                name,
                "no event for %s under filter %s* - a prefix filter that never "
                "fires cannot be used to namespace tasks"
                % (FILTER_LABEL, FILTER_PREFIX),
            )
            return

        # Negative control: from here on the sink must stay silent.
        before = sink.count()
        decoy_uid, (dstatus, _dh, dbody) = create_workitem(
            base, ae, FILTER_DECOY_LABEL, timeout
        )
        if dstatus not in (200, 201):
            report.info(
                name,
                "negative control skipped: decoy create -> HTTP %d %s"
                % (dstatus, ascii_text(dbody[:100])),
            )
            return
        created.append(decoy_uid)
        time.sleep(SINK_NEGATIVE_WINDOW_S)
        if sink.count() == before:
            report.pass_(
                name + " negative",
                "%s NOT delivered within %.0fs" % (FILTER_DECOY_LABEL, SINK_NEGATIVE_WINDOW_S),
            )
        else:
            report.fail(
                name + " negative",
                "unrelated workitem was delivered through a filtered "
                "subscription (%d push(es) within %.0fs)"
                % (sink.count() - before, SINK_NEGATIVE_WINDOW_S),
            )
    finally:
        for resource in (UPS_WORKLIST_FILTERED, UPS_WORKLIST_GLOBAL):
            unsubscribe(report, base, resource, ae, timeout)
        sink.shutdown()


def run_upsfilter(report, args):
    """UPS-RS subscriptions: filter obligation, control, and delivery.

    A separate suite rather than more `ups` checks so `--suite ups` keeps
    printing byte-identical lines; the `ups` suite asserts DIMSE-mailbox
    parity and knows nothing about subscriptions.
    """
    report.suite("upsfilter")
    for label, base, ae in (("A", args.a, args.a_ae), ("B", args.b, args.b_ae)):
        if not base:
            report.info("upsfilter %s" % label, "no base URL given")
            continue
        # Checks 1 and 2 assert the SUBSCRIBE response only and never
        # delivery, so the endpoint just has to be a syntactically valid URL.
        # Port 1 on loopback is guaranteed to have no listener, which keeps a
        # server that wrongly accepts the subscription from posting anywhere.
        endpoint = "http://127.0.0.1:1/ups-interop-sink"
        check_filtered_requires_filter(report, label, base, ae, endpoint, args.timeout)
        unsubscribe(report, base, UPS_WORKLIST_FILTERED, ae, args.timeout)
        check_unfiltered_accepts(report, label, base, ae, endpoint, args.timeout)
        unsubscribe(report, base, UPS_WORKLIST_GLOBAL, ae, args.timeout)
        check_wildcard_delivery(report, label, base, ae, args.timeout)


# -- Suite: fuzzy --------------------------------------------------------

# Deterministic edit scripts, so a run is reproducible: the position and the
# replacement character are fixed, not random.
EDIT_SUBSTITUTIONS = ["X", "Z"]


def edit_variants(name, count):
    """`count` deterministic single-character substitutions."""
    out = []
    for index in range(min(count, len(EDIT_SUBSTITUTIONS))):
        if not name:
            break
        pos = (index * 2 + 1) % len(name)
        out.append(name[:pos] + EDIT_SUBSTITUTIONS[index] + name[pos + 1 :])
    return out


def fuzzy_variants(names):
    """Build the query set: exact, 1-edit, 2-edit, wildcard, foreign name."""
    variants = []
    for name in names:
        variants.append(("exact", name))
        one = edit_variants(name, 1)
        if one:
            variants.append(("1-edit", one[0]))
        two = edit_variants(name, 2)
        if len(two) == 2:
            # Two substitutions applied together: a genuine 2-edit distance.
            pos_a, pos_b = (1 % len(name)), (3 % len(name))
            if pos_a != pos_b:
                chars = list(name)
                chars[pos_a] = EDIT_SUBSTITUTIONS[0]
                chars[pos_b] = EDIT_SUBSTITUTIONS[1]
                variants.append(("2-edit", "".join(chars)))
        variants.append(("wildcard", name[: max(1, len(name) // 2)] + "*"))
    # A name from the OTHER supplied name, when one exists: the false-match
    # probe. Only meaningful with >= 2 names on the command line.
    if len(names) >= 2:
        variants.append(("foreign", names[1]))
    # De-duplicate while keeping order.
    seen = set()
    unique = []
    for kind, value in variants:
        key = (kind, value)
        if key not in seen:
            seen.add(key)
            unique.append((kind, value))
    return unique


def qido_studies(base, name, timeout):
    """QIDO-RS study search with fuzzymatching=true. Returns (status, ids, err)."""
    url = query_param(
        base + "/studies",
        fuzzymatching="true",
        **{"00100010": name, "includefield": "00100010,00100020"}
    )
    status, _headers, body = http("GET", url, headers={"Accept": DICOM_JSON}, timeout=timeout)
    if status != 200:
        return status, [], ascii_text(body[:120])
    try:
        items = json.loads(body)
    except ValueError as exc:
        return status, [], "not JSON: %s" % exc
    ids = []
    for item in items if isinstance(items, list) else []:
        value = item.get("00100020", {}).get("Value", [])
        if value:
            ids.append(str(value[0]))
    return status, ids, ""


def run_fuzzy(report, args):
    report.suite("fuzzy")
    if not args.patient_name and not args.variants:
        report.info("fuzzy", "no --patient-name or --variants given")
        return
    if not (args.a and args.b):
        report.info("fuzzy", "skipping comparison (need both bases)")

    if args.variants:
        queries = [("variant", v) for v in args.variants]
    else:
        queries = fuzzy_variants(args.patient_name)

    for kind, value in queries:
        row = []
        for label, base in (("A", args.a), ("B", args.b)):
            if not base:
                row.append("%s: n/a" % label)
                continue
            status, ids, err = qido_studies(base, value, args.timeout)
            if status == 0:
                row.append("%s: [FAIL] %s" % (label, err))
            elif status != 200:
                row.append("%s: HTTP %d" % (label, status))
            else:
                shown = ",".join(ids) if ids else "-"
                row.append("%s: %d hit(s) [%s]" % (label, len(ids), shown))
        report.info("fuzzy %-8s %s" % (kind, value), " | ".join(row))

    # One count-only line per name, no interpretation: the reader compares.
    for name in args.patient_name:
        parts = []
        for label, base in (("A", args.a), ("B", args.b)):
            if not base:
                parts.append("%s=n/a" % label)
                continue
            status, ids, _err = qido_studies(base, name, args.timeout)
            parts.append("%s=%s" % (label, len(ids) if status == 200 else "err"))
        report.info("fuzzy total exact %s" % name, " ".join(parts))


# -- Suite: commit -------------------------------------------------------

def commit_body(sop_class, sop_instances):
    """Storage Commitment request body (PS3.18 13.4, PS3.3 Annex J.1).

    The module carries EXACTLY ONE of Referenced SOP Sequence (0008,1199) or
    Referenced Study Sequence (0008,1110); the instance form is used here.
    The Transaction UID is the request URI, not a body element, so it is not
    in this payload.
    """
    references = [
        {
            "00081150": {"vr": "UI", "Value": [sop_class]},
            "00081155": {"vr": "UI", "Value": [instance]},
        }
        for instance in sop_instances
    ]
    body = {"00081199": {"vr": "SQ", "Value": references}}
    return json.dumps(body).encode("utf-8")


def run_commit(report, args):
    report.suite("commit")
    if not args.sop_instance:
        report.info("commit", "commit skipped (no referenced instances)")
        return
    txuid = args.tx_uid or new_uid()
    body = commit_body(args.sop_class, args.sop_instance)

    for label, base in (("A", args.a), ("B", args.b)):
        if not base:
            report.info("commit %s" % label, "no base URL given")
            continue
        status, _headers, payload = http(
            "POST",
            base + "/commitment-requests/%s" % txuid,
            body=body,
            headers={"Content-Type": DICOM_JSON},
            timeout=args.timeout,
        )
        name = "commit request %s" % label
        if status in (200, 201, 202):
            report.pass_(name, "HTTP %d" % status)
        elif status == 0:
            report.fail(name, ascii_text(payload[:160]))
        elif status in (404, 405, 400):
            report.info(name, "HTTP %d (no Storage Commitment over DICOMweb)" % status)
        else:
            report.fail(name, "HTTP %d %s" % (status, ascii_text(payload[:160])))

        # The verdict poll: PS3.18 13.5 GET.
        status, _headers, payload = http(
            "GET",
            base + "/commitment-requests/%s" % txuid,
            headers={"Accept": DICOM_JSON},
            timeout=args.timeout,
        )
        name = "commit result %s" % label
        if status == 200:
            parsed_ok = False
            try:
                result = json.loads(payload)
                result = result[0] if isinstance(result, list) else result
                parsed_ok = "00081199" in result or "00081198" in result
            except (ValueError, TypeError):
                parsed_ok = False
            if parsed_ok:
                report.pass_(
                    name,
                    "HTTP 200, %d bytes, Referenced/Failed SOP Sequence present"
                    % len(payload),
                )
            else:
                report.fail(
                    name,
                    "HTTP 200 but no Referenced (0008,1199) / Failed (0008,1198) SOP Sequence",
                )
        elif status == 0:
            report.fail(name, ascii_text(payload[:160]))
        elif status in (404, 405, 400):
            report.info(name, "HTTP %d (no Storage Commitment over DICOMweb)" % status)
        else:
            report.fail(name, "HTTP %d %s" % (status, ascii_text(payload[:160])))


# -- Suite: syntax ------------------------------------------------------

def transfer_syntax_of(content_type, body):
    """The transfer-syntax= parameter from a Content-Type or the first part.

    The instance multipart response declares it on each part header
    (Content-Type: application/dicom; transfer-syntax=...); dcm4chee also
    repeats it on the outer multipart Content-Type. Returns the UID or None.
    """
    for piece in (content_type or "").split(";"):
        piece = piece.strip()
        if piece.lower().startswith("transfer-syntax="):
            return piece.split("=", 1)[1].strip().strip('"')
    for line in (body or b"")[:1024].split(b"\r\n"):
        if line.lower().startswith(b"content-type"):
            for piece in line.decode("ascii", "replace").split(";"):
                piece = piece.strip()
                if piece.lower().startswith("transfer-syntax="):
                    return piece.split("=", 1)[1].strip().strip('"')
    return None


def _report_ts(report, name, status, body, served, expect_reencode):
    if status != 200:
        report.fail(name, "HTTP %d %s" % (status, ascii_text(body[:120])))
        return
    if served is None:
        report.fail(name, "HTTP 200 but no transfer-syntax declared")
        return
    if expect_reencode:
        if served == EVRLE_TS:
            report.pass_(name, "served %s (IVRLE re-encoded per PS3.18 8.7.3)" % served)
        elif served == IVRLE_TS:
            report.fail(name, "served %s over DICOMweb (violates PS3.18 8.7.3)" % served)
        else:
            report.fail(name, "expected %s, got %s" % (EVRLE_TS, served))
    else:
        report.info(name, "served %s" % served)


def stow_with_media_type(base, path, timeout, content_type):
    """STOW one file with an overridden outer Content-Type.

    The part body is byte-identical to stow_one's, so the only variable is the
    container header: PS3.18 10.5.4 Table 10.5.4-1 lists `multipart/related`
    and nothing that merely begins with it, which is exactly the distinction
    a prefix-matching server would get wrong.
    """
    blob = _read_for_stow(path)
    if blob is None:
        return 0, {}, (
            "file is %d bytes, above the %d-byte client ceiling"
            % (os.path.getsize(path), _STOW_MAX_BYTES)
        ).encode("ascii", "replace")
    boundary = "dicomweb-interop-%s" % uuid.uuid4().hex
    body = bytearray()
    body += ("--%s\r\nContent-Type: application/dicom\r\n\r\n" % boundary).encode("ascii")
    body += blob
    body += ("\r\n--%s--\r\n" % boundary).encode("ascii")
    return http(
        "POST",
        base + "/studies",
        body=bytes(body),
        headers={"Content-Type": content_type % boundary},
        timeout=timeout,
    )


def check_invalid_media_type(report, label, base, path, fields, timeout):
    """A Content-Type that only STARTS with multipart/related must be 415.

    PS3.18 10.5.3.1 Table 10.5.3-1 types 415 as "unsupported media type", and
    Table 10.5.4-1 enumerates the accepted types - a subtype of
    `related-invalid` is in none of them. The payload stays a valid DICOM
    part on purpose: a server that looks only at the part body would store it,
    and the check then has something to prove.
    """
    name = "syntax invalid-media-type %s" % label
    status, _headers, body = stow_with_media_type(
        base,
        path,
        timeout,
        'multipart/related-invalid; boundary=%s; type="application/dicom"',
    )
    if status == 415:
        report.pass_(name, "refused the invalid media type")
        return
    if status in (200, 201, 202):
        report.fail(
            name,
            "stored a body whose Content-Type subtype is 'related-invalid' - "
            "PS3.18 10.5.3-1 types 415 for an unsupported media type",
        )
        # The follow-up turns "it answered 200" into "it persisted a malformed
        # request", which is the sentence a bug report needs.
        if fields.get("study") and fields.get("instance"):
            inst_url = "%s/studies/%s/series/%s/instances" % (
                base,
                fields["study"],
                fields["series"],
            )
            qstatus, _qh, qbody = http(
                "GET",
                query_param(inst_url, SOPInstanceUID=fields["instance"]),
                headers={"Accept": DICOM_JSON},
                timeout=timeout,
            )
            present = False
            if qstatus == 200:
                try:
                    hits = json.loads(qbody)
                    present = bool(hits)
                except ValueError:
                    present = False
            report.info(
                name + " stored?",
                "instance %s query -> HTTP %d, %s"
                % (
                    short_uid(fields["instance"]),
                    qstatus,
                    "PRESENT (it persisted the malformed request)" if present else "absent",
                ),
            )
        return
    if status == 400:
        report.info(
            name,
            "HTTP 400 - it did not store it, but 415 is the typed answer",
        )
        return
    report.fail(name, "HTTP %d %s" % (status, ascii_text(body[:120])))


def run_syntax(report, args):
    """Transfer-syntax round-trip (PS3.18 8.7.3) on an IVRLE file.

    STOWs --ivrle-file to both servers, then retrieves the instance and one
    frame and asserts the served transfer syntax. An Implicit VR Little
    Endian instance must come back Explicit VR Little Endian: PS3.18 8.7.3
    forbids IVRLE over Web Services. Metadata 0002,0010 and single-part
    Accept are reported as facts, not verdicts.
    """
    report.suite("syntax")
    if not args.ivrle_file:
        report.info("syntax", "no --ivrle-file given (round-trip skipped)")
        return
    fields = read_dicom_fields(args.ivrle_file)
    if not fields or not fields.get("study") or not fields.get("instance"):
        report.fail("syntax", "cannot read Study/Instance UID from %s" % args.ivrle_file)
        return
    report.info(
        "syntax input",
        "ts=%s study=%s instance=%s"
        % (fields.get("ts") or "?", fields["study"], short_uid(fields["instance"])),
    )
    expect_reencode = fields.get("ts") == IVRLE_TS

    for label, base in (("A", args.a), ("B", args.b)):
        if not base:
            report.info("syntax %s" % label, "no base URL given")
            continue
        status, _headers, body = stow_one(base, args.ivrle_file, args.timeout)
        if status not in (200, 201, 202, 204):
            report.fail("syntax stow %s" % label, "HTTP %d %s" % (status, ascii_text(body[:120])))
            continue
        report.pass_("syntax stow %s" % label, "HTTP %d" % status)

        inst = "%s/studies/%s/series/%s/instances/%s" % (
            base,
            fields["study"],
            fields["series"],
            fields["instance"],
        )

        status, headers, body = http(
            "GET", inst,
            headers={"Accept": 'multipart/related; type="application/dicom"'},
            timeout=args.timeout,
        )
        served = transfer_syntax_of(headers.get("Content-Type", ""), body)
        _report_ts(report, "syntax instance %s" % label, status, body, served, expect_reencode)

        status, headers, body = http(
            "GET", inst + "/frames/1",
            headers={"Accept": 'multipart/related; type="application/octet-stream"'},
            timeout=args.timeout,
        )
        served = transfer_syntax_of(headers.get("Content-Type", ""), body)
        _report_ts(report, "syntax frame %s" % label, status, body, served, expect_reencode)

        status, _headers, body = http(
            "GET", inst + "/metadata",
            headers={"Accept": DICOM_JSON},
            timeout=args.timeout,
        )
        if status == 200:
            try:
                parsed = json.loads(body)
                ds = parsed[0] if isinstance(parsed, list) else parsed
                report.info(
                    "syntax metadata %s" % label,
                    "0002,0010 %s" % ("present" if "00020010" in ds else "absent"),
                )
            except (ValueError, IndexError, KeyError, TypeError) as exc:
                report.fail("syntax metadata %s" % label, "not parseable: %s" % exc)
        else:
            report.info("syntax metadata %s" % label, "HTTP %d" % status)

        status, _headers, _body = http(
            "GET", inst,
            headers={"Accept": "application/dicom"},
            timeout=args.timeout,
        )
        report.info(
            "syntax single-part %s" % label,
            "Accept application/dicom -> HTTP %d" % status,
        )

        # Media-type check runs AFTER the legitimate round-trip: a server that
        # refuses this whole suite still gets its real STOW asserted first.
        check_invalid_media_type(report, label, base, args.ivrle_file, fields, args.timeout)


# -- Suite: wado ---------------------------------------------------------

def run_wado(report, args):
    """WADO-RS diff on the seeded corpus.

    The files STOWed by --seed-dir are fetched back through WADO-RS from both
    servers: /metadata must be 200 + parseable dicom+json carrying the SOP
    Instance UID; /frames/1 must be 200 and declare a transfer syntax. The
    agreement between A and B on the served frame transfer syntax is the diff;
    a disagreement is reported as a fact, not a verdict.
    """
    report.suite("wado")
    if not args.seed_dir:
        report.info("wado", "no --seed-dir corpus (wado skipped)")
        return
    files = seed_files(args.seed_dir, args.seed_glob)
    if not files:
        report.info("wado", "no matching files under %s" % args.seed_dir)
        return
    for path in files[: max(0, args.wado_limit)]:
        fields = read_dicom_fields(path)
        if (
            not fields
            or not fields.get("study")
            or not fields.get("series")
            or not fields.get("instance")
        ):
            report.info(
                "wado %s" % os.path.basename(path),
                "cannot read Study/Series/Instance UID",
            )
            continue
        inst = "/studies/%s/series/%s/instances/%s" % (
            fields["study"],
            fields["series"],
            fields["instance"],
        )
        served = {}
        for label, base in (("A", args.a), ("B", args.b)):
            if not base:
                report.info(
                    "wado %s %s" % (label, os.path.basename(path)), "no base URL"
                )
                continue
            status, _headers, body = http(
                "GET",
                base + inst + "/metadata",
                headers={"Accept": DICOM_JSON},
                timeout=args.timeout,
            )
            meta_ok = False
            if status == 200:
                try:
                    parsed = json.loads(body)
                    ds = parsed[0] if isinstance(parsed, list) else parsed
                    meta_ok = "00080018" in ds
                except (ValueError, IndexError, TypeError):
                    meta_ok = False
            name = "wado metadata %s %s" % (label, os.path.basename(path))
            if status == 200 and meta_ok:
                report.pass_(name, "200 + SOP Instance UID present")
            elif status == 0:
                report.fail(name, ascii_text(body[:120]))
            elif status == 200:
                report.fail(name, "200 but no SOP Instance UID in metadata")
            else:
                report.fail(name, "HTTP %d" % status)

            status, headers, body = http(
                "GET",
                base + inst + "/frames/1",
                headers={"Accept": 'multipart/related; type="application/octet-stream"'},
                timeout=args.timeout,
            )
            ts = transfer_syntax_of(headers.get("Content-Type", ""), body)
            served[label] = ts
            name = "wado frame %s %s" % (label, os.path.basename(path))
            if status == 200 and ts:
                report.pass_(name, "200 ts=%s" % ts)
            elif status == 200:
                report.fail(name, "200 but no transfer-syntax declared")
            elif status == 0:
                report.fail(name, ascii_text(body[:120]))
            else:
                report.fail(name, "HTTP %d" % status)

        if served.get("A") and served.get("B"):
            if served["A"] == served["B"]:
                report.info(
                    "wado ts agree %s" % os.path.basename(path), "%s" % served["A"]
                )
            else:
                report.info(
                    "wado ts DIFFER %s" % os.path.basename(path),
                    "A=%s B=%s" % (served["A"], served["B"]),
                )


# -- Suite: frames -------------------------------------------------------

# How many corpus headers the multi-frame search may read. A corpus prefix
# without a multi-frame instance is reported as skipped; walking an entire
# tree of whole-slide images to prove the absence would cost minutes.
_FRAMES_SCAN_LIMIT = 500


def multipart_part_count(content_type, body):
    """Parts in a multipart/related answer, 1 when it is not multipart.

    Counting is done on the BOUNDARY the response declares, because the body
    is binary: searching for `\\r\\n--` can hit those two bytes inside pixel
    data, while the declared delimiter is unambiguous. A multipart body has
    one more delimiter occurrence (the closing one) than it has parts.
    """
    if "multipart/related" not in (content_type or "").lower():
        return 1
    boundary = ""
    for piece in content_type.split(";"):
        piece = piece.strip()
        if piece.lower().startswith("boundary="):
            boundary = piece.split("=", 1)[1].strip().strip('"')
    if not boundary:
        return 1
    delimiter = b"--" + boundary.encode("ascii", "replace")
    seen = body.count(delimiter)
    # Closing delimiter "--boundary--" is not a part.
    return max(1, seen - 1)


def multi_frame_instance(files):
    """The first corpus file whose Number of Frames is >= 2, or None.

    read_dicom_fields already parses (0028,0008); no second parser is
    introduced for one field. The scan stops after _FRAMES_SCAN_LIMIT files:
    a corpus prefix that carries no multi-frame instance is a "skipped", and
    reading headers of every file in a large tree would turn that into a
    minutes-long walk for no extra answer.
    """
    for path in files[:_FRAMES_SCAN_LIMIT]:
        fields = read_dicom_fields(path)
        if not fields:
            continue
        try:
            if int(fields.get("frames") or 0) >= 2:
                return path, fields
        except (TypeError, ValueError):
            continue
    return None, None


def run_frames(report, args):
    """Frame-list diff (PS3.18 10.4.1.1.6), facts not verdicts.

    The standard defines the URI template `.../frames/{frames}` but no grammar
    for the value: no separator, no range notation, no statement that the
    answer for a list is all-or-none. A comma list therefore works by
    de-facto agreement, so the list answer is reported and never judged -
    the same shape as `wado ts agree` / `wado ts DIFFER`. `/frames/1` IS
    judged, because a single well-known frame number cannot be ambiguous.
    """
    report.suite("frames")
    if not args.seed_dir:
        report.info("frames", "no --seed-dir corpus (frames skipped)")
        return
    files = seed_files(args.seed_dir, args.seed_glob)
    path, fields = multi_frame_instance(files)
    if path is None:
        report.info("frames", "no multi-frame instance in the corpus - skipped")
        return
    report.info(
        "frames instance",
        "%s frames=%s" % (os.path.basename(path), fields.get("frames")),
    )
    inst = "/studies/%s/series/%s/instances/%s" % (
        fields["study"],
        fields["series"],
        fields["instance"],
    )
    results = {}
    for label, base in (("A", args.a), ("B", args.b)):
        if not base:
            report.info("frames %s" % label, "no base URL given")
            continue
        status, _headers, body = http(
            "GET",
            base + inst + "/frames/1",
            headers={"Accept": 'multipart/related; type="application/octet-stream"'},
            timeout=args.timeout,
        )
        name = "frames single %s" % label
        if status == 200:
            report.pass_(name, "200")
        elif status == 0:
            report.fail(name, ascii_text(body[:120]))
        else:
            report.fail(name, "HTTP %d" % status)

        status, headers, body = http(
            "GET",
            base + inst + "/frames/1,2",
            headers={"Accept": 'multipart/related; type="application/octet-stream"'},
            timeout=args.timeout,
        )
        parts = multipart_part_count(headers.get("Content-Type", ""), body)
        results[label] = (status, parts)
        report.info(
            "frames list %s" % label,
            "HTTP %d (%d parts) - comma list is de-facto, not specified by "
            "PS3.18 10.4.1.1.6" % (status, parts),
        )

    if "A" in results and "B" in results:
        (sa, pa), (sb, pb) = results["A"], results["B"]
        if sa == sb and pa == pb:
            report.info("frames list agree", "HTTP %d both" % sa)
        else:
            report.info(
                "frames list DIFFER",
                "A=%d parts=%d  B=%d parts=%d" % (sa, pa, sb, pb),
            )


# -- Suite: series -------------------------------------------------------

# PS3.18 2026d Table 10.6.3-4, Series Resources Search Response Payload.
# Captured 18.11 from the DocBook XHTML at
#   chtml/part18/sect_10.6.3.3.2.html
# "Type" is the PS3.4 C.2.2.1 Query/Retrieve attribute type: R = required,
# U = unique key, C = "shall be present if known", O = optional.
#
# The point of this table: Study Instance UID (0020,000D) is NOT in it. A
# client that reads json[i]['0020000D'].Value without a guard therefore
# breaks against any server that answers /series exactly as specified.
SERIES_RETURN_KEYS = {
    "00080060": ("R", "Modality"),
    "00081190": ("R", "Retrieve URL"),
    "0020000E": ("U", "Series Instance UID"),
    "00200011": ("R", "Series Number"),
    "00201209": ("R", "Number of Series Related Instances"),
}

# Type C keys: present when the server knows the value, absent otherwise.
SERIES_CONDITIONAL_KEYS = {
    "00080201": "Timezone Offset From UTC",
    "0008103E": "Series Description",
    "00400244": "Performed Procedure Step Start Date",
    "00400245": "Performed Procedure Step Start Time",
    "00400275": "Request Attributes Sequence",
}

# Attributes of this table whose VR is numeric, so DICOM JSON must encode
# them as JSON numbers, not strings (PS3.18 2026d F.2.3: IS/DS and the
# binary integer VRs travel as JSON numbers). Checking the shape catches a
# server that gets the value right and the type wrong - which a client that
# does Number() happens to survive, hiding the defect.
SERIES_NUMERIC_KEYS = {
    "00200011": "Series Number (IS)",
    "00201209": "Number of Series Related Instances (IS)",
}

# Study Instance UID, the one key this suite exists for.
STUDY_UID_TAG = "0020000D"


def _series_body(status, body):
    """Decode a /series answer into a list, or None when it is not a list."""
    if status != 200:
        return None
    try:
        parsed = json.loads(body.decode("utf-8", "replace"))
    except (ValueError, UnicodeDecodeError):
        return None
    return parsed if isinstance(parsed, list) else None


def _first_study(base, timeout):
    """One study UID from /studies, for the study-scoped form of the query.

    None when the server has no studies (an empty store is not a failure).
    """
    status, _headers, body = http(
        "GET", base + "/studies", headers={"Accept": "application/dicom+json"},
        timeout=timeout)
    items = _series_body(status, body)
    if not items:
        return None
    return _dig_uid(items[0], STUDY_UID_TAG)


def _dig_uid(item, tag):
    """('Value'[0]) of one DICOM JSON attribute, or None. Never raises.

    This used to demand a str, which turned every conformant numeric
    attribute into a false "missing". DICOM JSON types numbers as JSON
    numbers for the numeric VRs (IS, DS, US, SS, UL, SL, FL, FD, and the
    binary ints - PS3.18 2026d F.2.3), so both shapes are accepted here.
    """
    if not isinstance(item, dict):
        return None
    attr = item.get(tag)
    if not isinstance(attr, dict):
        return None
    values = attr.get("Value")
    if not isinstance(values, list) or not values:
        return None
    first = values[0]
    if isinstance(first, str):
        return first if first else None
    if isinstance(first, int) and not isinstance(first, bool):
        return str(first)
    return None


def _attr_shape(item, tag):
    """How an attribute arrived: 'absent', 'string', 'number' or 'other'.

    Separate from _dig_uid because the JSON *type* of a numeric VR is itself
    worth reporting: an IS attribute encoded as a string is an interop fact
    even when the value is correct.
    """
    if not isinstance(item, dict):
        return "absent"
    attr = item.get(tag)
    if not isinstance(attr, dict):
        return "absent"
    values = attr.get("Value")
    if not isinstance(values, list) or not values:
        return "absent"
    first = values[0]
    if isinstance(first, str):
        return "string"
    if isinstance(first, int) and not isinstance(first, bool):
        return "number"
    return "other"


def run_series(report, args):
    """What an origin actually returns from Search For Series.

    Three checks, and they are deliberately different in kind:

      series required <A/B>   PASS/FAIL against Table 10.6.3-4. A missing R
                              or U key is a conformance failure.
      series study uid   <A/B>   FACT, not a verdict. (0020,000D) is not a
                              return key of this table, so a server may omit
                              it. Printed because a whole class of DICOMweb
                              clients assumes it is there.
      series includefield <A/B>  FACT. Does the origin honour
                              ?includefield=0020000D when asked directly.

    The study-scoped form `/studies/{uid}/series` is checked too, because that
    is the shape a client uses after a study search - and there the origin
    demonstrably knows the Study UID, so omitting it is a choice, not a gap.
    """
    report.suite("series")
    for label, base in (("A", args.a), ("B", args.b)):
        if not base:
            report.info("series %s" % label, "no base URL given")
            continue
        # -- global series search, exactly as a client would ask
        status, _headers, body = http(
            "GET", base + "/series",
            headers={"Accept": "application/dicom+json"},
            timeout=args.timeout)
        items = _series_body(status, body)
        if items is None:
            report.fail("series required %s" % label,
                        "HTTP %d, not a JSON array" % status)
            continue
        if not items:
            report.info("series required %s" % label,
                        "empty store - nothing to inspect")
        else:
            first = items[0]
            missing = [name for tag, (_t, name) in sorted(SERIES_RETURN_KEYS.items())
                       if _dig_uid(first, tag) is None]
            if missing:
                report.fail(
                    "series required %s" % label,
                    "%d of %d series, first item missing: %s"
                    % (len(items), len(items), ", ".join(missing)))
            else:
                report.pass_(
                    "series required %s" % label,
                    "%d series, all of Table 10.6.3-4 R/U keys present"
                    % len(items))
            conditional = [name for tag, name in
                           sorted(SERIES_CONDITIONAL_KEYS.items())
                           if _dig_uid(first, tag) is not None]
            if conditional:
                report.info("series conditional %s" % label,
                            "present: %s" % ", ".join(conditional))

            # Numeric VRs must be JSON numbers, not strings.
            mistyped = []
            for tag, name in sorted(SERIES_NUMERIC_KEYS.items()):
                shape = _attr_shape(first, tag)
                if shape == "string":
                    mistyped.append("%s sent as a JSON string" % name)
            if mistyped:
                report.fail("series numeric vr %s" % label,
                            "; ".join(mistyped))
            elif not [t for t in SERIES_NUMERIC_KEYS
                      if _attr_shape(first, t) == "absent"]:
                report.pass_("series numeric vr %s" % label,
                             "IS attributes are JSON numbers")

        # -- (0020,000D): a fact, because the table does not require it
        study_uid = _dig_uid(items[0], STUDY_UID_TAG) if items else None
        report.info(
            "series study uid %s" % label,
            ("present %s" % short_uid(study_uid))
            if study_uid else
            "ABSENT - not required by Table 10.6.3-4; clients must not "
            "assume it")

        # -- includefield must be honoured when the client asks for it
        status, _headers, body = http(
            "GET",
            query_param(base + "/series", includefield=STUDY_UID_TAG,
                        limit="1"),
            headers={"Accept": "application/dicom+json"},
            timeout=args.timeout)
        asked = _series_body(status, body)
        got = _dig_uid(asked[0], STUDY_UID_TAG) if asked else None
        if status == 200 and got:
            report.pass_("series includefield %s" % label,
                         "0020000D returned (%s)" % short_uid(got))
        elif status == 200 and asked:
            report.fail("series includefield %s" % label,
                        "200 but 0020000D still absent after asking for it")
        else:
            report.info("series includefield %s" % label,
                        "HTTP %d, not judged" % status)

        # -- study-scoped form: the origin KNOWS the study UID here
        scoped = _first_study(base, args.timeout)
        if scoped is None:
            report.info("series study-scoped %s" % label,
                        "no studies on this origin - skipped")
            continue
        status, _headers, body = http(
            "GET",
            base + "/studies/%s/series" % scoped,
            headers={"Accept": "application/dicom+json"},
            timeout=args.timeout)
        scoped_items = _series_body(status, body)
        if scoped_items is None:
            report.info("series study-scoped %s" % label,
                        "HTTP %d, not judged" % status)
        elif not scoped_items:
            report.info("series study-scoped %s" % label,
                        "study %s has no series" % short_uid(scoped))
        else:
            echo = _dig_uid(scoped_items[0], STUDY_UID_TAG)
            if echo == scoped:
                report.pass_("series study-scoped echo %s" % label,
                             "0020000D matches the study in the URL")
            else:
                report.info(
                    "series study-scoped echo %s" % label,
                    "0020000D %s - the study UID is in the request URL, so "
                    "this is a server choice, not a data gap"
                    % ("matches" if echo else "ABSENT"))


# -- Entry point ---------------------------------------------------------

def build_parser():
    parser = argparse.ArgumentParser(
        description=(
            "Compare two DICOMweb servers (UPS-RS, UPS-RS subscriptions, "
            "fuzzymatching, Storage Commitment, transfer-syntax round-trip, "
            "frame lists) and print a PASS/FAIL/INFO report."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "examples:\n"
            "  %(prog)s --suite ups\n"
            "  %(prog)s --suite upsfilter\n"
            "  %(prog)s --suite fuzzy --patient-name <NAME> --patient-name <OTHER>\n"
            "  %(prog)s --suite all --format json --seed-dir D:\\\\criterion\n"
            "\n"
            "Patient names are always operator input: no name is built in.\n"
        ),
    )
    parser.add_argument(
        "--a",
        default="http://127.0.0.1:8024/dicomweb",
        help="base URL prefix of server A (default: Clarus on localhost)",
    )
    parser.add_argument(
        "--b",
        default="http://127.0.0.1:8080/dcm4chee-arc/aets/DCM4CHEE/rs",
        help="base URL prefix of server B (default: dcm4chee-arc on localhost)",
    )
    parser.add_argument("--a-ae", default="CLARUS", help="AE Title of server A")
    parser.add_argument("--b-ae", default="DCM4CHEE", help="AE Title of server B")
    parser.add_argument(
        "--suite",
        choices=[
            "ups",
            "upsfilter",
            "fuzzy",
            "commit",
            "syntax",
            "wado",
            "frames",
            "series",
            "all",
        ],
        default="all",
        help="which suite(s) to run (default: all)",
    )
    parser.add_argument(
        "--format", choices=["text", "json"], default="text", help="output format"
    )
    parser.add_argument(
        "--timeout", type=float, default=30.0, help="per-request timeout in seconds"
    )
    parser.add_argument(
        "--seed-dir",
        help="recursively STOW every matching file from this directory to both servers",
    )
    parser.add_argument(
        "--seed-glob", default="*.dcm", help="file suffix filter for --seed-dir"
    )
    parser.add_argument(
        "--ivrle-file",
        help="an Implicit VR Little Endian .dcm file for the syntax suite",
    )
    parser.add_argument(
        "--wado-limit",
        type=int,
        default=5,
        help="max seeded files to fetch back through WADO-RS (default: 5)",
    )
    parser.add_argument(
        "--patient-name",
        action="append",
        default=[],
        help="base patient name for fuzzy variants (repeatable)",
    )
    parser.add_argument(
        "--variants",
        help="comma-separated explicit query variants (disables generation)",
    )
    parser.add_argument("--tx-uid", help="Transaction UID for commit (default: uuid4)")
    parser.add_argument(
        "--sop-class",
        default=DEFAULT_SOP_CLASS,
        help="referenced SOP Class UID for commit (default: CT Image Storage)",
    )
    parser.add_argument(
        "--sop-instance",
        action="append",
        default=[],
        help="referenced SOP Instance UID for commit (repeatable)",
    )
    return parser


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.variants:
        args.variants = [v.strip() for v in args.variants.split(",") if v.strip()]

    report = Report(args.a, args.b)
    started = time.time()

    if args.seed_dir:
        run_seed(report, args)
    if args.suite in ("ups", "all"):
        run_ups(report, args)
    if args.suite in ("upsfilter", "all"):
        run_upsfilter(report, args)
    if args.suite in ("fuzzy", "all"):
        run_fuzzy(report, args)
    if args.suite in ("commit", "all"):
        run_commit(report, args)
    if args.suite in ("syntax", "all"):
        run_syntax(report, args)
    if args.suite in ("wado", "all"):
        run_wado(report, args)
    if args.suite in ("frames", "all"):
        run_frames(report, args)
    if args.suite in ("series", "all"):
        run_series(report, args)

    _total, _passed, failed = report.counts()
    if args.format == "json":
        payload = report.as_json()
        payload["summary"] = {
            "total": report.counts()[0],
            "pass": report.counts()[1],
            "fail": report.counts()[2],
            "elapsed_s": round(time.time() - started, 2),
        }
        print(json.dumps(payload, indent=2, ensure_ascii=True))
    else:
        print(report.summary_line())
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
