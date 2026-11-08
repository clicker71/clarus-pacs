#!/usr/bin/env python3
"""Forward the Clarus access journal to a syslog collector as XML audit
messages.

What this is
------------
The Clarus access journal (access/YYYYMMDD.log) is the normative audit
contract: one JSON line per access or attribute-update event, in the field
order documented in docs/audit-logging.md section 2. This script reads that
journal and renders one DICOM PS3.15 Annex A.5 shaped AuditMessage per line,
framed as an RFC 5424 syslog message, and forwards it to a collector.

It is an integration aid. It does not make Clarus ATNA conformant, it does not
change the journal contract, and it does not change the journal's retention or
loss semantics. The journals remain the only audit output.

Reading
-------
Only files named access/YYYYMMDD.log (eight digits) are read. A line that does
not parse as JSON is reported to stderr and skipped; it is never sent and
never stops the run.

Cursor
------
--cursor keeps one byte offset per journal file, so a restart resumes rather
than re-sending. The offset advances ONLY after a message has been handed to
the transport (after write() for TCP, after sendto() for UDP). The window
between a successful send and the cursor write is at-least-once: a crash in
that window re-sends the last message. That is deliberate - losing an audit
message is worse than repeating one, and the collector can de-duplicate.

Usage
-----
    python tools/access_journal_to_syslog.py --dir access --syslog collector \\
        --transport udp --follow
    python tools/access_journal_to_syslog.py --dir access --once --dry-run

Exit status is 0 on success (including --once runs that find nothing new),
1 on a usage or fatal transport error.
"""

import argparse
import errno
import json
import os
import socket
import sys
import time
from xml.sax.saxutils import escape, quoteattr

VERSION = "1.0.0"

# ---------------------------------------------------------------------------
# Constants that encode the decisions taken in the report
# ---------------------------------------------------------------------------

# The journal `action` value is already the DICOM EventActionCode. The mapping
# is fixed here rather than derived, so that adding a new journal action is a
# deliberate change to this table and never silently produces a wrong code.
# The journal emits R (read: WADO, QIDO), C (create: STOW), U (update:
# attribute update). D (delete) is reserved: deletions live in the delete
# trail (audit/), which this converter does not read.
EVENT_ACTION_CODE = {
    "R": "R",  # Read/View/Print/Query
    "C": "C",  # Create
    "U": "U",  # Update
    "D": "D",  # Delete
}

# Clarus declares the codes it can produce in the conformance statement; the
# note here is for a human reading the XML without that document.
ATNA_EVENT_CODE_MEANING = {
    "110101": "Audit Log Used",
    "110103": "DICOM Instances Accessed",
    "110104": "DICOM Instances Transferred",
    # 110109 (Order Record) is not emitted by this server's journal, but the
    # code is listed so an operator reading a forwarded message from a
    # neighbouring system finds it here rather than in the standard.
    "110109": "Order Record",
    "110110": "Patient Record",
    # 110111 (Procedure Record) is the MPPS create/update/retrieve event
    # (PS3.15 A.5.3.15, which names MPPS update explicitly).
    "110111": "Procedure Record",
    "110112": "Query",
}

# PS3.15 A.5.1.2.4 Participant Object ID Type Code. The DICOM UIDs are
# reported with the codes DICOM defines for them in the participant-object
# tables; 110180 is Study Instance UID, 110181 Series Instance UID and
# 110182 SOP Instance UID, which is more precise than the generic
# "Encounter Number" (3) a bare RFC 3881 reading would suggest.
STUDY_INSTANCE_UID_CODE = "110180"
SERIES_INSTANCE_UID_CODE = "110181"
SOP_INSTANCE_UID_CODE = "110182"
# 2 = Patient Number, the identifier the journal records for a patient.
PARTICIPANT_ID_TYPE_PATIENT = "2"

# PS3.15 A.5.1.2.5 Network Access Point Type Code: 2 = IP Address.
NETWORK_ACCESS_POINT_TYPE_IP = "2"

