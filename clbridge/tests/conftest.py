# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Shared test fixtures for Bridge handler tests — P31.

Provides:
- Shutdown event mocks (mock_shutdown_not_set, mock_shutdown_set)
- STOW mocks (mock_stow_success, mock_stow_failure)
- make_fake_cstore_event(): builds FakeEvent with real pydicom Dataset
- make_fake_move_event(): builds FakeEvent for C-MOVE handler tests
- make_fake_find_event(): builds FakeEvent for C-FIND handler tests
- make_fake_naction_event(): builds FakeEvent for N-ACTION handler tests
"""

from unittest.mock import MagicMock, patch

import pytest
from clean_code.protocol import (
    PATIENT_ROOT_UID,
)
from pydicom import Dataset

# ---------------------------------------------------------------------------
# Shutdown event mocks
# ---------------------------------------------------------------------------
# _shutdown_requested is imported by each handler module with
#   from clean_code.utils import _shutdown_requested
# so each handler has its own reference.  We must patch ALL of them.
_SHUTDOWN_TARGETS = [
    "clean_code.utils._shutdown_requested",
    "clean_code.handlers.cstore._shutdown_requested",
    "clean_code.handlers.cmove._shutdown_requested",
    "clean_code.handlers.cfind._shutdown_requested",
    "clean_code.handlers.naction._shutdown_requested",
]


def _patch_shutdown(is_set_value):
    """Context manager: patch _shutdown_requested in all handler modules."""
    mock_ev = MagicMock()
    mock_ev.is_set.return_value = is_set_value
    patchers = [patch(target, mock_ev) for target in _SHUTDOWN_TARGETS]
    for p in patchers:
        p.start()
    try:
        yield mock_ev
    finally:
        for p in patchers:
            p.stop()


@pytest.fixture
def mock_shutdown_not_set():
    """Ensure _shutdown_requested is NOT set during test."""
    yield from _patch_shutdown(False)


@pytest.fixture
def mock_shutdown_set():
    """Simulate shutdown in progress."""
    yield from _patch_shutdown(True)


# ---------------------------------------------------------------------------
# STOW mocks
# ---------------------------------------------------------------------------


@pytest.fixture
def mock_stow_success():
    """Mock stow_single_instance returns success (0x0000)."""
    with patch("clean_code.handlers.cstore.stow_single_instance") as mock_stow:
        mock_stow.return_value = (0x0000, 200, "OK")
        yield mock_stow


@pytest.fixture
def mock_stow_failure():
    """Mock stow_single_instance returns failure (0xC000)."""
    with patch("clean_code.handlers.cstore.stow_single_instance") as mock_stow:
        mock_stow.return_value = (0xC000, 500, "Internal Server Error")
        yield mock_stow


# ---------------------------------------------------------------------------
# Fake event builders
# ---------------------------------------------------------------------------


def make_fake_cstore_event(
    sop_class_uid="1.2.840.10008.5.1.4.1.1.2",
    sop_instance_uid="1.2.3.4.5",
    study_uid="1.2.3",
    series_uid="1.2.3.4",
    patient_name="Test^Patient",
):
    """Build a fake pynetdicom C-STORE event with real pydicom Dataset."""
    from pydicom.uid import ImplicitVRLittleEndian

    ds = Dataset()
    ds.SOPClassUID = sop_class_uid
    ds.SOPInstanceUID = sop_instance_uid
    ds.StudyInstanceUID = study_uid
    ds.SeriesInstanceUID = series_uid
    ds.PatientName = patient_name
    ds.SpecificCharacterSet = "ISO_IR 192"
    # Set up file_meta for dcmwrite compatibility
    ds.file_meta = Dataset()
    ds.file_meta.MediaStorageSOPClassUID = sop_class_uid
    ds.file_meta.MediaStorageSOPInstanceUID = sop_instance_uid
    ds.file_meta.TransferSyntaxUID = ImplicitVRLittleEndian

    class FakeContext:
        abstract_syntax = sop_class_uid

    class FakeRequestor:
        ae_title = "TEST_SCU"
        address = "127.0.0.1"
        port = 11112

    class FakeAssoc:
        requestor = FakeRequestor()

    class FakeEvent:
        def __init__(self):
            self.dataset = ds
            self.context = FakeContext()
            self.assoc = FakeAssoc()
            # _measure_dataset_bytes accesses event.request.DataSet
            self.request = MagicMock()
            self.request.DataSet = None  # force slow-path measurement

    return FakeEvent()


def make_fake_move_event(
    study_uid="1.2.3.4.5",
    move_dest="DEST_AE",
    calling_ae="TEST_SCU",
    calling_ip="127.0.0.1",
):
    """Build a fake pynetdicom C-MOVE event with real pydicom Dataset.

    MoveDestination is set on the Dataset (as _parse_c_move_rq expects).
    """
    ds = Dataset()
    ds.StudyInstanceUID = study_uid
    if move_dest:
        ds.MoveDestination = move_dest

    class FakeRequestor:
        ae_title = calling_ae
        address = calling_ip
        port = 11112

    class FakeAssoc:
        requestor = FakeRequestor()

    class FakeContext:
        abstract_syntax = "1.2.840.10008.5.1.4.1.2.2.2"  # Study Root C-MOVE

    class FakeEvent:
        def __init__(self):
            self.identifier = ds
            self.assoc = FakeAssoc()
            self.context = FakeContext()
            self.is_cancelled = lambda: False
            # pynetdicom provides move_destination as a direct event attribute
            self.move_destination = move_dest

    return FakeEvent()


def make_fake_find_event(
    model_uid=PATIENT_ROOT_UID, patient_name="Test*", calling_ae="TEST_SCU"
):
    """Build a fake pynetdicom C-FIND event with real pydicom Dataset."""
    ds = Dataset()
    ds.PatientName = patient_name

    class FakeContext:
        abstract_syntax = model_uid

    class FakeRequestor:
        ae_title = calling_ae

    class FakeAssoc:
        requestor = FakeRequestor()

    class FakeEvent:
        def __init__(self):
            self.identifier = ds
            self.context = FakeContext()
            self.assoc = FakeAssoc()

    return FakeEvent()


def make_fake_naction_event(
    tx_uid="1.2.3.4.5", sop_list=None, action_type=1, calling_ae="TEST_SCU"
):
    """Build a fake pynetdicom N-ACTION event with ReferencedSOPSequence.

    Args:
        tx_uid: TransactionUID for the N-ACTION request
        sop_list: list of (sop_class_uid, sop_instance_uid) tuples
        action_type: action_type_id (1 = Storage Commitment Request)
        calling_ae: AE title of the calling SCU
    """
    ds = Dataset()
    ds.TransactionUID = tx_uid
    if sop_list:
        ref_seq = []
        for sop_class, sop_uid in sop_list:
            item = Dataset()
            item.ReferencedSOPClassUID = sop_class
            item.ReferencedSOPInstanceUID = sop_uid
            ref_seq.append(item)
        ds.ReferencedSOPSequence = ref_seq

    class FakeRequestor:
        # A real association always carries the peer's socket address, and the
        # N-ACTION handler needs it: the report destination is resolved FROM
        # that address through the peer config (the ephemeral source port is
        # never used). Without it every request looks unresolvable.
        ae_title = calling_ae
        address = "10.0.0.5"
        port = 11112

    class FakeAssoc:
        requestor = FakeRequestor()

    class FakeEvent:
        def __init__(self):
            self.dataset = ds
            self.assoc = FakeAssoc()
            self.action_type = action_type

    return FakeEvent()
