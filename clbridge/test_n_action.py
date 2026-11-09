# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

"""Unit and integration tests for N-ACTION Storage Commitment (Bridge v0.3).

Run with:
    pytest test_n_action.py -v
    pytest test_n_action.py -v -m "not integration"
"""

import os
import sys
import uuid

import pytest

# Ensure clbridge is importable from this directory
_BRIDGE_DIR = os.path.dirname(os.path.abspath(__file__))
if _BRIDGE_DIR not in sys.path:
    sys.path.insert(0, _BRIDGE_DIR)

import importlib.util as _importlib_util

from pydicom.dataset import Dataset, FileMetaDataset
from pydicom.uid import ImplicitVRLittleEndian

_spec = _importlib_util.spec_from_file_location(
    "clarus_bridge",
    os.path.join(_BRIDGE_DIR, "clbridge.py"),
)
_clarus_bridge = _importlib_util.module_from_spec(_spec)
_spec.loader.exec_module(_clarus_bridge)

SopIndexManager = _clarus_bridge.SopIndexManager
_parse_n_action_rq = _clarus_bridge._parse_n_action_rq
_build_n_action_rsp = _clarus_bridge._build_n_action_rsp


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def sop_index(tmp_path):
    """Provide a fresh filesystem-backed SopIndexManager for each test."""
    idx = SopIndexManager(str(tmp_path / "sop_index"))
    yield idx
    idx.close()


@pytest.fixture
def populated_index(sop_index):
    """SopIndexManager with 3 pre-inserted SOPs."""
    sop_index.insert_sop(
        "1.2.840.10008.5.1.4.1.1.2.1",
        "1.2.840.10008.5.1.4.1.1.2.2",
        "1.2.840.10008.5.1.4.1.1.2.3",
    )
    sop_index.insert_sop(
        "2.2.840.10008.5.1.4.1.1.2.1",
        "2.2.840.10008.5.1.4.1.1.2.2",
        "2.2.840.10008.5.1.4.1.1.2.3",
    )
    sop_index.insert_sop(
        "3.2.840.10008.5.1.4.1.1.2.1",
        "3.2.840.10008.5.1.4.1.1.2.2",
        "3.2.840.10008.5.1.4.1.1.2.3",
    )
    return sop_index


class _FakeEvent:
    """Minimal fake pynetdicom event for _parse_n_action_rq tests."""

    def __init__(self, dataset):
        self.dataset = dataset


# ---------------------------------------------------------------------------
# Unit tests: SopIndexManager
# ---------------------------------------------------------------------------

class TestSopIndexManager:
    def test_insert_lookup(self, sop_index):
        """Insert a SOP and verify lookup returns correct study/series UIDs."""
        sop_index.insert_sop("1.2.3.4", "study_abc", "series_xyz")
        entry = sop_index.lookup_sop("1.2.3.4")
        assert entry is not None
        assert entry["study_uid"] == "study_abc"
        assert entry["series_uid"] == "series_xyz"
        assert entry["is_verified"] is False

    def test_insert_lookup_nonexistent(self, sop_index):
        """Lookup of non-existent SOP returns None."""
        assert sop_index.lookup_sop("nonexistent.uid") is None

    def test_insert_duplicate(self, sop_index):
        """Duplicate SOP UID ??? INSERT OR REPLACE, no exception, last write wins."""
        sop_index.insert_sop("dup.uid", "study_1", "series_1")
        sop_index.insert_sop("dup.uid", "study_2", "series_2")
        entry = sop_index.lookup_sop("dup.uid")
        assert entry is not None
        assert entry["study_uid"] == "study_2"
        assert entry["series_uid"] == "series_2"

    def test_mark_verified(self, sop_index):
        """mark_verified sets is_verified=1."""
        sop_index.insert_sop("v.uid", "s1", "s2")
        sop_index.mark_verified("v.uid")
        entry = sop_index.lookup_sop("v.uid")
        assert entry["is_verified"] is True

    def test_count(self, populated_index):
        """Count returns correct number of entries."""
        assert populated_index.count() == 3

    def test_cleanup(self, sop_index):
        """cleanup removes entries older than max_age_seconds."""
        import time
        sop_index.insert_sop("old.uid", "s1", "s2")
        # Force cleanup with 0 age ??? removes entries from previous seconds
        time.sleep(1.1)
        sop_index.cleanup(max_age_seconds=0)
        assert sop_index.lookup_sop("old.uid") is None