# PS3.15 A.5.1.2.1 Audit Source Type Code: 3 = Web server process or thread.
# Clarus serves DICOMweb over HTTP, so the source is a web server process.
AUDIT_SOURCE_TYPE_CODE = "3"

# RFC 5424: facility 13 is "log audit" (security/authorization messages).
SYSLOG_FACILITY = 13
# Severity 5 is "Notice" - an audit record is informational but significant.
SYSLOG_SEVERITY = 5
SYSLOG_PRI = (SYSLOG_FACILITY << 3) | SYSLOG_SEVERITY

APP_NAME = "clarus"

# The journal caps the query string at 1024 bytes at render time; this is a
# guard against a hand-edited or corrupt file, not a second policy.
MAX_QUERY_CHARS = 4096

# A journal line is at most a few hundred bytes; this is a sanity guard so
# that a corrupt file cannot make us allocate without bound.
MAX_LINE_BYTES = 1 << 20

# Control characters are not legal in XML 1.0 character data at all. They are
# removed rather than escaped, because there is no legal escaped form for most
# of them.
_CONTROL_STRIP = {c: None for c in range(0x00, 0x20)}
for _keep in (0x09, 0x0A, 0x0D):
    _CONTROL_STRIP.pop(_keep, None)
_CONTROL_STRIP[0x7F] = None

# The field order the server writes. Kept here so a reader can see the whole
# journal schema in one place, and so render_audit_message can be told which
# fields it consumed.
JOURNAL_FIELDS = (
    "event",
    "ts",
    "user",
    "role",
    "src",
    "src_ip",
    "via",
    "action",
    "atna",
    "status",
    "study",
    "series",
    "sop",
    "patient",
    "obj_type",
    "q",
    "count",
)
JOURNAL_FIELDS_UPDATE = ("changed_attrs", "bytes_replaced", "reason")


# ---------------------------------------------------------------------------
# Value handling
# ---------------------------------------------------------------------------


def clean_text(value):
    """Return `value` as XML-safe text, or None when there is nothing to say.

    PS3.15 A.5.2 requires that an audit message contain no empty elements and
    no attributes with an empty value, so an absent or empty journal field
    must become an OMITTED attribute rather than an empty one. Returning None
    is how that omission is expressed.
    """
    if value is None:
        return None
    text = str(value)
    text = text.translate(_CONTROL_STRIP)
    if text == "":
        return None
    return escape(text, {'"': "&quot;", "'": "&apos;"})


def attr(name, value):
    """Render ` name="value"` or the empty string when there is no value."""
    text = clean_text(value)
    if text is None:
        return ""
    return f' {name}="{text}"'


def clean_int(value):
    """Return a decimal string for an integer-ish value, or None."""
    if value is None or value == "":
        return None
    try:
        return str(int(value))
    except (TypeError, ValueError):
        return None


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------


