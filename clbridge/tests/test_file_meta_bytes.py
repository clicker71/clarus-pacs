# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""3008 canonical transcode file-meta writer - bridge unit tests.

Covers the ONE canonical meta writer used before every bridge C-STORE:
- _ui_elem: even length, NULL INSIDE the length (PS3.5 sec 6.2)
- _extract_meta_ui / _extract_dataset_ui: byte-level UID resolution
- _ensure_file_meta_bytes: canonical trio, ascending, validate-or-rewrite
"""

import io
import struct

from clean_code.handlers.cmove import (
    _ensure_file_meta_bytes,
    _extract_dataset_ui,
    _extract_meta_ui,
    _ui_elem,
    _walk_meta,
)
from pydicom import dcmread

SC = "1.2.840.10008.5.1.4.1.1.2"  # 25 chars - odd
SI = "1.2.3.4.5.6.7.8.9.10"  # 20 chars - even
TS = "1.2.840.10008.1.2.1"  # 19 chars - odd

# PS3.10 7.1 Table 7.1-1 marks six elements Type 1 in the File Meta
# Information. A group 0002 without (0002,0000), (0002,0001) or (0002,0012)
# is a non-conformant Part 10 file even though pydicom still parses it.
MANDATORY_META_ELEMS = [0x0000, 0x0001, 0x0002, 0x0003, 0x0010, 0x0012]


def _ui(value):
    """Reference UI element bytes (even length, NULL inside)."""
    v = value.encode("ascii")
    if len(v) % 2:
        v += b"\x00"
    return v


def _meta_ui_bytes(elem, value):
    v = _ui(value)
    return struct.pack("<HH2sH", 0x0002, elem, b"UI", len(v)) + v


def _ts_only_meta():
    return _meta_ui_bytes(0x0010, TS)


def _explicit_dataset(sc=SC, si=SI):
    d = b""
    for elem, value in ((0x0016, sc), (0x0018, si)):
        d += struct.pack("<HH", 0x0008, elem)
        d += b"UI"
        d += struct.pack("<H", len(_ui(value)))
        d += _ui(value)
    d += (
        struct.pack("<HH", 0x0028, 0x0010)
        + b"US"
        + struct.pack("<H", 2)
        + struct.pack("<H", 4)
    )
    d += (
        struct.pack("<HH", 0x0028, 0x0011)
        + b"US"
        + struct.pack("<H", 2)
        + struct.pack("<H", 4)
    )
    d += (
        struct.pack("<HH", 0x0028, 0x0004)
        + b"CS"
        + struct.pack("<H", 12)
        + b"MONOCHROME2\x00"
    )
    d += (
        struct.pack("<HH", 0x7FE0, 0x0010)
        + b"OW"
        + b"\x00\x00"
        + struct.pack("<I", 4)
        + b"\xaa" * 4
    )
    return d


def _with_preamble(body):
    return b"\x00" * 128 + b"DICM" + body


def test_ui_elem_null_inside_odd_length():
    e = _ui_elem(0x0010, TS)
    assert e[0:2] == struct.pack("<H", 0x0002)
    assert e[2:4] == struct.pack("<H", 0x0010)
    assert e[4:6] == b"UI"
    length = struct.unpack("<H", e[6:8])[0]
    assert length == 20  # 19 chars + NULL inside
    assert len(e) == 8 + length  # no external pad
    assert e[-1:] == b"\x00"
    assert e[8:-1] == TS.encode("ascii")


def test_ensure_ts_only_meta_emits_canonical_trio():
    original = _with_preamble(_ts_only_meta() + _explicit_dataset())
    # pydicom gave '' for both UIDs (the 30.08 incident) - bytes must win.
    out = _ensure_file_meta_bytes(original, "", "", TS)

    _, elems, meta_end = _walk_meta(out)
    assert [e for e, _ in elems] == MANDATORY_META_ELEMS  # ascending
    values = {e: v for e, v in elems}
    assert values[0x0001] == b"\x00\x01", "Version: bit 0 of the first byte"
    assert values[0x0002].rstrip(b"\x00") == SC.encode("ascii")
    assert values[0x0003].rstrip(b"\x00") == SI.encode("ascii")
    assert values[0x0010].rstrip(b"\x00") == TS.encode("ascii")
    assert values[0x0012], "(0002,0012) Implementation Class UID is Type 1"
    # (0002,0000) counts the bytes that follow its own value field.
    assert struct.unpack("<I", values[0x0000])[0] == meta_end - 132 - 12, (
        "the group length must cover exactly the rest of group 0002"
    )
    # Every value length is even and the pad is INSIDE the length. The UL
    # group length and the UI UIDs use the 8-byte SHORT header; the OB
    # version element uses the 12-byte LONG one (2 reserved bytes + 4-byte
    # length), which is how pydicom's own writer encodes (0002,0001).
    pos = 132
    for _e, v in elems:
        assert len(v) % 2 == 0
        pos += 12 if _e == 0x0001 else 8
        pos += len(v)
    assert pos == meta_end
    # The old TS-only element is gone (exactly one 0010) and the dataset is
    # untouched.
    assert sum(1 for e, _ in elems if e == 0x0010) == 1
    assert out[meta_end:] == _explicit_dataset()

    ds = dcmread(io.BytesIO(out), stop_before_pixels=True)
    assert str(ds.SOPClassUID) == SC
    assert str(ds.SOPInstanceUID) == SI
    assert str(ds.file_meta.TransferSyntaxUID) == TS


def test_ensure_canonical_trio_unchanged():
    # The writer's OWN output is the fixed point: repairing it changes nothing.
    body = _explicit_dataset()
    once = _ensure_file_meta_bytes(_with_preamble(body), "", "", TS)
    twice = _ensure_file_meta_bytes(once, "", "", TS)
    assert twice == once  # byte-identical no-op
    _, elems, meta_end = _walk_meta(once)
    assert [e for e, _ in elems] == MANDATORY_META_ELEMS
    assert once[meta_end:] == body


def test_ensure_rewrites_external_pad_meta():
    # Old emitter shape: odd length + external pad byte (bug F6).
    def broken_ui(elem, value):
        v = value.encode("ascii")
        return (
            struct.pack("<HH2sH", 0x0002, elem, b"UI", len(v))
            + v
            + (b"\x00" if len(v) % 2 else b"")
        )

    meta = broken_ui(0x0002, SC) + broken_ui(0x0003, SI) + broken_ui(0x0010, TS)
    original = _with_preamble(meta + _explicit_dataset())
    out = _ensure_file_meta_bytes(original, "", "", TS)

    _, elems, meta_end = _walk_meta(out)
    assert [e for e, _ in elems] == MANDATORY_META_ELEMS
    for _e, v in elems:
        assert len(v) % 2 == 0  # even length, NULL inside
    assert out[meta_end:] == _explicit_dataset()
    ds = dcmread(io.BytesIO(out), stop_before_pixels=True)
    assert str(ds.SOPClassUID) == SC


def test_extract_meta_ui_external_pad_element():
    v = SC.encode("ascii")
    elem = (
        struct.pack("<HH2sH", 0x0002, 0x0002, b"UI", len(v))
        + v
        + b"\x00"  # external pad after odd value
    )
    data = _with_preamble(elem + b"\x08\x00\x60\x00CS\x02\x00CR")
    assert _extract_meta_ui(data, 0x0002) == SC


def test_extract_dataset_ui_ts_only_meta():
    data = _with_preamble(_ts_only_meta() + _explicit_dataset())
    assert _extract_dataset_ui(data, 0x0016) == SC
    assert _extract_dataset_ui(data, 0x0018) == SI


SERIES = "1.2.840.113857.113857.1005.120855"


def _sq_undefined_body(item_payload):
    """One defined-length item + sequence delimitation item (PS3.5 A.4)."""
    return (
        struct.pack("<HHI", 0xFFFE, 0xE000, len(item_payload))
        + item_payload
        + (b"\x00" if len(item_payload) % 2 else b"")
        + struct.pack("<HHI", 0xFFFE, 0xE0DD, 0)
    )


def _dataset_with_undefined_sq_before_series(body):
    """(0008,2112) SQ undefined-length BEFORE (0020,000E) - the ZONARE
    0109 shape that used to stop the dataset scan and kill the C-MOVE
    refetch rescue (no series UID for the instance URL)."""
    d = (
        struct.pack("<HH", 0x0008, 0x0018)
        + b"UI"
        + struct.pack("<H", len(_ui(SI)))
        + _ui(SI)
    )
    d += (
        struct.pack("<HH", 0x0008, 0x2112)
        + b"SQ"
        + b"\x00\x00"
        + struct.pack("<I", 0xFFFFFFFF)
        + body
    )
    d += (
        struct.pack("<HH", 0x0020, 0x000E)
        + b"UI"
        + struct.pack("<H", len(_ui(SERIES)))
        + _ui(SERIES)
    )
    return d


def test_extract_dataset_ui_skips_undefined_sq():
    item = (
        struct.pack("<HH", 0x0008, 0x1150)
        + b"UI"
        + struct.pack("<H", len(_ui(SC)))
        + _ui(SC)
    )
    data = _with_preamble(
        _ts_only_meta()
        + _dataset_with_undefined_sq_before_series(_sq_undefined_body(item))
    )
    assert _extract_dataset_ui(data, 0x000E, group=0x0020) == SERIES


def test_extract_dataset_ui_skips_undefined_item_with_raw_dataset():
    # ZONARE field shape: the first item is undefined-length and its
    # content is a RAW DATASET (0008,1150 / 0008,1155), not a nested
    # item list.
    inner = (
        struct.pack("<HH", 0x0008, 0x1150)
        + b"UI"
        + struct.pack("<H", len(_ui(SC)))
        + _ui(SC)
        + struct.pack("<HH", 0x0008, 0x1155)
        + b"UI"
        + struct.pack("<H", len(_ui(SI)))
        + _ui(SI)
    )
    body = (
        struct.pack("<HHI", 0xFFFE, 0xE000, 0xFFFFFFFF)
        + inner
        + struct.pack("<HHI", 0xFFFE, 0xE0DD, 0)
    )
    data = _with_preamble(
        _ts_only_meta() + _dataset_with_undefined_sq_before_series(body)
    )
    assert _extract_dataset_ui(data, 0x000E, group=0x0020) == SERIES


def test_extract_dataset_ui_malformed_undefined_sq_returns_none():
    # No delimitation item: the skip bails and the scan returns None
    # (no panic, no wrong UID).
    d = (
        struct.pack("<HH", 0x0008, 0x2112)
        + b"SQ"
        + b"\x00\x00"
        + struct.pack("<I", 0xFFFFFFFF)
        + b"\x00" * 8
    )
    data = _with_preamble(_ts_only_meta() + d)
    assert _extract_dataset_ui(data, 0x000E, group=0x0020) is None


def test_empty_pydicom_value_does_not_suppress_0003():
    # meta-less source; the caller parsed '' for the instance UID - the
    # dataset bytes must still fill (0002,0003).
    original = _with_preamble(_explicit_dataset())
    out = _ensure_file_meta_bytes(original, "", "", TS)
    _, elems, _ = _walk_meta(out)
    values = {e: v for e, v in elems}
    assert 0x0003 in values
    assert values[0x0003].rstrip(b"\x00") == SI.encode("ascii")


def test_meta_less_source_builds_preamble_and_trio():
    # No DICM magic at all: preamble is built, the full meta group synthesized.
    body = _explicit_dataset()
    out = _ensure_file_meta_bytes(body, "", "", TS)
    assert out[:132] == b"\x00" * 128 + b"DICM"
    _, elems, meta_end = _walk_meta(out)
    assert [e for e, _ in elems] == MANDATORY_META_ELEMS
    assert out[meta_end:] == body
    ds = dcmread(io.BytesIO(out), stop_before_pixels=True)
    assert str(ds.file_meta.TransferSyntaxUID) == TS
    assert str(ds.SOPClassUID) == SC
