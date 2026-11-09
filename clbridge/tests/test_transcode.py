# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Unit tests for _pick_refetch_ts - Rule A refetch-target selection (0909).

Rule A (no probing ever; analysis/0909_architect_microdicom_probe_refetch_
resume_REPORT.md sec. 2.2): a stored TS the viewer did not accept is refetched
on the MAIN association only if the server can produce an accepted TS. The
target is chosen from the intersection of the viewer's accepted TS set for the
class and the server's producible set, in priority EVRLE -> IVRLE -> any
remaining. An empty intersection means no deliverable form (the caller fails
the instance honestly).

0909 hotfix: the server's producible set is the caps list PLUs EVRLE whenever
caps is non-empty - a transcode-capable origin server is expected able to emit
the EVRLE default (PS3.18 8.7.3.1 Table 8.7.3-2). On a no-transcode server
(caps empty/None) EVRLE is never invented.

This file replaces the former _pick_transfer_syntax tests (that helper was the
study-level negotiation pick and is removed with the probe/fallback path).
"""

from clean_code.handlers.cmove import _pick_refetch_ts
from pydicom.uid import (
    ExplicitVRLittleEndian,
    ImplicitVRLittleEndian,
    JPEG2000Lossless,
    JPEGLSLossless,
)

EVRLE = ExplicitVRLittleEndian
IVRLE = ImplicitVRLittleEndian


class TestPickRefetchTs:

    def test_prefers_evrle_in_intersection(self):
        """EVRLE first when it is accepted by the viewer and producible."""
        acc = {EVRLE, JPEGLSLossless}
        prod = [JPEGLSLossless, EVRLE]
        assert _pick_refetch_ts(acc, prod) == EVRLE

    def test_evrle_wins_when_server_transcode_capable(self):
        """0909 hotfix: once the server claims transcode ability (caps
        non-empty, here it stores/produces IVRLE) EVRLE is treated as
        producible per PS3.18 8.7.3.1 (EVRLE is the Default transfer syntax)
        and wins by priority over IVRLE when the viewer accepts both. The old
        premise 'server produces IVRLE but not EVRLE' is no longer a
        supported state: a transcode-capable origin server is expected able
        to emit the uncompressed EVRLE default."""
        acc = {EVRLE, IVRLE}
        prod = [IVRLE]
        assert _pick_refetch_ts(acc, prod) == EVRLE

    def test_evrle_added_to_producible_on_transcode_server(self):
        """caps lists only compressed codecs (no EVRLE) yet a transcode
        server + EVRLE-accepting viewer still deliver via EVRLE."""
        acc = {EVRLE}
        prod = [JPEGLSLossless]  # transcode build, EVRLE not in caps
        assert _pick_refetch_ts(acc, prod) == EVRLE

    def test_evrle_not_invented_on_no_transcode_server(self):
        """caps EMPTY/None == no-transcode server: nothing - not even EVRLE -
        is invented, so the instance fails honestly."""
        assert _pick_refetch_ts(EVRLE, []) == ""
        assert _pick_refetch_ts(EVRLE, None) == ""
        assert _pick_refetch_ts({EVRLE, IVRLE}, []) == ""

    def test_evrle_priority_when_accepted_and_transcode_server(self):
        """EVRLE chosen over a compressed accepted TS on a transcode server
        (EVRLE enters prod by standard default; priority EVRLE -> IVRLE ->
        remainder)."""
        acc = {EVRLE, JPEGLSLossless}
        prod = [JPEGLSLossless]
        assert _pick_refetch_ts(acc, prod) == EVRLE

    def test_chooses_any_accepted_producible_when_no_uncompressed(self):
        """Any accepted-and-producible TS (deterministic) when EVRLE/IVRLE
        are absent from the intersection."""
        acc = {JPEG2000Lossless, JPEGLSLossless}
        prod = [JPEGLSLossless]
        assert _pick_refetch_ts(acc, prod) == JPEGLSLossless

    def test_empty_intersection_returns_empty(self):
        """No overlap -> '' (the caller fails the instance honestly)."""
        acc = {JPEG2000Lossless}
        prod = [EVRLE]
        assert _pick_refetch_ts(acc, prod) == ""

    def test_empty_server_returns_empty(self):
        """No-transcode server (prod empty) + stored-not-accepted -> ''."""
        acc = {EVRLE}
        assert _pick_refetch_ts(acc, []) == ""

    def test_empty_accepted_returns_empty(self):
        """No accepted TS for the class -> ''."""
        assert _pick_refetch_ts(set(), [EVRLE]) == ""

    def test_accepts_single_string_and_list_forms(self):
        """The accepted set may arrive as a string, list or set (R3)."""
        assert _pick_refetch_ts(EVRLE, [EVRLE]) == EVRLE
        assert _pick_refetch_ts([EVRLE, JPEGLSLossless], [EVRLE]) == EVRLE
