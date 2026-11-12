# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""1013 rabbit pass 5 - D3..D6.

D3: a partial pre-flight walk must not be cached (the missed series would
    stay out of the proposal for the whole TTL).
D4: refetch bytes are counted ONCE, on acceptance (bytes_total describes
    delivered traffic, not attempts).
D5: a JSON `null` inside a DICOMweb `Value` array is an ABSENT value, not the
    five characters "None".
D6: the bundler's relative-import error names the file it came from.
"""

import pytest
from clean_code import protocol, utils
from clean_code._bundle_order import find_imports
from clean_code.handlers import cmove as cm

CT = "1.2.840.10008.5.1.4.1.1.2"
TS_470 = "1.2.840.10008.1.2.4.70"


# ---------------------------------------------------------------------------
# D3 - only a COMPLETE walk is cached
# ---------------------------------------------------------------------------
class _Resp:
    def __init__(self, payload):
        self._payload = payload
        self.status_code = 200

    def json(self):
        return self._payload

    def raise_for_status(self):
        pass


def _inst_row(sop):
    return {"00080018": {"vr": "UI", "Value": [sop]}}


def _class_ts_row(ts=TS_470):
    return [{"00080016": {"vr": "UI", "Value": [CT]}, "00020010": {"vr": "UI", "Value": [ts]}}]


def test_a_partial_preflight_walk_is_not_cached(monkeypatch):
    """One series failed to probe -> the walk is partial -> do NOT remember it.

    Caching it would keep the missed series out of the proposal for the whole
    120 s TTL, so every C-MOVE in that window would fall back to the
    registry-wide set - the exact failure mode the probe exists to remove.
    The partial result is still RETURNED for the current C-MOVE.
    """
    cm._preflight_cache.clear()
    series = [
        {"0020000E": {"vr": "UI", "Value": ["2.25.1"]}},
        {"0020000E": {"vr": "UI", "Value": ["2.25.2"]}},
    ]

    monkeypatch.setattr(
        cm,
        "_qido_request",
        lambda url, *a, **k: series if url.endswith("/series") else [_inst_row("1.2.3")],
    )

    n = {"i": 0}

    def _get(url, **kwargs):
        n["i"] += 1
        if n["i"] > 1:
            raise RuntimeError("series 2 probe timed out")
        return _Resp(_class_ts_row())

    monkeypatch.setattr(cm.requests, "get", _get)
    out, partial = cm._preflight_study_ts_by_class("2.25.9", "http://x/dicomweb")

    assert partial is True, "the walk is partial: one series probe failed"
    assert out == {CT: {TS_470}}, "the partial result is still useful for THIS call"
    assert cm._preflight_cache == {}, (
        "a partial walk must not be cached - the missed series would stay out "
        f"of the proposal for the whole TTL: {cm._preflight_cache}"
    )


def test_a_complete_preflight_walk_is_cached(monkeypatch):
    """Control: an all-probed walk IS cached, so D3 did not disable the cache."""
    cm._preflight_cache.clear()
    series = [{"0020000E": {"vr": "UI", "Value": ["2.25.1"]}}]
    monkeypatch.setattr(
        cm,
        "_qido_request",
        lambda url, *a, **k: series if url.endswith("/series") else [_inst_row("1.2.3")],
    )
    monkeypatch.setattr(cm.requests, "get", lambda url, **k: _Resp(_class_ts_row()))
    cm._preflight_study_ts_by_class("2.25.10", "http://x/dicomweb")
    assert ("2.25.10", False) in cm._preflight_cache


# ---------------------------------------------------------------------------
# D4 - refetch bytes counted once, on acceptance
# ---------------------------------------------------------------------------
def test_refetch_does_not_increment_bytes_at_send_time():
    """`_refetch_and_send` must not touch bytes_total.

    The main send loop counts `part_size` once when the peer answers 0x0000.
    Counting here as well double-counted every refetched instance (once with
    the refetched size, once with the stored size) and also counted instances
    the peer rejected.
    """
    # `_refetch_and_send` is a closure inside `_execute_cmove_job`, so read the
    # module source to scope the assertion to that function's body.
    with open(cm.__file__, encoding="utf-8") as f:
        module_src = f.read()
    start = module_src.index("def _refetch_and_send(")
    end = module_src.index("def _refetch_after_rejection(", start)
    body = module_src[start:end]
    # Strip comments: the explanatory note NAMES bytes_total, and a substring
    # scan over raw text would fail on the comment rather than on code.
    code_lines = []
    for line in body.splitlines():
        hash_at = line.find("#")
        code_lines.append(line if hash_at < 0 else line[:hash_at])
    code = "\n".join(code_lines)
    assert "cstore_outbound_duration" in code, (
        "the duration observe must stay - only the bytes increment goes"
    )
    assert "bytes_total" not in code, (
        "the refetch path must not increment bytes_total: the success branch "
        f"of the main loop already counts it. Found in:\n{code}"
    )


# ---------------------------------------------------------------------------
# D5 - a null inside Value is absent, not the text "None"
# ---------------------------------------------------------------------------
def test_a_none_inside_a_multi_value_array_is_dropped():
    """`["CT", None]` -> `["CT"]`, never `["CT", "None"]`."""
    from pydicom import Dataset

    ds = Dataset()
    assert protocol._json_add_vr(ds, 0x0008, 0x0061, "CS", ["CT", None]) is True
    # One remaining value is a plain CS value, not a one-item list.
    assert str(ds.ModalitiesInStudy) == "CT", ds


def test_an_all_none_value_array_is_not_added():
    """`[None]` carries no value: the element is not added at all."""
    from pydicom import Dataset

    ds = Dataset()
    assert protocol._json_add_vr(ds, 0x0008, 0x0060, "CS", [None]) is False
    assert "Modality" not in ds


# ---------------------------------------------------------------------------
# D6 - the bundler error names the file
# ---------------------------------------------------------------------------
def test_the_relative_import_error_names_the_file():
    """The message names the offending module AND the file it came from."""
    with pytest.raises(ValueError) as exc:
        find_imports("from .handlers import cfind\n", path="handlers/nevent.py")
    msg = str(exc.value)
    assert "handlers/nevent.py" in msg, msg
    assert "cfind" in msg, msg
    assert "\n" not in msg, "the message stays one line"


def test_find_imports_without_a_path_still_reports_the_import():
    """Omitting `path` (the existing tests and callers) does not crash."""
    with pytest.raises(ValueError) as exc:
        find_imports("from . import config\n")
    assert "config" in str(exc.value)


# ---------------------------------------------------------------------------
# R2 - docstring only; the BEHAVIOUR is unchanged
# ---------------------------------------------------------------------------
def test_normalize_host_still_collapses_the_loopback_block():
    """R2 changed only the docstring. Pin the behaviour it now describes."""
    assert utils.normalize_host("::ffff:127.0.0.1") == "127.0.0.1"
    assert utils.normalize_host("::1") == "127.0.0.1"
    assert utils.normalize_host("localhost") == "127.0.0.1"
    assert utils.normalize_host("0.0.0.0") == "127.0.0.1"
    assert utils.normalize_host("MYHOST") == "myhost", "a name is not resolved"
    assert utils.normalize_host("10.1.2.3") == "10.1.2.3"
