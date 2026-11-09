# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""3108 bridge TS order - EVRLE proposed FIRST (31.08 owner experiment).

Unit: the IMAGE_TS lists (main.py SCP-side and cmove.py outbound C-MOVE)
start with ExplicitVRLittleEndian; stored 4.80 rides the per-(class, 4.80)
fallback association (wire-tested in test_stored_first_wire.py).
NON_IMAGE_TS unchanged.

Wire: real pynetdicom negotiation - the ACCEPTOR picks the first TS of
ITS OWN supported list that appears in the proposal (proposal order is
irrelevant for a standard SCP): a 4.80-capable receiver still negotiates
4.80; a receiver WITHOUT 4.80 gets EVRLE.
"""

import socket
import time

import clean_code.main as main_mod
from clean_code.handlers.cmove import _associate_for_move
from clean_code.protocol import (
    IMAGE_TS,
    JPEGLS_LOSSLESS_TS,
    NON_IMAGE_TS,
)
from pydicom.uid import (
    ExplicitVRLittleEndian,
    ImplicitVRLittleEndian,
    JPEGLSLossless,
)
from pynetdicom import AE, evt

EVRLE = ExplicitVRLittleEndian
IVRLE = ImplicitVRLittleEndian
TS_480 = "1.2.840.10008.1.2.4.80"

CT = "1.2.840.10008.5.1.4.1.1.2"
SR = "1.2.840.10008.5.1.4.1.1.88.11"  # Basic Text SR

SOP_REGISTRY = {
    CT: {"accept": True},
    SR: {"accept": True, "non_image": True},
}


def _free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def test_image_ts_evrle_first():
    assert JPEGLS_LOSSLESS_TS == TS_480
    assert IMAGE_TS[0] == EVRLE, "EVRLE must be proposed FIRST (31.08 owner experiment)"
    # The rest of the order is preserved (31.08 YAGNI trim: 4.57 and
    # Baseline 4.50 dropped - verdict
    # analysis/3108_architect_cmove_rule_yagni.md Task 2).
    assert IMAGE_TS[1:] == [
        TS_480,
        IVRLE,
        "1.2.840.10008.1.2.4.70",
    ]
    assert "1.2.840.10008.1.2.4.57" not in IMAGE_TS
    assert "1.2.840.10008.1.2.4.50" not in IMAGE_TS


def test_non_image_ts_unchanged():
    assert TS_480 not in NON_IMAGE_TS
    assert NON_IMAGE_TS == [EVRLE, IVRLE]


def test_main_module_lists_match():
    # main.py (bridge SCP + store-and-forward SCU side) carries the same
    # owner decision.
    assert main_mod.JPEGLS_LOSSLESS_TS == TS_480
    assert main_mod.IMAGE_TS[0] == EVRLE
    assert main_mod.IMAGE_TS[1:] == IMAGE_TS[1:]
    assert TS_480 not in main_mod.NON_IMAGE_TS


class _Scp:
    """Real pynetdicom SCP accepting the given (sop_class, ts) contexts."""

    def __init__(self, contexts):
        self.ae = AE(ae_title="TEST_SCP")
        for sop_class, ts_list in contexts:
            self.ae.add_supported_context(sop_class, ts_list)
        self.port = _free_port()
        self.server = self.ae.start_server(
            ("127.0.0.1", self.port),
            block=False,
            evt_handlers=[(evt.EVT_C_STORE, lambda e: 0x0000)],
        )

    def shutdown(self):
        try:
            self.ae.shutdown()
        except Exception:
            pass


def test_wire_receiver_with_480_still_negotiates_480():
    # Wire fact (pynetdicom presentation.py negotiate_as_acceptor): each of
    # the proposed per-TS contexts for a class is accepted/rejected
    # independently (PS3.8 9.3.2.2). A 4.80-capable receiver accepts BOTH the
    # uncompressed contexts AND the 4.80 one -> the set-valued accepted map
    # (R3) carries EVRLE, IVRLE and 4.80, so stored 4.80 instances send as-is
    # on the main association. The multi-context proposal keeps 4.80 outcomes
    # identical to the old single-context shape.
    scp = _Scp([(CT, [JPEGLSLossless, EVRLE, IVRLE]), (SR, [EVRLE, IVRLE])])
    time.sleep(0.3)
    try:
        assoc, accepted = _associate_for_move(
            "TEST_SCP",
            "CLARUS_BRIDGE",
            SOP_REGISTRY,
            "127.0.0.1",
            scp.port,
        )
        assert (
            TS_480 in accepted[CT]
        ), f"image class must accept the 4.80 context, got {accepted}"
        assert EVRLE in accepted[CT], "image class accepts uncompressed contexts too"
        assert accepted[SR] == {EVRLE, IVRLE}, "non-image class keeps EVRLE + IVRLE"
        assoc.release()
    finally:
        scp.shutdown()


def test_wire_receiver_without_480_falls_back_evrle():
    # Receiver without JPEG-LS accepts only the uncompressed contexts from
    # the same proposal - standard DIMSE negotiation, nothing extra needed.
    scp = _Scp([(CT, [EVRLE, IVRLE]), (SR, [EVRLE, IVRLE])])
    time.sleep(0.3)
    try:
        assoc, accepted = _associate_for_move(
            "TEST_SCP",
            "CLARUS_BRIDGE",
            SOP_REGISTRY,
            "127.0.0.1",
            scp.port,
        )
        assert (
            EVRLE in accepted[CT]
        ), f"no-4.80 receiver must accept EVRLE context, got {accepted}"
        assert TS_480 not in accepted[CT], "no-4.80 receiver rejects the 4.80 context"
        assert accepted[CT] == {EVRLE, IVRLE}
        assert accepted[SR] == {EVRLE, IVRLE}
        assoc.release()
    finally:
        scp.shutdown()