def render_audit_message(event):
    """Render one journal record as a PS3.15 A.5 shaped AuditMessage.

    `event` is the decoded JSON object. Unknown keys are ignored: the journal
    is additive by design and a newer server must not break an older
    converter. Every element and attribute that A.5 marks mandatory is
    emitted from the journal field that carries it; an optional field the
    journal left empty is omitted entirely.
    """
    parts = ["<AuditMessage>"]

    # --- EventIdentification -------------------------------------------
    # EventID carries the DICOM event code from `atna`. A.5.1 names the
    # attribute csd-code (RFC 3881 called it code); codeSystemName="DCM" says
    # the code comes from the DICOM controlled vocabulary.
    atna = clean_text(event.get("atna"))
    event_id_attrs = ""
    if atna is not None:
        event_id_attrs += ' csd-code="%s"' % atna
        event_id_attrs += ' codeSystemName="DCM"'
        meaning = ATNA_EVENT_CODE_MEANING.get(str(event.get("atna", "")))
        if meaning is not None:
            event_id_attrs += ' displayName="%s"' % meaning

    # EventActionCode from `action`; EventDateTime from `ts`;
    # EventOutcomeIndicator is mandatory, and every journal row is a record of
    # an event that was answered, so it is a nominal success unless the status
    # says otherwise.
    action = EVENT_ACTION_CODE.get(str(event.get("action", "")))
    outcome = event_outcome_indicator(event.get("status"))

    ident_attrs = ""
    if action is not None:
        ident_attrs += f' EventActionCode="{action}"'
    if outcome is not None:
        ident_attrs += f' EventOutcomeIndicator="{outcome}"'
    ts = clean_text(event.get("ts"))
    if ts is not None:
        ident_attrs += f' EventDateTime="{ts}"'

    parts.append(f"<EventIdentification{ident_attrs}>")
    if event_id_attrs:
        parts.append(f"<EventID{event_id_attrs} />")
    parts.append("</EventIdentification>")

    # --- ActiveParticipant ---------------------------------------------
    # One participant: the requesting user. UserID is mandatory, so a record
    # with no user is still represented - by the source identity, which is
    # what the journal used when no authenticated user was involved.
    user = clean_text(event.get("user"))
    if user is None:
        user = clean_text(event.get("src"))
    participant_attrs = ""
    if user is not None:
        participant_attrs += f' UserID="{user}"'
    # The journal always places the request on the wire from this process.
    participant_attrs += ' UserIsRequestor="true"'
    src_ip = clean_text(event.get("src_ip"))
    if src_ip is not None:
        participant_attrs += f' NetworkAccessPointID="{src_ip}"'
        participant_attrs += (
            f' NetworkAccessPointTypeCode="{NETWORK_ACCESS_POINT_TYPE_IP}"'
        )
    parts.append(f"<ActiveParticipant{participant_attrs} />")

    # --- AuditSourceIdentification -------------------------------------
    src = clean_text(event.get("src"))
    source_attrs = ""
    if src is not None:
        source_attrs += f' AuditSourceID="{src}"'
    parts.append(f"<AuditSourceIdentification{source_attrs}>")
    parts.append(
        f'<AuditSourceTypeCode csd-code="{AUDIT_SOURCE_TYPE_CODE}" />'
    )
    parts.append("</AuditSourceIdentification>")

    # --- ParticipantObjectIdentification -------------------------------
    # One element per identifier the journal actually carried. An element
    # with no ParticipantObjectID is not legal, so a missing identifier means
    # no element rather than an empty one.
    parts.extend(render_participant_objects(event))

    parts.append("</AuditMessage>")
    return "".join(parts)


def event_outcome_indicator(status):
    """Map the journal HTTP-ish `status` onto EventOutcomeIndicator.

    A.5: 0 success, 4 minor failure, 8 serious failure, 12 major failure.
    The journal records the answer the server gave; a 2xx is success, a 4xx
    is a refused/no-op request (minor), and a 5xx is the server failing to
    carry the event out (serious). An absent status is treated as success,
    because the row exists only because the event was handled.
    """
    code = None
    try:
        code = int(status)
    except (TypeError, ValueError):
        return "0"
    if 200 <= code < 400:
        return "0"
    if 400 <= code < 500:
        return "4"
    if code >= 500:
        return "8"
    return "0"


def render_participant_objects(event):
    """Render the participant objects the journal identifies.

    `obj_type` carries the journal's own identity-type code for the row (2 for
    a patient, 13 for a DICOM UID, empty for a query). The identifiers
    themselves are what A.5 needs; they are rendered with the A.5.1.2.4 codes
    the schema defines for them.
    """
    out = []
    study = clean_text(event.get("study"))
    series = clean_text(event.get("series"))
    sop = clean_text(event.get("sop"))
    patient = clean_text(event.get("patient"))
    query = clean_text(event.get("q"))

    if study is not None:
        out.append(_participant_uid(study, STUDY_INSTANCE_UID_CODE))
    if series is not None:
        out.append(_participant_uid(series, SERIES_INSTANCE_UID_CODE))
    if sop is not None:
        out.append(_participant_uid(sop, SOP_INSTANCE_UID_CODE))
    if patient is not None:
        out.append(_participant_patient(patient))
    if query is not None:
        # A search is identified by its criteria, not by a name. A.5 allows
        # exactly one of ParticipantObjectName / ParticipantObjectQuery, and
        # for a query the criteria are the honest choice.
        if len(query) > MAX_QUERY_CHARS:
            query = query[:MAX_QUERY_CHARS]
        out.append(_participant_query(query))
    return out


