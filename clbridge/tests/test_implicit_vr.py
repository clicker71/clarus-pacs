# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""0209 bridge W4 - implicit VR byte parser.

- _extract_dataset_ui(implicit_vr=True): stored IVRLE datasets carry no
  VR field (tag + 4-byte LE length); the series UID must survive even
  AFTER an undefined-length SQ (field Job #38 - the refetch rescue was
  dead: "no series UID for the instance URL").
- _ensure_file_meta_bytes: ts_uid==IVRLE flips the dataset fallback scan
  inside the canonical meta writer.
"""

import struct

from clean_code.handlers.cmove import (
    _ensure_file_meta_bytes,
    _extract_dataset_ui,
    _extract_meta_ui,
)
from pydicom.uid import ExplicitVRLittleEndian, ImplicitVRLittleEndian

SC = "1.2.840.10008.5.1.4.1.1.2"  # 25 chars - odd
SI = "1.2.3.4.5.6.7.8.9.10"  # 20 chars - even
SERIES = "1.2.840.113857.113857.1005.120855"


def _ui(value):
    v = value.encode("ascii")
    return v + b"\x00" if len(v) % 2 else v


def _implicit_elem(group, elem, value):
    """Implicit VR element: tag (4) + length (4, LE), NO VR field."""
    return struct.pack("<HHI", group, elem, len(value)) + value


def _meta_ui(elem, value):
    v = _ui(value)
    return struct.pack("<HH2sH", 0x0002, elem, b"UI", len(v)) + v


def _with_preamble(body):
    return b"\x00" * 128 + b"DICM" + body


def _implicit_dataset():
    d = _implicit_elem(0x0008, 0x0016, _ui(SC))
    d += _implicit_elem(0x0008, 0x0018, _ui(SI))
    d += _implicit_elem(0x0020, 0x000E, _ui(SERIES))
    d += struct.pack("<HHI", 0x7FE0, 0x0010, 4) + b"\xaa" * 4
    return d


def _implicit_sq_before_series(sq_body):
    d = _implicit_elem(0x0008, 0x0018, _ui(SI))
    d += struct.pack("<HHI", 0x0008, 0x2112, 0xFFFFFFFF) + sq_body
    d += _implicit_elem(0x0020, 0x000E, _ui(SERIES))
    return d


def _implicit_sq_body(item_payload):
    """One defined-length implicit item + sequence delimitation item."""
    return (
        struct.pack("<HHI", 0xFFFE, 0xE000, len(item_payload))
        + item_payload
        + (b"\x00" if len(item_payload) % 2 else b"")
        + struct.pack("<HHI", 0xFFFE, 0xE0DD, 0)
    )


def test_extract_implicit_dataset_ui_simple():
    # Simple implicit VR dataset (no SQ): elements carry no VR field.
    body = _with_preamble(
        _meta_ui(0x0010, ImplicitVRLittleEndian) + _implicit_dataset()
    )
    assert _extract_dataset_ui(body, 0x0016, implicit_vr=True) == SC
    assert _extract_dataset_ui(body, 0x0018, implicit_vr=True) == SI
    assert _extract_dataset_ui(body, 0x000E, group=0x0020, implicit_vr=True) == SERIES


def test_extract_implicit_series_after_undefined_sq():
    # Field Job #38 shape: (0020,000E) AFTER an undefined-length SQ whose
    # items are implicit VR too (no VR fields anywhere in the dataset).
    item = _implicit_elem(0x0008, 0x1150, _ui(SC))
    body = _with_preamble(
        _meta_ui(0x0010, ImplicitVRLittleEndian)
        + _implicit_sq_before_series(_implicit_sq_body(item))
    )
    assert _extract_dataset_ui(body, 0x000E, group=0x0020, implicit_vr=True) == SERIES


def test_ensure_file_meta_bytes_implicit_dataset_resolves_uids():
    # ts_uid==IVRLE flips the dataset fallback scan inside the writer: a
    # meta-less implicit instance resolves its UIDs byte-level.
    body = _with_preamble(_implicit_dataset())
    out = _ensure_file_meta_bytes(body, "", "", ImplicitVRLittleEndian)
    assert _extract_meta_ui(out, 0x0002) == SC
    assert _extract_meta_ui(out, 0x0003) == SI
    assert _extract_meta_ui(out, 0x0010) == ImplicitVRLittleEndian


def test_extract_explicit_dataset_ui_regression():
    # Explicit VR LE (default): unchanged - the 0109 ZONARE SQ shape still
    # skips the undefined-length SQ and finds (0020,000E).
    item = (
        struct.pack("<HH", 0x0008, 0x1150)
        + b"UI"
        + struct.pack("<H", len(_ui(SC)))
        + _ui(SC)
    )
    sq_body = (
        struct.pack("<HHI", 0xFFFE, 0xE000, len(item))
        + item
        + struct.pack("<HHI", 0xFFFE, 0xE0DD, 0)
    )
    d = (
        struct.pack("<HH", 0x0008, 0x2112)
        + b"SQ"
        + b"\x00\x00"
        + struct.pack("<I", 0xFFFFFFFF)
        + sq_body
    )
    d += (
        struct.pack("<HH", 0x0020, 0x000E)
        + b"UI"
        + struct.pack("<H", len(_ui(SERIES)))
        + _ui(SERIES)
    )
    body = _with_preamble(_meta_ui(0x0010, ExplicitVRLittleEndian) + d)
    assert _extract_dataset_ui(body, 0x000E, group=0x0020) == SERIES
    assert _extract_dataset_ui(body, 0x000E, group=0x0020, implicit_vr=False) == SERIES
