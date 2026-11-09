# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Tests for handle_n_action — N-ACTION handler: action_type validation,
shutdown, parse errors, and WADO verify flow.

B1 (2408_writer_b1_naction_0000): a correctly-parsed request is ALWAYS
acknowledged 0x0000 (J.3.2.1.4) - success / partial / total-fail differ ONLY
by the Success / Failed SOP sequences. Protocol refusals use the standard
codes from the verdict (0x0122 no-such-action, 0x0110/0x0213
processing-failure), never the implementation-range 0xC2xx.
"""

import tempfile
from unittest.mock import MagicMock, patch

import pytest
from clean_code.handlers.naction import handle_n_action
from clean_code.sop_index import SopIndexManager

from tests.conftest import (
    make_fake_naction_event,
)

URL = "http://127.0.0.1:8024/dicomweb"


@pytest.fixture
def sop_index():
    """Temp-dir SopIndexManager for testing."""
    import shutil

    d = tempfile.mkdtemp(prefix="sop_index_test_")
    s = SopIndexManager(index_dir=d)
    yield s
    s.close()
    shutil.rmtree(d, ignore_errors=True)


class TestHandleNActionValidation:

    def test_rejects_during_shutdown(self, mock_shutdown_set, sop_index):
        """During shutdown → (0xC000, None)."""
        event = make_fake_naction_event()
        status, rsp = handle_n_action(
            event,
            URL,
            False,
            sop_index,
        )
        assert status == 0xC000
        assert rsp is None

    def test_rejects_invalid_action_type(self, mock_shutdown_not_set, sop_index):
        """action_type != 1 -> 0x0122 (no-such-action, verdict set)."""
        event = make_fake_naction_event(action_type=2)
        status, rsp = handle_n_action(
            event,
            URL,
            False,
            sop_index,
        )
        assert status == 0x0122

    def test_rejects_missing_transaction_uid(self, mock_shutdown_not_set, sop_index):
        """Missing TransactionUID -> parse error -> 0x0110 (processing failure)."""
        event = make_fake_naction_event(tx_uid="")
        status, rsp = handle_n_action(
            event,
            URL,
            False,
            sop_index,
        )
        assert status == 0x0110

    def test_action_type_none_accepted(self, mock_shutdown_not_set, sop_index):
        """action_type=None (not set) should be treated as valid (default)."""
        event = make_fake_naction_event(action_type=None, sop_list=[])
        status, rsp = handle_n_action(
            event,
            URL,
            False,
            sop_index,
        )
        # No SOPs → all success
        assert status == 0x0000


class TestHandleNActionWadoVerify:
    """M9: the RSP acknowledges receipt; the verdict is the tracker's.

    PS3.4 J.3.2: a successful N-ACTION-RSP tells the SCU only that the SCP
    RECEIVED the request. The per-SOP verdict is the N-EVENT-REPORT's job
    (J.3.3.1). These tests used to assert the OLD behaviour - a synchronous
    WADO per SOP, with the outcome in the RSP sequences - which held the
    DIMSE association open for one round-trip per instance. The RSP is now
    always status-only, and nothing here probes WADO at all.
    """

    def test_sop_not_in_index_returns_status_only_rsp(
        self, mock_shutdown_not_set, sop_index
    ):
        """An unknown SOP is not a synchronous failure: nothing was checked."""
        event = make_fake_naction_event(
            tx_uid="1.2.3.4.5",
            sop_list=[("1.2.840.10008.5.1.4.1.1.2", "1.2.3.4.5.6.7")],
        )
        with patch("clean_code.handlers.naction._wado_metadata_request") as wado:
            status, rsp = handle_n_action(
                event,
                URL,
                False,
                sop_index,
            )
        assert status == 0x0000
        assert rsp is None, "the RSP carries no per-SOP sequences (J.3.2)"
        assert not wado.called, (
            "the handler must not probe WADO: one round-trip per SOP is what "
            "kept the association open for tens of minutes"
        )

    def test_sop_in_index_wado_confirms_success(self, mock_shutdown_not_set, sop_index):
        """A stored SOP still gets 0x0000 - and still without a WADO probe."""
        sop_uid = "1.2.3.4.5.6.7"
        sop_index.insert_sop(sop_uid, "1.2.3", "1.2.3.4")

        event = make_fake_naction_event(
            tx_uid="1.2.3.4.5",
            sop_list=[("1.2.840.10008.5.1.4.1.1.2", sop_uid)],
        )
        with patch("clean_code.handlers.naction._wado_metadata_request") as wado:
            status, rsp = handle_n_action(
                event,
                URL,
                False,
                sop_index,
            )
        assert status == 0x0000
        assert rsp is None, "status-only RSP: the verdict comes later"
        assert not wado.called, "no synchronous verification in the handler"

    def test_sop_in_index_wado_fails_returns_failed(
        self, mock_shutdown_not_set, sop_index
    ):
        """A SOP the origin would reject is still accepted for receipt."""
        sop_uid = "1.2.3.4.5.6.8"
        sop_index.insert_sop(sop_uid, "1.2.3", "1.2.3.4")

        event = make_fake_naction_event(
            tx_uid="1.2.3.4.5",
            sop_list=[("1.2.840.10008.5.1.4.1.1.2", sop_uid)],
        )
        with patch("clean_code.handlers.naction._wado_metadata_request") as wado:
            status, rsp = handle_n_action(
                event,
                URL,
                False,
                sop_index,
            )
        assert status == 0x0000
        assert rsp is None
        assert not wado.called, "the handler no longer verifies; the tracker does"

    def test_mixed_success_failure(self, mock_shutdown_not_set, sop_index):
        """A mixed batch is acknowledged whole; the split is the report's job."""
        sop_good = "1.2.3.4.5.6.9"
        sop_bad = "1.2.3.4.5.6.10"
        sop_index.insert_sop(sop_good, "1.2.3", "1.2.3.4")
        # sop_bad NOT in index

        event = make_fake_naction_event(
            tx_uid="1.2.3.4.5",
            sop_list=[
                ("1.2.840.10008.5.1.4.1.1.2", sop_good),
                ("1.2.840.10008.5.1.4.1.1.2", sop_bad),
            ],
        )
        with patch("clean_code.handlers.naction._wado_metadata_request") as wado:
            status, rsp = handle_n_action(
                event,
                URL,
                False,
                sop_index,
            )
        assert status == 0x0000
        assert rsp is None, (
            "neither sequence is filled: at this instant nothing has been "
            "checked, so a per-SOP claim would be evidence-free"
        )
        assert not wado.called


