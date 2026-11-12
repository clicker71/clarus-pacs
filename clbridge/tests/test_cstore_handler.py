# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Tests for handle_c_store — full C-STORE handler flow.

P31: Tests shutdown rejection, STOW forwarding (success/failure),
oversized dataset rejection, unsupported SOP class, and missing SOPInstanceUID.
"""

from clean_code.handlers.cstore import handle_c_store

from tests.conftest import (
    make_fake_cstore_event,
)

URL = "http://127.0.0.1:8024/dicomweb"


class TestHandleCStoreFlow:

    def test_rejects_during_shutdown(self, mock_shutdown_set):
        """During shutdown, handle_c_store returns 0xC000 immediately."""
        event = make_fake_cstore_event()
        status = handle_c_store(event, URL, False)
        assert status == 0xC000

    def test_forwards_to_stow_and_returns_success(
        self, mock_shutdown_not_set, mock_stow_success
    ):
        """Normal flow: STOW succeeds → 0x0000."""
        event = make_fake_cstore_event()
        status = handle_c_store(event, URL, False)
        assert status == 0x0000
        mock_stow_success.assert_called_once()

    def test_stow_failure_returns_c000(self, mock_shutdown_not_set, mock_stow_failure):
        """STOW failure → 0xC000."""
        event = make_fake_cstore_event()
        status = handle_c_store(event, URL, False)
        assert status == 0xC000
        mock_stow_failure.assert_called_once()

    def test_rejects_oversized_dataset(self, mock_shutdown_not_set, mock_stow_success):
        """Dataset exceeding max_store_bytes → 0xA700 (Out of Resources)."""
        event = make_fake_cstore_event()
        # max_store_bytes=100 — any real DICOM is larger
        status = handle_c_store(
            event,
            URL,
            False,
            max_store_bytes=100,
        )
        assert status == 0xA700
        mock_stow_success.assert_not_called()

    def test_rejects_unsupported_sop_class(self, mock_shutdown_not_set):
        """Unknown SOP Class UID → 0x0122 SOP Class not supported (PS3.7
        Annex C; 0xA801 does not exist in the C-STORE table, PS3.4 Table
        B.2-1)."""
        event = make_fake_cstore_event(sop_class_uid="9.9.9.9.9")
        sop_registry = {"1.2.840.10008.5.1.4.1.1.2": {"accept": True}}
        status = handle_c_store(
            event,
            URL,
            False,
            sop_registry=sop_registry,
        )
        assert status == 0x0122

    def test_disabled_sop_class_rejected(self, mock_shutdown_not_set):
        """SOP class with accept=False → 0x0122 (SOP Class not supported)."""
        event = make_fake_cstore_event(sop_class_uid="1.2.840.10008.5.1.4.1.1.2")
        sop_registry = {"1.2.840.10008.5.1.4.1.1.2": {"accept": False}}
        status = handle_c_store(
            event,
            URL,
            False,
            sop_registry=sop_registry,
        )
        assert status == 0x0122

    def test_missing_sop_instance_uid_rejected_a900(
        self, mock_shutdown_not_set, mock_stow_success
    ):
        """Missing SOPInstanceUID (Type 1) -> 0xA900, NOT enqueued/acknowledged.

        Enqueueing an instance without its SOP Instance UID produced an empty
        outbox key (perpetual "duplicate", never persisted) while the modality
        still got 0x0000. The instance must be refused before it reaches STOW."""
        event = make_fake_cstore_event(sop_instance_uid="")
        status = handle_c_store(event, URL, False)
        assert status == 0xA900
        mock_stow_success.assert_not_called()

    def test_max_store_bytes_zero_allows_any_size(
        self, mock_shutdown_not_set, mock_stow_success
    ):
        """max_store_bytes=0 disables size check — large dataset accepted."""
        event = make_fake_cstore_event()
        status = handle_c_store(
            event,
            URL,
            False,
            max_store_bytes=0,
        )
        assert status == 0x0000
        mock_stow_success.assert_called_once()