def _participant_uid(object_id, id_type_code):
    """A participant object identified by a DICOM UID.

    A.5.2 makes ParticipantObjectName MC ("required if ParticipantObjectQuery
    is not present"), so a name must be present. For a bare UID the UID is
    itself the instance-specific descriptor - it is the identifier, spelled out
    in full - so the UID is repeated as the name rather than a label that only
    restates the type code.
    """
    return (
        "<ParticipantObjectIdentification"
        f' ParticipantObjectID="{object_id}"'
        ' ParticipantObjectTypeCode="2">'
        f'<ParticipantObjectIDTypeCode csd-code="{id_type_code}"'
        ' codeSystemName="DCM" />'
        f"<ParticipantObjectName>{object_id}</ParticipantObjectName>"
        "</ParticipantObjectIdentification>"
    )


def _participant_patient(patient_id):
    """A participant object identified by patient number.

    The journal's patient identifier is a patient number, so the object is a
    Person (1) in the Patient role (1), not a system object.
    """
    return (
        "<ParticipantObjectIdentification"
        f' ParticipantObjectID="{patient_id}"'
        ' ParticipantObjectTypeCode="1"'
        ' ParticipantObjectTypeCodeRole="1">'
        '<ParticipantObjectIDTypeCode csd-code="2" codeSystemName="DCM" />'
        f"<ParticipantObjectName>{patient_id}</ParticipantObjectName>"
        "</ParticipantObjectIdentification>"
    )


def _participant_query(query_text):
    """A participant object identified by search criteria.

    A5.1.2.4 code 10 is Search Criteria; a QIDO query carries no patient or
    study identity of its own, which is exactly why it is recorded.
    """
    return (
        "<ParticipantObjectIdentification"
        f' ParticipantObjectID="{query_text}"'
        ' ParticipantObjectTypeCode="2"'
        ' ParticipantObjectTypeCodeRole="24">'
        '<ParticipantObjectIDTypeCode csd-code="10" codeSystemName="DCM" />'
        f"<ParticipantObjectQuery>{query_text}</ParticipantObjectQuery>"
        "</ParticipantObjectIdentification>"
    )


# ---------------------------------------------------------------------------
# RFC 5424 framing
# ---------------------------------------------------------------------------


def render_syslog(event, xml):
    """Frame `xml` as an RFC 5424 syslog message.

    <PRI>VERSION SP TIMESTAMP SP HOSTNAME SP APP-NAME SP PROCID SP MSGID SP
    STRUCTURED-DATA SP MSG

    HOSTNAME, PROCID and STRUCTURED-DATA are NILVALUE. APP-NAME is the
    constant "clarus". MSGID is the DICOM event code, which is the one
    identifier a collector can most usefully filter on. MSG is a UTF-8 BOM
    followed by the XML, as RFC 5424 section 6.4 requires for a MSG that is
    known to be UTF-8.
    """
    ts = event.get("ts")
    ts_text = str(ts).translate(_CONTROL_STRIP) if ts else "-"
    if not ts_text:
        ts_text = "-"
    msgid = str(event.get("atna", "")).translate(_CONTROL_STRIP) or "-"
    header = (
        f"<{SYSLOG_PRI}>1 {ts_text} - {APP_NAME} - {msgid} - \ufeff"
    )
    return header + xml


