#!/usr/bin/env python3
"""Tests for tools/access_journal_to_syslog.py.

Plain asserts, no pytest: run `python tools/test_access_journal_to_syslog.py`.
Exit 0 means every check passed.

The converter reads the NORMATIVE Clarus access-journal JSON-lines schema
(docs/audit-logging.md section 2) and renders one DICOM PS3.15 Annex A.5
shaped AuditMessage per line, framed as an RFC 5424 syslog message.
"""

import importlib.util
import io
import json
import os
import pathlib
import socket
import sys

HERE = pathlib.Path(__file__).resolve().parent
SCRIPT = HERE / "access_journal_to_syslog.py"

# The field sample used by the size measurements in the architect report;
# absence is fine, the test that needs it skips.
FIELD_SAMPLE = (
    HERE.parent
    / "fieldtest"
    / "clarus"
    / "data-2609-prev"
    / "access"
    / "20260924.log"
)


def load_module():
    """Import the converter by path (it is a script, not a package)."""
    spec = importlib.util.spec_from_file_location("access_journal_to_syslog", SCRIPT)
    if spec is None or spec.loader is None:
        raise AssertionError(f"cannot load {SCRIPT}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


# One full access line, all fields present, exactly as the server renders it.
FULL_LINE = (
    '{"event":"access","ts":"2026-09-24T10:16:43Z","user":"anonymous",'
    '"role":"","src":"clarus-server","src_ip":"172.20.41.80","via":"bridge",'
    '"action":"R","atna":"110112","status":200,'
    '"study":"1.2.840.113857.10.2026.9.22.4",'
    '"series":"1.2.840.113857.10.2026.9.22.4.1",'
    '"sop":"1.2.840.113857.10.2026.9.22.4.1.1",'
    '"patient":"PAT-1","obj_type":"13",'
    '"q":"StudyInstanceUID=1.2.840.113857.10.2026.9.22.4","count":1}'
)

# An attribute-update line: the U action plus its three extra fields.
UPDATE_LINE = (
    '{"event":"attribute_update","ts":"2026-09-24T11:00:00Z","user":"admin",'
    '"role":"","src":"clarus-server","src_ip":"10.0.0.5","via":"nginx",'
    '"action":"U","atna":"110104","status":200,"study":"","series":"","sop":"",'
    '"patient":"","obj_type":"","q":"","count":1,'
    '"changed_attrs":3,"bytes_replaced":1,"reason":"hidden_phi_excised"}'
)


def test_red_fact_module_exists():
    """RED FACT: the converter must exist before anything else can pass.

    Before the script was written this aborted with "cannot load
    tools/access_journal_to_syslog.py"; every other test below depends on it.
    """
    assert SCRIPT.exists(), f"the converter is missing: {SCRIPT}"


def test_full_line_renders_the_a5_skeleton():
    """A full line yields an AuditMessage with the event identity filled in."""
    m = load_module()
    xml = m.render_audit_message(json.loads(FULL_LINE))

    assert xml.startswith("<AuditMessage"), xml[:80]
    assert xml.rstrip().endswith("</AuditMessage>")

    # EventIdentification: the DICOM event code from the `atna` field.
    assert "<EventIdentification" in xml
    assert 'csd-code="110112"' in xml, xml
    assert 'codeSystemName="DCM"' in xml, xml
    assert "<EventID" in xml
    # EventActionCode comes from `action`.
    assert 'EventActionCode="R"' in xml, xml
    # EventDateTime comes from `ts`.
    assert 'EventDateTime="2026-09-24T10:16:43Z"' in xml, xml
    # EventOutcomeIndicator is mandatory in A.5: status 200 is a success.
    assert 'EventOutcomeIndicator="0"' in xml, xml

    # ActiveParticipant: the user and the network access point.
    assert "<ActiveParticipant" in xml
    assert "UserID=" in xml
    assert 'NetworkAccessPointID="172.20.41.80"' in xml
    assert 'NetworkAccessPointTypeCode="2"' in xml, "an IP is type 2"

    # AuditSourceIdentification from `src`.
    assert "<AuditSourceIdentification" in xml
    assert 'AuditSourceID="clarus-server"' in xml

    # ParticipantObjectIdentification: one per present identifier, identified
    # by the DICOM codes for each UID kind.
    assert "<ParticipantObjectIdentification" in xml
    assert 'ParticipantObjectID="1.2.840.113857.10.2026.9.22.4"' in xml
    assert 'csd-code="110180"' in xml, "study instance UID"
    assert 'csd-code="110181"' in xml, "series instance UID"
    assert 'csd-code="110182"' in xml, "SOP instance UID"
    assert "<ParticipantObjectIDTypeCode" in xml
    # ParticipantObjectName is mandatory-if-no-query, so the four identifier
    # objects carry a name while the query object carries the criteria.
    assert xml.count("<ParticipantObjectName>") == 4, xml
    assert xml.count("<ParticipantObjectQuery>") == 1, xml


def test_action_maps_to_event_action_code():
    """R/C/U/D map onto the A.5 EventActionCode enumeration."""
    m = load_module()
    assert m.EVENT_ACTION_CODE["R"] == "R"
    assert m.EVENT_ACTION_CODE["C"] == "C"
    assert m.EVENT_ACTION_CODE["U"] == "U"
    assert m.EVENT_ACTION_CODE["D"] == "D"
    # A query is a Read, which is what the server records for QIDO.
    for code in ("R", "C", "U", "D"):
        assert code in m.EVENT_ACTION_CODE


def test_attribute_update_line_renders():
    """A U line renders, and its extra counters do not break the XML."""
    m = load_module()
    xml = m.render_audit_message(json.loads(UPDATE_LINE))
    assert 'EventActionCode="U"' in xml, xml
    assert 'csd-code="110104"' in xml, xml
    assert "<AuditMessage" in xml


def test_no_empty_elements_or_attribute_values():
    """PS3.15 A.5.2: audit messages carry no empty elements and no empty
    attribute values. A field the journal left empty must be OMITTED."""
    m = load_module()
    # This line has empty study/series/sop/patient/q - the common case.
    xml = m.render_audit_message(json.loads(UPDATE_LINE))
    assert '=""' not in xml, f"an empty attribute value survived: {xml}"
    assert "<ParticipantObjectIdentification" not in xml, (
        "no participant identifiers were present, so no such element"
    )


def test_escaping_of_quotes_angles_and_control_characters():
    """Dynamic values are XML-escaped; control characters cannot break the
    document."""
    m = load_module()
    line = json.loads(FULL_LINE)
    line["user"] = 'a"b<c>&d'
    line["q"] = "x<y>&\"z"
    xml = m.render_audit_message(line)

    assert "&quot;" in xml or "&#34;" in xml, xml
    assert "&lt;" in xml and "&gt;" in xml, xml
    assert "&amp;" in xml, xml
    # The raw forms must not appear inside a value.
    assert '<c>&' not in xml, xml
    assert "x<y>" not in xml, xml

    # A NUL and a lone control character are stripped or escaped, never left
    # raw in the document.
    line2 = json.loads(FULL_LINE)
    line2["q"] = "a\x00b\x01c"
    xml2 = m.render_audit_message(line2)
    assert "\x00" not in xml2, "a NUL survived into the XML"
    assert "\x01" not in xml2, "a control character survived into the XML"


def test_rfc5424_frame_shape():
    """PRI/version/timestamp/host/app/procid/msgid/SD, then BOM + XML."""
    m = load_module()
    xml = m.render_audit_message(json.loads(FULL_LINE))
    frame = m.render_syslog(json.loads(FULL_LINE), xml)

    # facility 13 (log audit) << 3 | severity 5 = 109
    assert frame.startswith("<109>1 "), frame[:40]
    assert " 2026-09-24T10:16:43Z " in frame
    # HOSTNAME is NILVALUE.
    assert " - clarus - 110112 - " in frame, frame[:160]
    # MSGID is the atna code and structured data is NILVALUE.
    assert frame.endswith("BOM<AuditMessage".replace("BOM", "\ufeff") + xml[len("<AuditMessage"):]) or (
        "\ufeff<AuditMessage" in frame
    ), "the MSG must be a BOM followed by the XML"


def test_tcp_frame_is_msg_len_space_msg():
    """RFC 6587 octet-counting: `MSG-LEN SP SYSLOG-MSG`, byte for byte."""
    m = load_module()
    xml = m.render_audit_message(json.loads(FULL_LINE))
    frame = m.render_syslog(json.loads(FULL_LINE), xml)

    wire = m.frame_tcp(frame)
    length_text, _, rest = wire.partition(b" ")
    assert length_text.isdigit(), wire[:40]
    assert int(length_text) == len(rest), (
        f"declared {length_text} but carried {len(rest)} bytes"
    )
    assert rest.decode("utf-8") == frame
    # And the whole thing is exactly the declared length plus the header.
    assert len(wire) == len(length_text) + 1 + int(length_text)


def test_cursor_advances_only_over_delivered_bytes():
    """Resume: the cursor must not move past a line that was not delivered."""
    m = load_module()
    lines = [FULL_LINE, UPDATE_LINE]
    blob = ("\n".join(lines) + "\n").encode("utf-8")
    key = "journal-1"

    first_len = len(lines[0].encode("utf-8")) + 1
    delivered = []

    def send(_frame):
        # The first send succeeds, the second raises: the cursor must be
        # persisted only up to what was actually delivered.
        delivered.append(True)
        if len(delivered) > 1:
            raise OSError("injected transport failure")

    cursor = m.Cursor(path=None)
    state = m.process_buffer(key, blob, cursor, send, warn=lambda _m: None)
    assert state["delivered"] == 1
    assert state["failed"] == 1
    assert cursor.get(key) == first_len, (
        f"cursor moved past the undelivered line: {cursor.get(key)} != {first_len}"
    )

    # Re-running from that offset delivers only the second line, once.
    again = []
    cursor2 = m.Cursor(path=None)
    cursor2.set(key, state and cursor.get(key))
    m.process_buffer(
        key,
        blob[cursor.get(key):],
        cursor2,
        lambda frame: again.append(frame),
        warn=lambda _m: None,
        start_offset=cursor.get(key),
    )
    assert len(again) == 1, "the second line is delivered exactly once"
    assert cursor2.get(key) == len(blob)


def test_cursor_file_round_trip():
    """The cursor persists per-file offsets and reloads them."""
    m = load_module()
    target = HERE / (".cursor_test_%d.json" % os.getpid())
    try:
        cur = m.Cursor(path=target)
        cur.set("/a/b/20260924.log", 1234)
        cur.set("/a/b/20260925.log", 7)
        cur.save()

        again = m.Cursor(path=target)
        again.load()
        assert again.get("/a/b/20260924.log") == 1234
        assert again.get("/a/b/20260925.log") == 7
        assert again.get("/a/b/20260926.log") is None
    finally:
        try:
            target.unlink()
        except OSError:
            pass


def test_unparsable_line_does_not_stop_the_run():
    """A corrupt line is skipped with a warning; the rest still render."""
    m = load_module()
    blob = (
        '{"event":"access" BROKEN\n' + FULL_LINE + "\n" + UPDATE_LINE + "\n"
    ).encode("utf-8")
    sent = []
    err = []
    state = m.process_buffer(
        "journal-2",
        blob,
        m.Cursor(path=None),
        lambda frame: sent.append(frame),
        warn=err.append,
    )
    assert state["delivered"] == 2, "both good lines were delivered"
    assert state["skipped"] == 1, "the broken line was skipped"
    assert "BROKEN" not in "".join(sent), "nothing unusable was sent"
    assert err, "a warning was emitted"


def test_udp_send_and_tcp_send_reach_a_socket():
    """The transport actually puts bytes on the wire (loopback)."""
    m = load_module()
    xml = m.render_audit_message(json.loads(FULL_LINE))
    frame = m.render_syslog(json.loads(FULL_LINE), xml)

    # UDP.
    us = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    us.bind(("127.0.0.1", 0))
    us.settimeout(5)
    host, port = us.getsockname()
    sender = m.make_sender("udp", host, port, verbose=False)
    sender(frame)
    data, _ = us.recvfrom(65535)
    us.close()
    assert data.decode("utf-8") == frame, "the UDP datagram is the frame"

    # TCP: the receiver must see the octet-counted frame.
    ts = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    ts.bind(("127.0.0.1", 0))
    ts.listen(1)
    ts.settimeout(5)
    thost, tport = ts.getsockname()
    tsender = m.make_sender("tcp", thost, tport, verbose=False)
    tsender(frame)
    conn, _ = ts.accept()
    conn.settimeout(5)
    got = b""
    while True:
        chunk = conn.recv(65536)
        if not chunk:
            break
        got += chunk
        if got.count(b" ") >= 1 and int(got.split(b" ", 1)[0]) + len(
            got.split(b" ", 1)[0]
        ) + 1 <= len(got):
            break
    conn.close()
    ts.close()
    length_text, _, rest = got.partition(b" ")
    assert int(length_text) == len(rest), f"TCP frame mis-framed: {got[:60]!r}"
    assert rest.decode("utf-8") == frame


def test_dry_run_renders_the_field_sample_if_present():
    """The real journal renders end to end, if the sample is checked out.

    The file is deliberately NOT committed (it is field data); when it is
    absent the test reports a skip rather than passing silently.
    """
    if not FIELD_SAMPLE.exists():
        print("skip: field sample not present at", FIELD_SAMPLE)
        return
    m = load_module()
    raw = FIELD_SAMPLE.read_bytes()
    lines = [l for l in raw.split(b"\n") if l.strip()]
    assert len(lines) == 5659, f"expected 5659 lines, found {len(lines)}"

    sizes = []
    for line in lines:
        obj = json.loads(line.decode("utf-8"))
        xml = m.render_audit_message(obj)
        assert xml.startswith("<AuditMessage"), xml[:60]
        sizes.append(len(m.render_syslog(obj, xml)))
    sizes.sort()
    p50 = sizes[len(sizes) // 2]
    print(
        f"field sample: {len(sizes)} messages, "
        f"min {sizes[0]} B, p50 {p50} B, max {sizes[-1]} B"
    )
    assert sizes[0] > 0


def test_dry_run_writes_utf8_bytes_regardless_of_console_encoding():
    """The message is UTF-8 on the wire, so --dry-run must emit UTF-8 bytes.

    A console whose encoding cannot represent the BOM (cp1251 is the default
    on a Russian Windows install) must not be able to break the dry run: the
    bytes are the artifact under test, and they go to the binary buffer.
    """
    m = load_module()
    original = sys.stdout
    captured = io.BytesIO()

    class ByteOnly:
        buffer = captured

    try:
        sys.stdout = ByteOnly()
        m.write_stdout_bytes("\ufeff<AuditMessage/>".encode("utf-8") + b"\n")
    finally:
        sys.stdout = original

    got = captured.getvalue()
    assert got == "\ufeff<AuditMessage/>".encode("utf-8") + b"\n", got
    assert got.startswith(b"\xef\xbb\xbf"), "the UTF-8 BOM is three raw bytes"


def test_cyrillic_survives_as_utf8():
    """Non-ASCII journal values survive to the wire as UTF-8, not mojibake."""
    m = load_module()
    line = json.loads(FULL_LINE)
    line["user"] = "\u0430\u0434\u043c\u0438\u043d"  # Cyrillic "admin"
    xml = m.render_audit_message(line)
    frame = m.render_syslog(line, xml)
    payload = frame.encode("utf-8")
    assert "\u0430\u0434\u043c\u0438\u043d".encode("utf-8") in payload
    # And it decodes back to the same text.
    assert "\u0430\u0434\u043c\u0438\u043d" in payload.decode("utf-8")


def test_participant_object_name_or_query_is_always_present():
    """A.5.2: ParticipantObjectName is MC - "required if ParticipantObjectQuery
    is not present" - so every participant object carries exactly one of the
    two. Neither absent nor both.

    This is the constraint that makes the identifiers renderable at all: a
    bare UID still needs a name, and the only honest name for a UID is the
    UID.
    """
    m = load_module()
    import re as _re

    for line in (FULL_LINE, UPDATE_LINE):
        xml = m.render_audit_message(json.loads(line))
        for block in _re.findall(
            r"<ParticipantObjectIdentification.*?</ParticipantObjectIdentification>|"
            r"<ParticipantObjectIdentification[^>]*/>",
            xml,
            _re.S,
        ):
            has_name = "<ParticipantObjectName>" in block
            has_query = "<ParticipantObjectQuery>" in block
            assert has_name != has_query, (
                f"exactly one of name/query is required, got name={has_name} "
                f"query={has_query}: {block}"
            )
            # ParticipantObjectID is mandatory.
            assert "ParticipantObjectID=" in block, block


def test_patient_object_is_a_person_in_the_patient_role():
    """A patient identifier is a Person (1) in the Patient role (1), not a
    system object."""
    m = load_module()
    xml = m.render_audit_message(json.loads(FULL_LINE))
    assert 'ParticipantObjectID="PAT-1"' in xml
    idx = xml.index('ParticipantObjectID="PAT-1"')
    block = xml[idx : xml.index("</ParticipantObjectIdentification>", idx)]
    assert 'ParticipantObjectTypeCode="1"' in block, block
    assert 'ParticipantObjectTypeCodeRole="1"' in block, block
    assert 'csd-code="2"' in block, "2 = Patient Number"


def test_query_object_uses_search_criteria():
    """A QIDO query is a Search Criteria object carrying the criteria as the
    query, never as a name."""
    m = load_module()
    line = json.loads(FULL_LINE)
    line["study"] = ""
    line["series"] = ""
    line["sop"] = ""
    line["patient"] = ""
    line["q"] = "PatientName=Smith*"
    xml = m.render_audit_message(line)
    assert 'csd-code="10"' in xml, "10 = Search Criteria"
    assert "<ParticipantObjectQuery>PatientName=Smith*</ParticipantObjectQuery>" in xml
    assert "<ParticipantObjectName>" not in xml, (
        "a query object must not also carry a name"
    )


def main():
    tests = [
        (name, fn)
        for name, fn in sorted(globals().items())
        if name.startswith("test_") and callable(fn)
    ]
    failed = 0
    for name, fn in tests:
        try:
            fn()
        except Exception as exc:  # noqa: BLE001 - a test runner reports all
            failed += 1
            print(f"FAIL {name}: {type(exc).__name__}: {exc}")
        else:
            print(f"ok   {name}")
    print(f"\n{len(tests) - failed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