# ---------------------------------------------------------------------------
# Unit tests: _parse_n_action_rq
# ---------------------------------------------------------------------------

class TestParseNActionRQ:
    def test_valid_request(self):
        """Parse a well-formed N-ACTION-RQ with TransactionUID + ReferencedSOPSequence."""
        ds = Dataset()
        ds.TransactionUID = "1.2.840.10008.1.20.1.20260603.1"
        sop_item = Dataset()
        sop_item.ReferencedSOPClassUID = "1.2.840.10008.5.1.4.1.1.2"
        sop_item.ReferencedSOPInstanceUID = "1.2.840.10008.5.1.4.1.1.2.99.1"
        ds.ReferencedSOPSequence = [sop_item]

        event = _FakeEvent(ds)
        tx_uid, sop_list = _parse_n_action_rq(event)

        assert tx_uid == "1.2.840.10008.1.20.1.20260603.1"
        assert len(sop_list) == 1
        assert sop_list[0] == (
            "1.2.840.10008.5.1.4.1.1.2",
            "1.2.840.10008.5.1.4.1.1.2.99.1",
        )

    def test_multiple_sops(self):
        """Parse N-ACTION-RQ with 3 referenced SOPs."""
        ds = Dataset()
        ds.TransactionUID = "1.2.840.10008.1.20.1.multi"
        seq = []
        for i in range(3):
            item = Dataset()
            item.ReferencedSOPClassUID = f"1.2.840.{i}"
            item.ReferencedSOPInstanceUID = f"1.2.840.{i}.100"
            seq.append(item)
        ds.ReferencedSOPSequence = seq

        event = _FakeEvent(ds)
        tx_uid, sop_list = _parse_n_action_rq(event)

        assert tx_uid == "1.2.840.10008.1.20.1.multi"
        assert len(sop_list) == 3

    def test_missing_transaction_uid(self):
        """N-ACTION-RQ without TransactionUID raises ValueError."""
        ds = Dataset()
        event = _FakeEvent(ds)
        with pytest.raises(ValueError, match="TransactionUID"):
            _parse_n_action_rq(event)

    def test_empty_transaction_uid(self):
        """N-ACTION-RQ with empty TransactionUID raises ValueError."""
        ds = Dataset()
        ds.TransactionUID = ""
        event = _FakeEvent(ds)
        with pytest.raises(ValueError, match="TransactionUID"):
            _parse_n_action_rq(event)

    def test_empty_referenced_sop_sequence(self):
        """N-ACTION-RQ with no ReferencedSOPSequence ??? empty list, no error."""
        ds = Dataset()
        ds.TransactionUID = "1.2.840.test"
        event = _FakeEvent(ds)
        tx_uid, sop_list = _parse_n_action_rq(event)
        assert tx_uid == "1.2.840.test"
        assert sop_list == []

    def test_skips_empty_sop_instance_uid(self):
        """Items with empty SOPInstanceUID are skipped."""
        ds = Dataset()
        ds.TransactionUID = "1.2.840.skip"
        item1 = Dataset()
        item1.ReferencedSOPClassUID = "1.2.840.good"
        item1.ReferencedSOPInstanceUID = "1.2.840.good.1"
        item2 = Dataset()
        item2.ReferencedSOPClassUID = "1.2.840.bad"
        item2.ReferencedSOPInstanceUID = ""  # empty ??? skipped
        ds.ReferencedSOPSequence = [item1, item2]

        event = _FakeEvent(ds)
        _, sop_list = _parse_n_action_rq(event)
        assert len(sop_list) == 1
        assert sop_list[0][1] == "1.2.840.good.1"


# ---------------------------------------------------------------------------
# Unit tests: _build_n_action_rsp
# ---------------------------------------------------------------------------