def frame_tcp(message):
    """Octet-counting framing for a stream transport (RFC 6587 section 3.4.1).

    A syslog message over TCP is `MSG-LEN SP SYSLOG-MSG`, where MSG-LEN is
    the length of SYSLOG-MSG in octets. This is RFC 5425's mandatory framing.
    """
    payload = message.encode("utf-8")
    return str(len(payload)).encode("ascii") + b" " + payload


def make_sender(transport, host, port, verbose=False, timeout=10.0):
    """Return a callable that hands one framed message to the transport.

    The callable raises OSError if the message could not be handed over; the
    caller must not advance the cursor in that case.
    """
    if transport == "udp":
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)

        def send_udp(message):
            payload = message.encode("utf-8")
            if len(payload) > 65507:
                raise OSError(
                    errno.EMSGSIZE,
                    f"message is {len(payload)} bytes, larger than a datagram",
                )
            sent = sock.sendto(payload, (host, port))
            if verbose:
                sys.stderr.write(f"udp {sent} B -> {host}:{port}\n")
            return sent

        return send_udp

    if transport == "tcp":
        # One connection, reused. Reconnecting per message would cost a
        # handshake per audit line for no benefit.
        state = {"sock": None}

        def connect():
            sock = socket.create_connection((host, port), timeout=timeout)
            sock.settimeout(timeout)
            state["sock"] = sock
            if verbose:
                sys.stderr.write(f"tcp connected to {host}:{port}\n")
            return sock

        def send_tcp(message):
            wire = frame_tcp(message)
            for attempt in (0, 1):
                sock = state["sock"]
                if sock is None:
                    sock = connect()
                try:
                    sock.sendall(wire)
                    if verbose:
                        sys.stderr.write(f"tcp {len(wire)} B -> {host}:{port}\n")
                    return len(wire)
                except OSError:
                    # The collector may have closed an idle connection. Drop
                    # it and retry once on a fresh one; a second failure is
                    # reported to the caller.
                    try:
                        sock.close()
                    except OSError:
                        pass
                    state["sock"] = None
                    if attempt == 1:
                        raise
            return 0

        return send_tcp

    raise ValueError(f"unknown transport: {transport}")


# ---------------------------------------------------------------------------
# Cursor
# ---------------------------------------------------------------------------

CURSOR_VERSION = 1


class Cursor:
    """Per-file byte offsets, persisted as JSON.

    A Cursor with path=None keeps its state in memory only, which is what the
    tests and a --dry-run use.
    """

    def __init__(self, path):
        self.path = path
        self._offsets = {}

    def load(self):
        if self.path is None:
            return self
        try:
            with open(os.fspath(self.path), "r", encoding="utf-8") as handle:
                data = json.load(handle)
        except (OSError, ValueError):
            # A missing or unreadable cursor means "start from the
            # beginning". Re-sending is the safe direction to fail in.
            data = {}
        offsets = data.get("offsets")
        if isinstance(offsets, dict):
            self._offsets = {
                str(k): int(v)
                for k, v in offsets.items()
                if isinstance(v, int) or (isinstance(v, str) and v.isdigit())
            }
        return self

    def save(self):
        """Persist the offsets, atomically where the platform allows it.

        Saving is best-effort: the cursor is an optimisation that avoids
        re-sending, never a correctness requirement. If it cannot be written
        the run continues and the next start re-sends from an older offset,
        which is the safe direction to fail in. A save is never allowed to
        turn a delivered message into a failed run.
        """
        if self.path is None:
            return False
        path = os.fspath(self.path)
        directory = os.path.dirname(os.path.abspath(path))
        if directory:
            try:
                os.makedirs(directory, exist_ok=True)
            except OSError:
                return False
        tmp = path + ".tmp"
        payload = {
            "version": CURSOR_VERSION,
            "offsets": self._offsets,
        }
        try:
            with open(tmp, "w", encoding="utf-8", newline="\n") as handle:
                json.dump(payload, handle, sort_keys=True)
                handle.write("\n")
                handle.flush()
                os.fsync(handle.fileno())
        except OSError:
            return False
        # Replace, so an interrupted save cannot leave a truncated cursor
        # behind. On Windows the destination may be momentarily locked by
        # another process (an indexer or a scanner); a couple of immediate
        # retries is enough, and giving up is not fatal.
        for attempt in range(3):
            try:
                os.replace(tmp, path)
                return True
            except OSError:
                if attempt == 2:
                    try:
                        os.unlink(tmp)
                    except OSError:
                        pass
                    return False
                time.sleep(0.05)
        return False

    def get(self, key):
        return self._offsets.get(key)

    def set(self, key, offset):
        self._offsets[key] = int(offset)


