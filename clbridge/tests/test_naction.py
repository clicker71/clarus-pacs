# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Unit tests for clarus_bridge.handlers.naction — N-ACTION handler.

Tests for _parse_n_action_rq and _build_n_action_rsp (already tested in
test_protocol.py) plus handle_n_action integration points.
"""

from clean_code.protocol import _build_n_action_rsp, _parse_n_action_rq
from pydicom import Dataset


class TestParseNActionRq:
    """Already covered in test_protocol.py — here we add edge cases."""

    def test_parses_multiple_sops(self):
        """Parse N-ACTION-RQ with multiple Referenced SOPs."""

        class FakeEvent:
            dataset = Dataset()

        FakeEvent.dataset.TransactionUID = "1.2.3.4.5"

        # Build ReferencedSOPSequence
        item1 = Dataset()
        item1.ReferencedSOPClassUID = "1.2.840.10008.5.1.4.1.1.2"
        item1.ReferencedSOPInstanceUID = "1.2.3.4.5.6.1"
        item2 = Dataset()
        item2.ReferencedSOPClassUID = "1.2.840.10008.5.1.4.1.1.4"
        item2.ReferencedSOPInstanceUID = "1.2.3.4.5.6.2"
        FakeEvent.dataset.ReferencedSOPSequence = [item1, item2]

        tx_uid, sop_list = _parse_n_action_rq(FakeEvent())
        assert tx_uid == "1.2.3.4.5"
        assert len(sop_list) == 2
        assert sop_list[0] == ("1.2.840.10008.5.1.4.1.1.2", "1.2.3.4.5.6.1")


class TestBuildNActionRsp:
    """Already covered in test_protocol.py — here we add structure tests."""

    def test_mixed_success_failure_response(self):
        """Build response with both success and failure SOPs."""
        rsp = _build_n_action_rsp(
            "1.2.840.10008.1.20.1",
            [("1.2.840.10008.5.1.4.1.1.2", "1.2.840.113619.2.55.3.1.2.1")],
            [("1.2.840.10008.5.1.4.1.1.2", "1.2.840.113619.2.55.3.1.2.2", 0xC201)],
        )
        assert rsp.TransactionUID == "1.2.840.10008.1.20.1"
        assert len(rsp.ReferencedSOPSequence) == 1
        assert len(rsp.FailedSOPSequence) == 1
        assert rsp.FailedSOPSequence[0].FailureReason == 0xC201


class TestHandleNActionUnsupported:
    """Unit tests for handle_n_action - unsupported action types (0x0122)."""

    def test_unsupported_action_type_returns_0122(self):
        """N-ACTION with action_type != 1 returns 0x0122 (no-such-action)."""
        from clean_code.handlers.naction import handle_n_action

        # Minimal mock: only the attributes checked before parsing
        class MockAssoc:
            class Requestor:
                ae_title = "TEST_AE"

            requestor = Requestor()

        class FakeEvent:
            assoc = MockAssoc()
            action_type = 99  # unsupported
            dataset = Dataset()

        # Call handler — should reject at action_type check, not reach parsing
        status, rsp = handle_n_action(
            FakeEvent(),
            clarus_url="http://127.0.0.1:8024/dicomweb",
            verbose=False,
            sop_index=None,
        )
        assert status == 0x0122
        assert rsp is None