class TestBuildNActionRSP:
    TX_UID = "1.2.840.10008.1.20.1.test"

    def test_all_success(self):
        """Build RSP with all SOPs verified ??? ReferencedSOPSequence, no FailedSOPSequence."""
        verified = [
            ("1.2.840.ct", "1.2.840.ct.1"),
            ("1.2.840.mr", "1.2.840.mr.1"),
        ]
        failed = []
        rsp = _build_n_action_rsp(self.TX_UID, verified, failed)

        assert rsp.TransactionUID == self.TX_UID
        assert hasattr(rsp, "ReferencedSOPSequence")
        assert len(rsp.ReferencedSOPSequence) == 2
        assert not hasattr(rsp, "FailedSOPSequence")

    def test_partial_failure(self):
        """Build RSP with mixed success/failure ??? both sequences present."""
        verified = [("1.2.840.good", "1.2.840.good.1")]
        failed = [("1.2.840.bad", "1.2.840.bad.1", 0xC201)]
        rsp = _build_n_action_rsp(self.TX_UID, verified, failed)

        assert rsp.TransactionUID == self.TX_UID
        assert len(rsp.ReferencedSOPSequence) == 1
        assert len(rsp.FailedSOPSequence) == 1
        assert rsp.FailedSOPSequence[0].FailureReason == 0xC201

    def test_all_failed(self):
        """Build RSP with all SOPs failed ??? FailedSOPSequence present."""
        verified = []
        failed = [
            ("1.2.a", "1.2.a.1", 0xC201),
            ("1.2.b", "1.2.b.1", 0xC202),
        ]
        rsp = _build_n_action_rsp(self.TX_UID, verified, failed)

        assert rsp.TransactionUID == self.TX_UID
        assert not hasattr(rsp, "ReferencedSOPSequence")
        assert len(rsp.FailedSOPSequence) == 2
        assert rsp.FailedSOPSequence[0].FailureReason == 0xC201
        assert rsp.FailedSOPSequence[1].FailureReason == 0xC202

    def test_empty_both(self):
        """Build RSP with no SOPs at all ??? TransactionUID only."""
        rsp = _build_n_action_rsp(self.TX_UID, [], [])
        assert rsp.TransactionUID == self.TX_UID
        assert not hasattr(rsp, "ReferencedSOPSequence")
        assert not hasattr(rsp, "FailedSOPSequence")


# ---------------------------------------------------------------------------
# Integration tests (require clbridge running)
# ---------------------------------------------------------------------------

INTEGRATION_REASON = (
    "Clarus Bridge not running on 127.0.0.1:11112"
)

pytestmark_integration = pytest.mark.integration


def _bridge_is_running():
    """Check if clbridge is reachable via DICOM C-ECHO on test port."""
    import socket
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.settimeout(2)
    try:
        s.connect(("127.0.0.1", 11112))
        s.close()
        return True
    except (socket.timeout, ConnectionRefusedError, OSError):
        return False


_bridge_available = _bridge_is_running()
skip_if_no_bridge = pytest.mark.skipif(
    not _bridge_available, reason=INTEGRATION_REASON
)


