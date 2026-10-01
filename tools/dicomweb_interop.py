#!/usr/bin/env python3
# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
"""Universal DICOMweb interoperability comparison tool.

Runs one identical set of checks against TWO DICOMweb servers and prints a
line-by-line PASS/FAIL/INFO report. Three suites:

  ups     UPS-RS CRUD parity (Create / Create with SOP Common / Search /
          ChangeState / Update), including the two honest wire differences:
          dcm4chee requires ?requester={AE} on ChangeState and spells the
          Update parameter "transaction-uid" (hyphenated).
  fuzzy   QIDO-RS study search with fuzzymatching=true. Clarus uses a bounded
          Levenshtein (1 edit); dcm4chee-arc uses Soundex (1918), which
          degenerates on Cyrillic. The tool prints counts, not verdicts.
  commit  Storage Commitment over DICOMweb (PS3.18 13.4/13.5). Clarus serves
          it; dcm4chee-arc has no route (404/400). Printed as a fact.

The tool is an external client: it only creates the state its own checks need.
It never deletes or mutates anything beyond that.

Only the Python standard library is used, by design (KISS / nginx-way).
No patient names are hard-coded anywhere: names are operator arguments.
"""

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

# Wire constants verified on 2026-10-01 against both servers.
# See analysis/0110_architect_dcm4chee_ups_ab_REPORT.md.
UPS_PUSH_SOP_CLASS = "1.2.840.10008.5.1.4.34.6.1"
DEFAULT_SOP_CLASS = "1.2.840.10008.5.1.4.1.1.2"  # CT Image Storage
DICOM_JSON = "application/dicom+json"

# The Update query parameter (PS3.18 11.6) is spelled differently by the two
# servers, verified live 01.10:
#   Clarus   accepts ONLY "TransactionUid" (case-sensitive; the hyphenated
#            spelling is answered 400 "Missing required query parameter").
#   dcm4chee accepts either spelling.
# The tool drives each server with the spelling that server accepts. This is
# a wire fact, not a preference: see analysis/0110_architect_dcm4chee_ups_ab_REPORT.md.
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


def stow_one(base, path, timeout):
    """STOW one file as a single-part multipart/related body.

    multipart/related is used rather than a bare `application/dicom` part
    because it is the ONE media type both servers accept: Clarus takes
    either, dcm4chee-arc answers a single-part POST with 415. The file bytes
    are sent verbatim - nothing is parsed on this side.
    """
    with open(path, "rb") as handle:
        blob = handle.read()
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
    status, _headers, body = http(
        "GET",
        query_param(base + "/workitems", **{"00404005": ae_title}),
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
    found = 0
    for item in items:
        value = item.get("00080018", {}).get("Value", [])
        if value and value[0] == uid:
            found += 1
    if found:
        report.pass_(name, "workitem %s found among %d result(s)" % (short_uid(uid), len(items)))
    else:
        report.fail(name, "workitem %s not present among %d result(s)" % (short_uid(uid), len(items)))


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
        check_change_state(
            report,
            label,
            base,
            ae,
            uid,
            new_uid(),
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
            report.pass_(name, "HTTP 200, %d bytes" % len(payload))
        elif status == 0:
            report.fail(name, ascii_text(payload[:160]))
        elif status in (404, 405, 400):
            report.info(name, "HTTP %d (no Storage Commitment over DICOMweb)" % status)
        else:
            report.fail(name, "HTTP %d %s" % (status, ascii_text(payload[:160])))


# -- Entry point ---------------------------------------------------------

def build_parser():
    parser = argparse.ArgumentParser(
        description=(
            "Compare two DICOMweb servers (UPS-RS, fuzzymatching, Storage "
            "Commitment over DICOMweb) and print a PASS/FAIL/INFO report."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "examples:\n"
            "  %(prog)s --suite ups\n"
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
        choices=["ups", "fuzzy", "commit", "all"],
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
    if args.suite in ("fuzzy", "all"):
        run_fuzzy(report, args)
    if args.suite in ("commit", "all"):
        run_commit(report, args)

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
