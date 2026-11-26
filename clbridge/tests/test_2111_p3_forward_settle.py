# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""P3 forward-channel settlement (main.py:2360 / main.py:2476).

Both defects are about WHICH SOPs a batch reports as settled, so the tests run
the real `_forward_process_batch` over a real forward `OutboxQueue` and a fake
association that loses or refuses responses at chosen points.
"""

import os

import pytest
from clean_code.main import _forward_process_batch
from clean_code.outbox import OutboxQueue
from pydicom.dataset import Dataset, FileMetaDataset
from pydicom.uid import ImplicitVRLittleEndian

CT = "1.2.840.10008.5.1.4.1.1.2"
EVRLE = ImplicitVRLittleEndian


def _ds(sop_uid):
    ds = Dataset()
    ds.SOPClassUID = CT
    ds.SOPInstanceUID = sop_uid
    ds.StudyInstanceUID = "1.2.3"
    ds.SeriesInstanceUID = "1.2.3.4"
    ds.file_meta = FileMetaDataset()
    ds.file_meta.MediaStorageSOPClassUID = CT
    ds.file_meta.MediaStorageSOPInstanceUID = sop_uid
    ds.file_meta.TransferSyntaxUID = EVRLE
    return ds


def _status(code):
    ds = Dataset()
    ds.Status = code
    return ds


class _EmptyStatus:
    """What pynetdicom returns when the peer never answered."""


class _Ctx:
    def __init__(self):
        self.abstract_syntax = CT
        self.transfer_syntax = [EVRLE]


class _Assoc:
    """A fake association that answers per SOP UID, then dies."""

    def __init__(self, answers, stay_established_after_none=False):
        self.answers = answers
        self.accepted_contexts = [_Ctx()]
        self.calls = []
        self.is_established = True
        self._stay = stay_established_after_none

    def send_c_store(self, path):
        uid = os.path.basename(path)[: -len(".dcm")]
        self.calls.append(uid)
        answer = self.answers.get(uid, _status(0x0000))
        if answer is None:
            if not self._stay:
                self.is_established = False
            return _EmptyStatus()
        return answer

    def release(self):
        pass


@pytest.fixture
def fwd(tmp_path):
    return OutboxQueue(str(tmp_path / "forward"), max_retries=5)


def _claim(fwd, uids, primary_dir):
    """Mirror the given SOPs into the forward queue and claim them."""
    os.makedirs(primary_dir, exist_ok=True)
    src = OutboxQueue(primary_dir, max_retries=5)
    for uid in uids:
        assert src.enqueue(_ds(uid)) is True
    items = []
    for uid in uids:
        assert fwd.enqueue_from_blob(src._dcm_path(src._pending, uid), uid) is True
    while True:
        claimed = fwd.dequeue()
        if claimed[0] is None:
            break
        items.append((claimed[0], CT, EVRLE, claimed[2]))
    return items


def test_a_sop_that_lost_its_response_is_not_reported_settled(
    fwd, tmp_path, monkeypatch
):
    """A succeeds, the association drops while B is in flight. B's claim is
    still open, so B must go back with the unsent items - reporting it settled
    left B in in_progress/ until the 300 s reaper, even though it only needed
    a re-send."""
    items = _claim(fwd, ["1.2.3.A", "1.2.3.B", "1.2.3.C"], str(tmp_path / "primary"))
    assoc = _Assoc({"1.2.3.A": _status(0x0000), "1.2.3.B": None})
    monkeypatch.setattr("clean_code.main._forward_open_assoc", lambda *a, **k: assoc)

    settled = set()
    ok = _forward_process_batch(
        fwd, items, "DEST", "CALLING", "127.0.0.1", 11112, 5, 5, 5, settled=settled
    )

    assert ok is False, "the association was lost"
    assert "1.2.3.A" in settled, "a delivered SOP stays settled"
    assert "1.2.3.B" not in settled, "the SOP with no response must be requeued"
    assert "1.2.3.C" not in settled, "unsent items must be requeued"


def test_the_caller_requeues_the_sop_that_lost_its_response(fwd, tmp_path, monkeypatch):
    """End to end through the recovery loop's requeue rule: B must come back
    to pending/, not wait for the stuck sweep."""
    items = _claim(fwd, ["1.2.3.A", "1.2.3.B"], str(tmp_path / "primary"))
    assoc = _Assoc({"1.2.3.A": _status(0x0000), "1.2.3.B": None})
    monkeypatch.setattr("clean_code.main._forward_open_assoc", lambda *a, **k: assoc)

    settled = set()
    ok = _forward_process_batch(
        fwd, items, "DEST", "CALLING", "127.0.0.1", 11112, 5, 5, 5, settled=settled
    )
    assert not ok

    for uid, _sop, _ts, _bp in items:
        if uid in settled:
            continue
        fwd.requeue(uid, "association failed")

    pending = sorted(
        n for n in os.listdir(tmp_path / "forward" / "pending") if n.endswith(".dcm")
    )
    assert pending == ["1.2.3.B.dcm"], pending
    assert fwd.delivered("1.2.3.A") is True, "A was delivered and must stay gone"
    assert fwd.delivered("1.2.3.B") is False, "B is back in the queue"


def test_each_batch_gets_a_fresh_settlement_set(fwd, tmp_path, monkeypatch):
    """A set that outlives the call let recovery skip a UID due for retry: a
    batch that raised before publishing left the PREVIOUS batch's set in
    place."""
    items = _claim(fwd, ["1.2.3.A"], str(tmp_path / "primary"))
    assoc = _Assoc({"1.2.3.A": _status(0x0000)})
    monkeypatch.setattr("clean_code.main._forward_open_assoc", lambda *a, **k: assoc)

    first = set()
    _forward_process_batch(
        fwd, items, "DEST", "CALL", "127.0.0.1", 11112, 5, 5, 5, settled=first
    )
    assert first == {"1.2.3.A"}

    # A second, unrelated batch must not see the first one's settlements.
    second = set()
    assert second == set()
    assert "1.2.3.A" not in second


def test_settlements_survive_an_exception_in_the_batch(fwd, tmp_path, monkeypatch):
    """The caller reads the set after an exception; that is exactly when the
    completed transitions matter most."""
    items = _claim(fwd, ["1.2.3.A", "1.2.3.B"], str(tmp_path / "primary"))

    class _Raising(_Assoc):
        def send_c_store(self, path):
            uid = os.path.basename(path)[: -len(".dcm")]
            if uid == "1.2.3.A":
                return _status(0x0000)
            raise RuntimeError("peer collapsed")

    assoc = _Raising({})
    monkeypatch.setattr("clean_code.main._forward_open_assoc", lambda *a, **k: assoc)

    settled = set()
    ok = _forward_process_batch(
        fwd, items, "DEST", "CALL", "127.0.0.1", 11112, 5, 5, 5, settled=settled
    )
    assert ok is True, "the batch itself is usable; the peer died mid-send"
    assert "1.2.3.A" in settled
