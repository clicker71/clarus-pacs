# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

"""Tests for Storage Commitment N-ACTION (CMP-008).

Covers: _send_storage_commitment() in clean_code/handlers/cmove.py
"""

import logging
from unittest.mock import MagicMock, PropertyMock, patch

# ── Helpers ──────────────────────────────────────────────────────────


def _make_mock_assoc(is_established=True, storage_commitment_accepted=True):
    """Create a mock pynetdicom Association."""
    assoc = MagicMock()
    assoc.is_established = is_established

    if storage_commitment_accepted:
        ctx = MagicMock()
        type(ctx).abstract_syntax = PropertyMock(return_value="1.2.840.10008.1.20.1")
        assoc.accepted_contexts = [ctx]
    else:
        assoc.accepted_contexts = []

    return assoc


def _make_mock_status(status_code):
    """Create a mock pynetdicom Status with given code."""
    status = MagicMock()
    status.Status = status_code
    return status


# ── Tests ────────────────────────────────────────────────────────────


class TestStorageCommitment:

    def test_send_storage_commitment_success(self, caplog):
        """N-ACTION returns 0x0000 → True, log says 'Success'."""
        from clean_code.handlers.cmove import _send_storage_commitment

        sop_list = [
            ("1.2.840.10008.5.1.4.1.1.2", "1.2.840.10008.5.1.4.1.1.2.999"),
        ]

        mock_assoc = _make_mock_assoc(
            is_established=True, storage_commitment_accepted=True
        )
        mock_assoc.send_n_action.return_value = (
            _make_mock_status(0x0000),
            None,
        )

        with caplog.at_level(logging.INFO):
            with patch("clean_code.handlers.cmove.AE") as mock_ae_class:
                mock_ae = MagicMock()
                mock_ae.associate.return_value = mock_assoc
                mock_ae_class.return_value = mock_ae

                result = _send_storage_commitment(
                    move_dest_ae="WEASIS",
                    target_host="192.168.1.20",
                    target_port=11112,
                    sop_list=sop_list,
                    bridge_ae_title="CLARUS_BRIDGE",
                )

        assert result is True
        log_text = "\n".join(r.message for r in caplog.records)
        assert "Storage Commitment" in log_text
        assert "0x0000" in log_text or "Success" in log_text

    def test_send_storage_commitment_class_instance_conflict(self, caplog):
        """N-ACTION returns 0x0118 → True (request sent), log says
        'Class-Instance Conflict'."""
        from clean_code.handlers.cmove import _send_storage_commitment

        sop_list = [
            ("1.2.840.10008.5.1.4.1.1.2", "1.2.840.10008.5.1.4.1.1.2.888"),
        ]

        mock_assoc = _make_mock_assoc(
            is_established=True, storage_commitment_accepted=True
        )
        mock_assoc.send_n_action.return_value = (
            _make_mock_status(0x0118),
            None,
        )

        with caplog.at_level(logging.INFO):
            with patch("clean_code.handlers.cmove.AE") as mock_ae_class:
                mock_ae = MagicMock()
                mock_ae.associate.return_value = mock_assoc
                mock_ae_class.return_value = mock_ae

                result = _send_storage_commitment(
                    move_dest_ae="WEASIS",
                    target_host="192.168.1.20",
                    target_port=11112,
                    sop_list=sop_list,
                    bridge_ae_title="CLARUS_BRIDGE",
                )

        # PS3.7 Annex C: 0x0118 is a FAILURE status - the SCP refused to open
        # the transaction because it cannot commit that SOP class. Reporting
        # success claimed a commitment nobody holds.
        assert result is False
        log_text = "\n".join(r.message for r in caplog.records)
        assert "Class-Instance Conflict" in log_text
        assert "not committed" in log_text

    def test_send_storage_commitment_c_space_warning_is_not_committed(self, caplog):
        """0x0001/0x0002 are C-* warnings the SC service does not define.

        PS3.4 J.3.2 gives the Storage Commitment N-ACTION-RSP exactly two
        outcomes: 0x0000 Success (the transaction was opened) and 0x0110
        Processing Failure. Its Warning class is 0x0107/0x0116 (attribute
        notes). 0x0001 "warning - SOP class not supported" belongs to the
        C-ECHO/C-STORE tables, so an SCP answering it accepted nothing - and
        treating it as accepted reported a commitment nobody holds.
        """
        from clean_code.handlers.cmove import _send_storage_commitment

        sop_list = [
            ("1.2.840.10008.5.1.4.1.1.2", "1.2.840.10008.5.1.4.1.1.2.777"),
        ]

        mock_assoc = _make_mock_assoc(
            is_established=True, storage_commitment_accepted=True
        )
        mock_assoc.send_n_action.return_value = (_make_mock_status(0x0001), None)

        with caplog.at_level(logging.INFO):
            with patch("clean_code.handlers.cmove.AE") as mock_ae_class:
                mock_ae = MagicMock()
                mock_ae.associate.return_value = mock_assoc
                mock_ae_class.return_value = mock_ae

                result = _send_storage_commitment(
                    move_dest_ae="WEASIS",
                    target_host="192.168.1.20",
                    target_port=11112,
                    sop_list=sop_list,
                    bridge_ae_title="CLARUS_BRIDGE",
                )

        assert result is False, (
            "0x0001 is not a Storage Commitment success - the transaction was "
            "never opened"
        )
        assert "not committed" in "\n".join(r.message for r in caplog.records)

    def test_send_storage_commitment_scp_unavailable(self, caplog):
        """Association fails → False, log says 'unavailable'."""
        from clean_code.handlers.cmove import _send_storage_commitment

        sop_list = [
            ("1.2.840.10008.5.1.4.1.1.2", "1.2.840.10008.5.1.4.1.1.2.777"),
        ]

        mock_assoc = _make_mock_assoc(is_established=False)

        with caplog.at_level(logging.INFO):
            with patch("clean_code.handlers.cmove.AE") as mock_ae_class:
                mock_ae = MagicMock()
                mock_ae.associate.return_value = mock_assoc
                mock_ae_class.return_value = mock_ae

                result = _send_storage_commitment(
                    move_dest_ae="WEASIS",
                    target_host="192.168.1.20",
                    target_port=11112,
                    sop_list=sop_list,
                    bridge_ae_title="CLARUS_BRIDGE",
                )

        assert result is False
        log_text = "\n".join(r.message for r in caplog.records)
        assert "unavailable" in log_text.lower()

    def test_send_storage_commitment_no_sc_support(self, caplog):
        """SCP doesn't support Storage Commitment → False, log says
        'does not support'."""
        from clean_code.handlers.cmove import _send_storage_commitment

        sop_list = [
            ("1.2.840.10008.5.1.4.1.1.2", "1.2.840.10008.5.1.4.1.1.2.666"),
        ]

        mock_assoc = _make_mock_assoc(
            is_established=True, storage_commitment_accepted=False
        )

        with caplog.at_level(logging.INFO):
            with patch("clean_code.handlers.cmove.AE") as mock_ae_class:
                mock_ae = MagicMock()
                mock_ae.associate.return_value = mock_assoc
                mock_ae_class.return_value = mock_ae

                result = _send_storage_commitment(
                    move_dest_ae="WEASIS",
                    target_host="192.168.1.20",
                    target_port=11112,
                    sop_list=sop_list,
                    bridge_ae_title="CLARUS_BRIDGE",
                )

        assert result is False
        log_text = "\n".join(r.message for r in caplog.records)
        assert "does not support" in log_text.lower()