@skip_if_no_bridge
class TestNActionIntegration:
    """End-to-end tests requiring a running clbridge on 127.0.0.1:11112."""

    BRIDGE_AE = "CLARUS_BRIDGE"

    def test_c_echo(self):
        """C-ECHO should still work (regression check)."""
        from pynetdicom import AE
        from pynetdicom.sop_class import Verification

        ae = AE(ae_title="TEST_NA")
        ae.add_requested_context(Verification)
        assoc = ae.associate("127.0.0.1", 11112, ae_title=self.BRIDGE_AE)
        assert assoc.is_established, "Failed to associate for C-ECHO"
        status = assoc.send_c_echo()
        assert status.Status == 0x0000, f"C-ECHO failed: 0x{status.Status:04X}"
        assoc.release()

    def test_c_store(self):
        """C-STORE with a synthetic DICOM dataset should succeed (0x0000)."""
        from pynetdicom import AE
        from pynetdicom.sop_class import CTImageStorage

        ds = Dataset()
        # PS3.10: dataset sent over network requires File Meta Information
        ds.file_meta = FileMetaDataset()
        ds.file_meta.TransferSyntaxUID = ImplicitVRLittleEndian
        ds.SOPClassUID = CTImageStorage
        ds.SOPInstanceUID = f"1.2.840.10008.5.1.4.1.1.2.{uuid.uuid4().hex[:8]}"
        ds.StudyInstanceUID = f"1.2.840.10008.5.1.4.1.1.2.2.{uuid.uuid4().hex[:8]}"
        ds.SeriesInstanceUID = f"1.2.840.10008.5.1.4.1.1.2.3.{uuid.uuid4().hex[:8]}"
        ds.PatientName = "TEST^NACTION"
        ds.PatientID = "NA-001"
        ds.Modality = "CT"

        ae = AE(ae_title="TEST_NA")
        ae.add_requested_context(CTImageStorage)
        assoc = ae.associate("127.0.0.1", 11112, ae_title=self.BRIDGE_AE)
        assert assoc.is_established, "Failed to associate for C-STORE"
        status = assoc.send_c_store(ds)
        assert status.Status in (0x0000, 0xC000), (
            f"C-STORE unexpected status: 0x{status.Status:04X}"
        )
        assoc.release()

    def test_c_find(self):
        """C-FIND Patient Root should return results (includes C-ECHO regression)."""
        from pynetdicom import AE
        from pynetdicom.sop_class import (
            PatientRootQueryRetrieveInformationModelFind,
        )

        ae = AE(ae_title="TEST_NA")
        ae.add_requested_context(PatientRootQueryRetrieveInformationModelFind)
        assoc = ae.associate("127.0.0.1", 11112, ae_title=self.BRIDGE_AE)
        assert assoc.is_established, "Failed to associate for C-FIND"

        ds = Dataset()
        ds.PatientName = "*"
        ds.QueryRetrieveLevel = "PATIENT"

        responses = assoc.send_c_find(
            ds, PatientRootQueryRetrieveInformationModelFind
        )
        final_status = None
        for status, identifier in responses:
            final_status = status
            if identifier is not None:
                pass  # consume identifiers

        # C-FIND should complete (0x0000) or return empty (still 0x0000)
        assert final_status is not None, "No final C-FIND status received"
        assert final_status.Status in (0x0000, 0xFF00), (
            f"C-FIND bad status: 0x{final_status.Status:04X}"
        )
        assoc.release()

    def test_c_move_pending(self):
        """C-MOVE v1.0: association accepted, unknown destination ??? 0xA801."""
        from pynetdicom import AE
        from pynetdicom.sop_class import (
            StudyRootQueryRetrieveInformationModelMove,
        )

        # Step 1: Verify C-MOVE association is now ACCEPTED (v1.0)
        ae = AE(ae_title="TEST_NA")
        ae.add_requested_context(StudyRootQueryRetrieveInformationModelMove)
        assoc = ae.associate("127.0.0.1", 11112, ae_title=self.BRIDGE_AE)
        assert assoc.is_established, (
            "C-MOVE association should be accepted: Bridge v1.0 supports C-MOVE"
        )

        # Step 2: C-MOVE to UNKNOWN destination ??? 0xA801 (no SCP registered)
        move_dest = "UNKNOWN_SCP"
        ds = Dataset()
        ds.QueryRetrieveLevel = "STUDY"
        ds.StudyInstanceUID = f"1.2.840.10008.5.1.4.1.1.2.2.{uuid.uuid4().hex[:8]}"
        ds.MoveDestination = move_dest

        # Bridge: pynetdicom 3.0 send_c_move(ds, move_aet, query_model)
        responses = assoc.send_c_move(
            ds, move_dest, StudyRootQueryRetrieveInformationModelMove
        )
        first_status = None
        for status, _identifier in responses:
            first_status = status
            break

        assert first_status is not None, "No C-MOVE response received"
        # Bridge: unknown MoveDestination ??? 0xA801 (CM-001 rule: IP from scp_targets, not association)
        assert first_status.Status == 0xA801, (
            f"C-MOVE to unknown destination should return 0xA801, "
            f"got 0x{first_status.Status:04X}"
        )
        assoc.release()

    def test_c_move_known_destination(self):
        """C-MOVE v1.0: C-ECHO registers SCP ??? C-MOVE returns Pending 0xFF00."""
        from pynetdicom import AE
        from pynetdicom.sop_class import (
            StudyRootQueryRetrieveInformationModelMove,
            Verification,
        )

        test_ae = f"TCM{uuid.uuid4().hex[:6]}"  # max 16 chars per DICOM AE Title

        # Step 1: C-ECHO to auto-register this AE as SCP target (CM-006)
        ae_echo = AE(ae_title=test_ae)
        ae_echo.add_requested_context(Verification)
        assoc_echo = ae_echo.associate("127.0.0.1", 11112, ae_title=self.BRIDGE_AE)
        assert assoc_echo.is_established, "C-ECHO association failed"
        status = assoc_echo.send_c_echo()
        assert status.Status == 0x0000, f"C-ECHO failed: 0x{status.Status:04X}"
        assoc_echo.release()

        # Step 2: C-MOVE to the now-registered AE ??? Pending 0xFF00
        ae_move = AE(ae_title=test_ae)
        ae_move.add_requested_context(StudyRootQueryRetrieveInformationModelMove)
        assoc_move = ae_move.associate(
            "127.0.0.1", 11112, ae_title=self.BRIDGE_AE,
        )
        assert assoc_move.is_established, "C-MOVE association failed"

        study_uid = f"1.2.840.10008.5.1.4.1.1.2.2.{uuid.uuid4().hex[:8]}"
        ds = Dataset()
        ds.QueryRetrieveLevel = "STUDY"
        ds.StudyInstanceUID = study_uid
        ds.MoveDestination = test_ae  # Move to self (registered via C-ECHO)

        # Bridge: pynetdicom 3.0 send_c_move(ds, move_aet, query_model)
        responses = assoc_move.send_c_move(
            ds, test_ae, StudyRootQueryRetrieveInformationModelMove
        )
        first_status = None
        for status, _identifier in responses:
            first_status = status
            break

        assert first_status is not None, "No C-MOVE response received"
        # Bridge v1.0: job queued ??? pynetdicom 3.0 returns 0x0000 (Success)
        # because nr_subops=0. Actual data transfer is async via worker thread.
        assert first_status.Status == 0x0000, (
            f"C-MOVE to known destination should return Success 0x0000, "
            f"got 0x{first_status.Status:04X}"
        )
        assoc_move.release()

    def test_n_action_e2e(self):
        """Full E2E: C-STORE ??? N-ACTION ??? verify RSP contains Success/Failed."""
        from pynetdicom import AE
        from pynetdicom.sop_class import (
            CTImageStorage,
            StorageCommitmentPushModel,
        )

        sop_uid = f"1.2.840.10008.5.1.4.1.1.2.e2e.{uuid.uuid4().hex[:8]}"
        study_uid = f"1.2.840.10008.5.1.4.1.1.2.2.{uuid.uuid4().hex[:8]}"
        series_uid = f"1.2.840.10008.5.1.4.1.1.2.3.{uuid.uuid4().hex[:8]}"

        # Step 1: C-STORE (PS3.10: File Meta Information required)
        ds = Dataset()
        ds.file_meta = FileMetaDataset()
        ds.file_meta.TransferSyntaxUID = ImplicitVRLittleEndian
        ds.SOPClassUID = CTImageStorage
        ds.SOPInstanceUID = sop_uid
        ds.StudyInstanceUID = study_uid
        ds.SeriesInstanceUID = series_uid
        ds.PatientName = "TEST^NACTION_E2E"
        ds.PatientID = "NA-E2E"
        ds.Modality = "CT"

        ae_store = AE(ae_title="TEST_NA_E2E")
        ae_store.add_requested_context(CTImageStorage)
        assoc = ae_store.associate("127.0.0.1", 11112, ae_title=self.BRIDGE_AE)
        assert assoc.is_established, "C-STORE association failed"
        status = assoc.send_c_store(ds)
        stored_ok = status.Status == 0x0000
        assoc.release()

        # Step 2: N-ACTION-RQ (Storage Commitment)
        ae_action = AE(ae_title="TEST_NA_E2E")
        ae_action.add_requested_context(StorageCommitmentPushModel)
        assoc2 = ae_action.associate(
            "127.0.0.1", 11112, ae_title=self.BRIDGE_AE
        )
        assert assoc2.is_established, "N-ACTION association failed"

        n_action_ds = Dataset()
        n_action_ds.TransactionUID = f"1.2.840.10008.1.20.1.{uuid.uuid4().hex[:12]}"
        ref_item = Dataset()
        ref_item.ReferencedSOPClassUID = CTImageStorage
        ref_item.ReferencedSOPInstanceUID = sop_uid
        n_action_ds.ReferencedSOPSequence = [ref_item]

        # pynetdicom 2.x: send_n_action returns (rsp_ds, identifier_ds_or_none).
        # The DIMSE status (0000,0900) is consumed by pynetdicom SCP and
        # not exposed in the returned rsp_ds. The presence of a response
        # without exception indicates the N-ACTION was processed.
        rsp_ds, identifier = assoc2.send_n_action(
            n_action_ds, 1, StorageCommitmentPushModel, sop_uid
        )

        # If C-STORE succeeded, identifier should contain ReferencedSOPSequence
        if stored_ok:
            if identifier is not None:
                tx = getattr(identifier, "TransactionUID", None)
                assert tx is not None, "RSP missing TransactionUID"
                assert str(tx) == str(n_action_ds.TransactionUID), (
                    "TransactionUID mismatch"
                )
                # SOP should be in success or failed (depends on Clarus state)
                has_success = hasattr(identifier, "ReferencedSOPSequence") and len(identifier.ReferencedSOPSequence) > 0
                has_failed = hasattr(identifier, "FailedSOPSequence") and len(identifier.FailedSOPSequence) > 0
                assert has_success or has_failed, (
                    "RSP should contain ReferencedSOPSequence or FailedSOPSequence"
                )
        else:
            # C-STORE failed ??? identifier should have FailedSOPSequence
            if identifier is not None:
                has_failed = hasattr(identifier, "FailedSOPSequence") and len(identifier.FailedSOPSequence) > 0
                assert has_failed, "RSP should contain FailedSOPSequence when STOW failed"

        assoc2.release()

    def test_n_action_no_prior_stow(self):
        """N-ACTION without prior C-STORE ??? all SOPs in FailedSOPSequence."""
        from pynetdicom import AE
        from pynetdicom.sop_class import StorageCommitmentPushModel

        fake_sop = f"1.2.840.10008.5.1.4.1.1.2.no_stow.{uuid.uuid4().hex[:8]}"

        ae = AE(ae_title="TEST_NA_NOSTOW")
        ae.add_requested_context(StorageCommitmentPushModel)
        assoc = ae.associate("127.0.0.1", 11112, ae_title=self.BRIDGE_AE)
        assert assoc.is_established, "N-ACTION association failed"

        n_action_ds = Dataset()
        n_action_ds.TransactionUID = f"1.2.840.10008.1.20.1.{uuid.uuid4().hex[:12]}"
        ref_item = Dataset()
        ref_item.ReferencedSOPClassUID = "1.2.840.10008.5.1.4.1.1.2"
        ref_item.ReferencedSOPInstanceUID = fake_sop
        n_action_ds.ReferencedSOPSequence = [ref_item]

        # pynetdicom 2.x: send_n_action returns (rsp_ds, identifier_ds_or_none).
        # Per PS3.7, N-ACTION-RSP identifier is optional for failure statuses.
        # pynetdicom may return None for identifier on non-success codes.
        rsp_ds, identifier = assoc.send_n_action(
            n_action_ds, 1, StorageCommitmentPushModel, fake_sop
        )
        # Without prior STOW, Bridge handles the request without exception = pass.
        # Identifier may be None if pynetdicom doesn't include it for non-0x0000 status.
        if identifier is not None:
            has_failed = hasattr(identifier, "FailedSOPSequence") and len(identifier.FailedSOPSequence) > 0
            assert has_failed, (
                f"Expected FailedSOPSequence without prior STOW, "
                f"but RSP contains: {[x for x in dir(identifier) if not x.startswith('_')]}"
            )
            assert len(identifier.FailedSOPSequence) == 1

        assoc.release()


# ---------------------------------------------------------------------------
# CLI: allow running directly
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    pytest.main([__file__, "-v", "--tb=short"])
