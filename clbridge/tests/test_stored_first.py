# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""3008 stored-first passthrough - bridge unit tests.

- _decide_send_path: pure two-state decision, table-driven.
- _parse_part_transfer_syntax: per-part Content-Type transfer-syntax capture.
- _extract_dataset_ui(group=...): series UID byte-level extraction for the
  instance-level refetch URL.
"""

import struct

from clean_code.handlers.cmove import (
    _decide_send_path,
    _extract_dataset_ui,
    _extract_meta_ui,
)
from clean_code.streaming import _parse_part_transfer_syntax
from pydicom.uid import (
    ExplicitVRLittleEndian,
    ImplicitVRLittleEndian,
    JPEGBaseline8Bit,
)

EVRLE = ExplicitVRLittleEndian
IVRLE = ImplicitVRLittleEndian
JPG = JPEGBaseline8Bit


def test_decide_send_path_table():
    # 0909 Rule A (no probe ever): the stored TS determines the path. A class
    # actually accepts a compressed stored TS -> send_as_is on the MAIN
    # association; a stored TS the class did NOT accept -> 'needs_deliver'
    # (the caller then chooses a refetch TS or fails honestly; the probe is
    # gone).
    # stored TS in the class's accepted set -> as-is on the main assoc
    assert _decide_send_path(EVRLE, EVRLE, True) == "send_as_is"
    assert _decide_send_path({EVRLE, JPG}, JPG, True) == "send_as_is"
    assert _decide_send_path([EVRLE, JPG], JPG, True) == "send_as_is"
    # stored TS accepted only by ANOTHER class -> needs_deliver (not covered)
    assert _decide_send_path(EVRLE, JPG, True) == "needs_deliver"
    # bare-native class set + stored compressed -> needs_deliver
    assert _decide_send_path(EVRLE, JPG, True) == "needs_deliver"
    assert _decide_send_path([EVRLE], JPG, True) == "needs_deliver"
    # class not negotiated at all -> needs_deliver (only refetch-on-main or
    # honest failure remain - no probe)
    assert _decide_send_path("", JPG, True) == "needs_deliver"
    # non-image SOPs: always as-is on the main association
    assert _decide_send_path(EVRLE, JPG, False) == "send_as_is"
    assert _decide_send_path("", "", False) == "send_as_is"
    # empty accepted set + an image -> needs_deliver (no probe; honest fail
    # unless the server can produce an accepted TS)
    assert _decide_send_path(None, JPG, True) == "needs_deliver"
    assert _decide_send_path(None, None, True) == "send_as_is"
    # unknown stored TS -> as-is (no decision to refetch on)
    assert _decide_send_path(EVRLE, "", True) == "send_as_is"
    assert _decide_send_path(EVRLE, None, True) == "send_as_is"


def test_parse_part_transfer_syntax_present():
    h = b"Content-Type: application/dicom; transfer-syntax=1.2.840.10008.1.2.1\r\n"
    assert _parse_part_transfer_syntax(h) == "1.2.840.10008.1.2.1"


def test_parse_part_transfer_syntax_quoted():
    h = b'Content-Type: application/dicom; transfer-syntax="1.2.840.10008.1.2.4.50"\r\n'
    assert _parse_part_transfer_syntax(h) == "1.2.840.10008.1.2.4.50"


def test_parse_part_transfer_syntax_absent():
    h = b"Content-Type: application/dicom\r\n"
    assert _parse_part_transfer_syntax(h) is None


def test_parse_part_transfer_syntax_other_params():
    h = (
        b"Content-Type: application/dicom; type=x; "
        b"transfer-syntax=1.2.840.10008.1.2.1; boundary=q\r\n"
    )
    assert _parse_part_transfer_syntax(h) == "1.2.840.10008.1.2.1"


def _ui(value):
    v = value.encode("ascii")
    return v + b"\x00" if len(v) % 2 else v


def _dataset_bytes():
    """Explicit VR LE dataset with SOP class/instance and series UIDs."""
    d = b""
    for group, elem, value in (
        (0x0008, 0x0016, "1.2.840.10008.5.1.4.1.1.2"),
        (0x0008, 0x0018, "1.2.3.4.5.6.7.8.9.10"),
        (0x0020, 0x000E, "2.25.987654321"),
    ):
        vv = _ui(value)
        d += struct.pack("<HH", group, elem) + b"UI" + struct.pack("<H", len(vv)) + vv
    return d


def test_extract_dataset_ui_series_uid_group_param():
    # The instance-level refetch URL needs the series UID (0020,000E),
    # extracted byte-level - no pydicom in the hot loop.
    body = b"\x00" * 128 + b"DICM" + _dataset_bytes()
    assert _extract_dataset_ui(body, 0x0016) == "1.2.840.10008.5.1.4.1.1.2"
    assert _extract_dataset_ui(body, 0x0018) == "1.2.3.4.5.6.7.8.9.10"
    assert _extract_dataset_ui(body, 0x000E, group=0x0020) == "2.25.987654321"


def test_extract_meta_ui_used_for_ts():
    v = _ui(EVRLE)
    meta = struct.pack("<HH2sH", 0x0002, 0x0010, b"UI", len(v)) + v
    body = b"\x00" * 128 + b"DICM" + meta + _dataset_bytes()
    assert _extract_meta_ui(body, 0x0010) == EVRLE