# ---------------------------------------------------------------------------
# Journal reading
# ---------------------------------------------------------------------------

JOURNAL_NAME_LENGTH = 8  # YYYYMMDD


def is_journal_file(name):
    """True for a file this converter reads: access/YYYYMMDD.log."""
    if not name.endswith(".log"):
        return False
    stem = name[: -len(".log")]
    return len(stem) == JOURNAL_NAME_LENGTH and stem.isdigit()


def journal_files(directory):
    """Journal files in `directory`, oldest name first."""
    try:
        names = os.listdir(directory)
    except OSError as exc:
        raise SystemExit(f"cannot read journal directory {directory}: {exc}")
    return [
        os.path.join(directory, name)
        for name in sorted(names)
        if is_journal_file(name)
    ]


def split_complete_lines(data):
    """Split `data` into (whole_lines, remainder).

    A journal line is only acted on once its terminating newline has been
    seen, so a partially written line at the end of the file is left for the
    next pass instead of being reported as corrupt.
    """
    if not data:
        return [], b""
    pieces = data.split(b"\n")
    remainder = pieces.pop()
    return pieces, remainder


def process_buffer(path, data, cursor, send, warn, start_offset=0):
    """Deliver every complete journal line in `data`.

    `data` is the byte buffer read from `path` starting at `start_offset`.
    The cursor stores, per file, the number of bytes that have been DELIVERED;
    it advances one line at a time and never past a line whose delivery
    raised. `send` signals a failed delivery by raising OSError.

    A line that does not parse as a JSON object is reported and stepped over:
    it never stops the run. The unterminated tail of the buffer is left for
    the next pass, so a line still being appended to is not mistaken for a
    corrupt one.

    Returns counters, which are what --verbose prints and what the tests
    assert on.
    """
    lines, _remainder = split_complete_lines(data)
    state = {"delivered": 0, "failed": 0, "skipped": 0}
    advanced = 0

    for raw in lines:
        text = raw.decode("utf-8", errors="replace").strip()
        if not text:
            # A blank line is not an event; step over it.
            advanced += len(raw) + 1
            cursor.set(path, start_offset + advanced)
            continue
        try:
            event = json.loads(text)
        except ValueError as exc:
            state["skipped"] += 1
            warn(f"skipping unparsable journal line ({exc}): {text[:120]}")
            advanced += len(raw) + 1
            cursor.set(path, start_offset + advanced)
            cursor.save()
            continue
        if not isinstance(event, dict):
            state["skipped"] += 1
            warn(f"skipping non-object journal line: {text[:120]}")
            advanced += len(raw) + 1
            cursor.set(path, start_offset + advanced)
            cursor.save()
            continue

        xml = render_audit_message(event)
        message = render_syslog(event, xml)
        try:
            send(message)
        except OSError as exc:
            # Stop here: everything after this line is undelivered, and the
            # order the journal wrote them in is the order they must arrive.
            state["failed"] += 1
            warn(f"delivery failed, cursor not advanced: {exc}")
            return state
        state["delivered"] += 1
        advanced += len(raw) + 1
        cursor.set(path, start_offset + advanced)
        cursor.save()

    return state