class TestNActionRegistersInCommitTracker:
    """Stage 2: handle_n_action registers the batch for the async report."""

    def _event_with_requestor(
        self, sop_list, calling_ae="TEST_SCU", address="10.0.0.5", port=11112
    ):
        event = make_fake_naction_event(
            tx_uid="1.2.3.4.5", sop_list=sop_list, calling_ae=calling_ae
        )
        Req = type(
            "Req", (), {"ae_title": calling_ae, "address": address, "port": port}
        )
        Assoc = type("Assoc", (), {"requestor": Req()})
        event.assoc = Assoc()
        return event

    def test_registers_batch_when_tracker_provided(
        self, mock_shutdown_not_set, sop_index
    ):
        """Batch + requestor address/port -> registered in the tracker."""
        from clean_code.commit_tracker import CommitTracker

        tracker = CommitTracker(verify=lambda c, u: True, report=lambda *a, **k: None)

        sop_good = "1.2.3.4.5.6.11"
        sop_bad = "1.2.3.4.5.6.12"
        sop_index.insert_sop(sop_good, "1.2.3", "1.2.3.4")
        event = self._event_with_requestor(
            sop_list=[
                ("1.2.840.10008.5.1.4.1.1.2", sop_good),
                ("1.2.840.10008.5.1.4.1.1.2", sop_bad),
            ]
        )

        mock_resp = MagicMock()
        mock_resp.status_code = 200
        with patch(
            "clean_code.handlers.naction._wado_metadata_request", return_value=mock_resp
        ):
            status, rsp = handle_n_action(
                event,
                URL,
                False,
                sop_index,
                commit_tracker=tracker,
                # PS3.8: A-ASSOCIATE carries no listening port, so the peer
                # config is the only source of one. `requestor.port` is the
                # association's ephemeral SOURCE port and must never be used.
                resolve_scp=lambda ae, addr: ("10.0.0.5", 11112),
            )

        # RSP is synchronous as before; upper status 0x0000 (B1).
        assert status == 0x0000
        # Batch is registered in the tracker for the async report.
        assert tracker.pending_count() == 1

    def test_no_registration_without_tracker(self, mock_shutdown_not_set, sop_index):
        """No tracker -> no registration, and still a status-only RSP."""
        sop_good = "1.2.3.4.5.6.13"
        sop_index.insert_sop(sop_good, "1.2.3", "1.2.3.4")
        event = self._event_with_requestor(
            sop_list=[
                ("1.2.840.10008.5.1.4.1.1.2", sop_good),
            ]
        )
        status, rsp = handle_n_action(
            event,
            URL,
            False,
            sop_index,
        )
        assert status == 0x0000
        assert rsp is None, "the RSP is status-only whether or not a tracker exists"

    def test_registers_using_resolved_address_when_provided(
        self, mock_shutdown_not_set, sop_index
    ):
        """resolve_scp result overrides the ephemeral source address/port."""
        from clean_code.commit_tracker import CommitTracker

        tracker = CommitTracker(verify=lambda c, u: True, report=lambda *a, **k: None)
        sop_good = "1.2.3.4.5.6.14"
        sop_index.insert_sop(sop_good, "1.2.3", "1.2.3.4")
        event = self._event_with_requestor(
            sop_list=[
                ("1.2.840.10008.5.1.4.1.1.2", sop_good),
            ]
        )
        mock_resp = MagicMock()
        mock_resp.status_code = 200
        with patch(
            "clean_code.handlers.naction._wado_metadata_request", return_value=mock_resp
        ):
            handle_n_action(
                event,
                URL,
                False,
                sop_index,
                commit_tracker=tracker,
                resolve_scp=lambda ae, ip: ("192.168.0.10", 12345),
            )
        jobs = list(tracker._jobs.values())
        assert len(jobs) == 1
        assert jobs[0].host == "192.168.0.10"
        assert jobs[0].port == 12345