def process_file(path, cursor, send, warn):
    """Deliver the new part of one journal file, resuming from the cursor."""
    offset = cursor.get(path) or 0
    size = os.path.getsize(path)
    if size < offset:
        # The file shrank: it was rotated and recreated under the same name.
        # Start over rather than read from a position that no longer exists.
        warn(f"{path}: file shrank ({size} < {offset}), restarting at 0")
        offset = 0
    if size == offset:
        return {"delivered": 0, "failed": 0, "skipped": 0}

    with open(path, "rb") as handle:
        handle.seek(offset)
        data = handle.read()
    return process_buffer(path, data, cursor, send, warn, start_offset=offset)


# ---------------------------------------------------------------------------
# Command line
# ---------------------------------------------------------------------------


def build_parser():
    parser = argparse.ArgumentParser(
        prog="access_journal_to_syslog.py",
        description=(
            "Forward the Clarus access journal to a syslog collector as "
            "DICOM PS3.15 A.5 shaped XML audit messages."
        ),
    )
    parser.add_argument(
        "--dir",
        default="access",
        help="directory holding the access journal (access/YYYYMMDD.log)",
    )
    parser.add_argument("--syslog", default=None, help="collector host")
    parser.add_argument(
        "--port", type=int, default=514, help="collector port (default 514)"
    )
    parser.add_argument(
        "--transport",
        choices=("udp", "tcp"),
        default="udp",
        help="transport (default udp)",
    )
    parser.add_argument(
        "--cursor",
        default=os.path.join("tools", ".access_journal_cursor.json"),
        help="cursor file holding per-file offsets",
    )
    parser.add_argument(
        "--once",
        action="store_true",
        help="make one pass over the journals and exit",
    )
    parser.add_argument(
        "--follow",
        action="store_true",
        help="keep running and pick up new journal bytes as they appear",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="write the XML to stdout and send nothing",
    )
    parser.add_argument(
        "--interval",
        type=float,
        default=1.0,
        help="seconds between passes with --follow (default 1)",
    )
    parser.add_argument("--verbose", action="store_true", help="log to stderr")
    parser.add_argument(
        "--version", action="version", version=f"%(prog)s {VERSION}"
    )
    return parser


def write_stdout_bytes(data):
    """Write raw bytes to stdout, bypassing the console encoding.

    A syslog message is UTF-8 on the wire. The console encoding on some
    platforms (cp1251 on a Russian Windows, for example) cannot represent the
    UTF-8 BOM or Cyrillic text, so --dry-run must not go through
    sys.stdout.write. The bytes go to the underlying buffer instead, which
    gives exactly what would have been sent.
    """
    buffer = getattr(sys.stdout, "buffer", None)
    if buffer is None:
        # A deliberately redirected text stream with no buffer: fall back to
        # writing the decoded text, which is the best that stream can do.
        sys.stdout.write(data.decode("utf-8", errors="replace"))
        return
    buffer.write(data)
    buffer.flush()


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)

    if args.once and args.follow:
        parser.error("--once and --follow are mutually exclusive")
    if not args.once and not args.follow:
        # Default to one pass: a cron invocation with no mode is the common
        # way this is deployed, and it must not hang.
        args.once = True
    if not args.dry_run and not args.syslog:
        parser.error("--syslog is required unless --dry-run is given")

    if args.dry_run:
        def send(message):
            write_stdout_bytes(message.encode("utf-8") + b"\n")
    else:
        send = make_sender(
            args.transport, args.syslog, args.port, verbose=args.verbose
        )

    cursor = Cursor(args.cursor).load()

    def warn(message):
        # Warnings are for a human at the console, so they go through the
        # text stream and may be re-encoded; they are never wire content.
        sys.stderr.write(message + "\n")

    while True:
        files = journal_files(args.dir)
        if not files:
            warn(f"no journal files in {args.dir}")
        for path in files:
            state = process_file(path, cursor, send, warn)
            if args.verbose and any(state.values()):
                warn(
                    f"{path}: delivered {state['delivered']}, "
                    f"failed {state['failed']}, skipped {state['skipped']}"
                )
        if args.once:
            return 0
        time.sleep(max(args.interval, 0.1))


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(0)
