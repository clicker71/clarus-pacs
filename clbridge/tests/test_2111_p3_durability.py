# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""P3 durability - one test per outbox/forward finding, driven on a real queue.

Every test here runs against a REAL `OutboxQueue` on a temp directory (or the
real forward batch function), because the defects are about STATE: a lost
item, a duplicated one, a count that drifts. A mocked queue would assert that
the mock was called, not that the queue is intact after the transition.
"""

import json
import os
import threading

import pytest
from clean_code.outbox import OutboxQueue
from pydicom.dataset import Dataset, FileMetaDataset
from pydicom.uid import ImplicitVRLittleEndian

CT = "1.2.840.10008.5.1.4.1.1.2"


def _ds(sop_uid, study="1.2.3", pixel_bytes=0):
    ds = Dataset()
    ds.SOPClassUID = CT
    ds.SOPInstanceUID = sop_uid
    ds.StudyInstanceUID = study
    ds.SeriesInstanceUID = "1.2.3.4"
    if pixel_bytes:
        # Enough of the image module for pydicom to resolve implicit VR for
        # PixelData; the payload is what consumes the byte budget.
        ds.Rows = 1
        ds.Columns = 1
        ds.BitsAllocated = 8
        ds.BitsStored = 8
        ds.HighBit = 7
        ds.PixelRepresentation = 0
        ds.SamplesPerPixel = 1
        ds.PhotometricInterpretation = "MONOCHROME2"
        ds.PixelData = b"\x00" * pixel_bytes
    ds.file_meta = FileMetaDataset()
    ds.file_meta.MediaStorageSOPClassUID = CT
    ds.file_meta.MediaStorageSOPInstanceUID = sop_uid
    ds.file_meta.TransferSyntaxUID = ImplicitVRLittleEndian
    return ds


@pytest.fixture
def q(tmp_path):
    return OutboxQueue(str(tmp_path), max_retries=3)


# ---------------------------------------------------------------------------
# outbox.py:297 - a modified instance is never published without its audit
# ---------------------------------------------------------------------------
def test_an_audit_sidecar_failure_blocks_publication(q, tmp_path, monkeypatch):
    """`.mods` is what makes the instance a DOCUMENTED modification. Writing
    `.meta` anyway published a blob that `dequeue()` delivers as raw bytes -
    the destination stored the new patient fields with no FR-001 record."""
    monkeypatch.setattr(q, "_write_mods_sidecar", lambda *a, **k: False)

    ok = q.enqueue(_ds("1.2.3.10"), modifications=[((0x0010, 0x0020), "OLD", "NEW")])

    assert ok is False, "publication must fail when the audit trail is missing"
    pending = os.listdir(tmp_path / "pending")
    assert not [n for n in pending if n.startswith("1.2.3.10.")], pending
    assert q.pending_count() == 0


def test_a_successful_sidecar_still_publishes(q, tmp_path):
    """Non-vacuity: with the sidecar written, the item is published whole."""
    ok = q.enqueue(_ds("1.2.3.11"), modifications=[((0x0010, 0x0020), "OLD", "NEW")])
    assert ok is True
    pending = os.listdir(tmp_path / "pending")
    assert "1.2.3.11.dcm" in pending
    assert "1.2.3.11.mods" in pending
    assert "1.2.3.11.meta" in pending


def test_an_unmodified_instance_needs_no_sidecar(q, tmp_path):
    """The sidecar stays optional for unchanged instances."""
    assert q.enqueue(_ds("1.2.3.12")) is True
    assert "1.2.3.12.mods" not in os.listdir(tmp_path / "pending")


# ---------------------------------------------------------------------------
# outbox.py:356 - the byte budget is enforced before publication
# ---------------------------------------------------------------------------
def test_the_byte_budget_refuses_before_publishing(tmp_path):
    """With 400 MiB pending and a 500 MiB budget a 150 MiB instance used to be
    admitted (550 MiB) and acknowledged 0x0000. The budget is now checked
    BEFORE a byte is written, and the refusal publishes nothing."""
    q = OutboxQueue(str(tmp_path), max_retries=3, max_queue_bytes=200 * 1024)
    baseline = 0
    for i in range(6):
        ds = _ds(f"1.2.3.{100 + i}", pixel_bytes=60 * 1024)
        if q.enqueue(ds):
            baseline += 1
    assert baseline >= 1, "some instances must fit"
    used = q.pending_bytes()
    assert used <= 200 * 1024, f"queue exceeded its byte budget: {used}"

    big = _ds("1.2.3.999", pixel_bytes=400 * 1024)
    before = set(os.listdir(tmp_path / "pending"))
    assert q.enqueue(big) is False
    assert set(os.listdir(tmp_path / "pending")) == before, "nothing was published"


def test_the_byte_reservation_is_released_on_a_failed_publish(tmp_path, monkeypatch):
    """A reservation is not a leak: the refused item must not consume budget
    forever, or the queue locks itself out after one bad instance."""
    q = OutboxQueue(str(tmp_path), max_retries=3, max_queue_bytes=1024 * 1024)
    monkeypatch.setattr(
        "clean_code.outbox.dcmwrite",
        lambda *a, **k: (_ for _ in ()).throw(ValueError("bad dataset")),
    )
    assert q.enqueue(_ds("1.2.3.20")) is False
    monkeypatch.undo()
    assert q.enqueue(_ds("1.2.3.21")) is True, "the reservation must be released"


# ---------------------------------------------------------------------------
# outbox.py:610 - claimed names are retired from the dequeue cache
# ---------------------------------------------------------------------------
def test_draining_a_backlog_does_not_reopen_claimed_metadata(tmp_path, monkeypatch):
    """Every dequeue used to restart at the head of the same cached list, so a
    backlog of n cost n^2/2 metadata opens of items already claimed."""
    q = OutboxQueue(str(tmp_path), max_retries=3)
    for i in range(25):
        assert q.enqueue(_ds(f"1.2.3.{200 + i}"))

    opens = {"n": 0}
    real_open = open

    def _counting_open(path, *a, **k):
        if str(path).endswith(".meta"):
            opens["n"] += 1
        return real_open(path, *a, **k)

    monkeypatch.setattr("builtins.open", _counting_open)
    claimed = 0
    while True:
        item = q.dequeue()
        if item[0] is None:
            break
        claimed += 1
    assert claimed == 25
    # A quadratic drain would open ~325 metadata files here; linear is ~25.
    assert opens["n"] <= 60, f"dequeue re-opened claimed metadata: {opens['n']} opens"


def test_a_retry_becomes_claimable_at_its_deadline(q, tmp_path):
    """Retirement must not drop an item that is merely NOT DUE YET: it has to
    come back once its backoff expires."""
    assert q.enqueue(_ds("1.2.3.30")) is True
    item = q.dequeue()
    assert item[0] == "1.2.3.30"
    q.mark_failed("1.2.3.30", "transient")
    # Its deadline is in the future: nothing is claimable right now.
    assert q.dequeue()[0] is None

    meta_path = q._meta_path(q._pending, "1.2.3.30")
    with open(meta_path, encoding="utf-8") as f:
        meta = json.load(f)
    meta["next_retry_at"] = 0
    with open(meta_path, "w", encoding="utf-8") as f:
        json.dump(meta, f)

    # Newly ready: the cache was dropped when the queue looked empty.
    assert q.dequeue()[0] == "1.2.3.30"


def test_a_single_item_queue_is_claimable(q):
    """Regression guard for the cache retirement: a fresh listing must never
    be truncated to nothing, or a one-item queue deadlocks."""
    assert q.enqueue(_ds("1.2.3.31")) is True
    assert q.dequeue()[0] == "1.2.3.31"


# ---------------------------------------------------------------------------
# outbox.py:756 - delivery readiness is ONE queue state
# ---------------------------------------------------------------------------
def test_delivered_is_not_confused_by_a_concurrent_park(tmp_path, monkeypatch):
    """Checking undelivered/ and the live directories in two separate reads let
    a SOP parked BETWEEN them read as absent from both - so a failed instance
    reported itself delivered."""
    q = OutboxQueue(str(tmp_path), max_retries=3)
    assert q.enqueue(_ds("1.2.3.40")) is True
    claimed = q.dequeue()
    assert claimed[0] == "1.2.3.40"

    # Park it: after this, the SOP exists ONLY in undelivered/.
    q.move_to_undelivered("1.2.3.40")
    assert q.delivered("1.2.3.40") is False


def test_delivered_is_serialised_with_the_transition_lock(tmp_path):
    """The finding's TOCTOU: reading undelivered/ and then the live directories
    as two UNGUARDED probes lets a park in between look absent from both, so a
    failed instance reports itself delivered. The guard is what makes them one
    consistent view, and this pins that the guard is actually taken."""
    q = OutboxQueue(str(tmp_path), max_retries=3)
    assert q.enqueue(_ds("1.2.3.40")) is True
    assert q.dequeue()[0] == "1.2.3.40"

    guard = q._publish_guard_for("1.2.3.40")
    assert guard.acquire(timeout=0) is True, "the per-UID guard must be free"
    observed = {}

    def _reader():
        observed["value"] = q.delivered("1.2.3.40")

    t = threading.Thread(target=_reader, daemon=True)
    t.start()
    t.join(timeout=0.2)
    assert t.is_alive(), (
        "delivered() read the queue state without taking the per-UID guard - "
        "that is exactly the unguarded two-probe read the finding describes"
    )
    guard.release()
    t.join(timeout=2)
    assert observed.get("value") is False, "a claimed item is still owned"


def test_a_parked_item_never_reads_as_delivered(tmp_path):
    """The consequence the guard prevents: an instance the archive rejected
    must not report ready for Storage Commitment."""
    q = OutboxQueue(str(tmp_path), max_retries=3)
    assert q.enqueue(_ds("1.2.3.41")) is True
    q.dequeue()
    q.move_to_undelivered("1.2.3.41")
    assert q.delivered("1.2.3.41") is False


def test_delivered_is_true_for_an_unknown_uid(q):
    """An unknown UID must still get an independent verdict from the origin."""
    assert q.delivered("9.9.9.not-here") is True


# ---------------------------------------------------------------------------
# outbox.py:885 - a blob that could not be parked keeps its audit and state
# ---------------------------------------------------------------------------
def test_a_failed_terminal_move_keeps_the_sidecars_and_the_accounting(
    q, tmp_path, monkeypatch
):
    """Moving the sidecars and decrementing the counters after a FAILED blob
    move left the blob in pending/ with no `.meta` beside it: recovery then
    adopted it as a fresh item with zero retries and no audit trail."""
    assert q.enqueue(_ds("1.2.3.50")) is True
    item = q.dequeue()
    assert item[0] == "1.2.3.50"
    count_before = q.pending_count()

    import shutil as _shutil

    def _boom(src, dst):
        raise OSError("cannot move")

    monkeypatch.setattr(_shutil, "move", _boom)
    parked = q.move_to_undelivered("1.2.3.50")
    monkeypatch.undo()

    assert parked is False
    assert q.pending_count() == count_before, "accounting must not move"
    # The item is still a LIVE queue entry, WITH its metadata.
    assert os.path.isfile(q._meta_path(q._in_progress, "1.2.3.50"))
    assert os.path.isfile(q._dcm_path(q._in_progress, "1.2.3.50"))


def test_a_successful_terminal_move_takes_the_sidecars_with_it(q, tmp_path):
    """Non-vacuity: a move that succeeds carries the audit and accounts once."""
    assert q.enqueue(_ds("1.2.3.51"), modifications=[((0x0010, 0x0020), "A", "B")])
    q.dequeue()
    assert q.move_to_undelivered("1.2.3.51") is True
    und = os.listdir(tmp_path / "undelivered")
    assert "1.2.3.51.dcm" in und
    assert "1.2.3.51.meta" in und
    assert "1.2.3.51.mods" in und
    assert q.pending_count() == 0


# ---------------------------------------------------------------------------
# outbox.py:1111 - recovery reconciles the count without losing publications
# ---------------------------------------------------------------------------
def test_recovery_counts_claims_so_owned_items_are_not_lost(tmp_path):
    """`mark_delivered` is what subtracts a claim, so recovery must COUNT
    claims: leaving them out dropped the queue count to zero while claimed
    files were still owned, which is a crash-recover loss of track."""
    q = OutboxQueue(str(tmp_path), max_retries=3)
    assert q.enqueue(_ds("1.2.3.70")) is True
    assert q.enqueue(_ds("1.2.3.71")) is True
    q.dequeue()

    q._sync_pending_count()
    # One still pending, one claimed: the queue owns two, not one.
    assert q.pending_count() == 2

    # Delivering the claimed one is what subtracts it.
    q.mark_delivered("1.2.3.70")
    assert q.pending_count() == 1


def test_recovery_counts_claimed_items(tmp_path):
    """`mark_delivered` is what subtracts a claim, so recovery must COUNT
    claims or a crash-recover cycle drops owned items to zero."""
    q = OutboxQueue(str(tmp_path), max_retries=3)
    assert q.enqueue(_ds("1.2.3.70")) is True
    assert q.enqueue(_ds("1.2.3.71")) is True
    q.dequeue()

    q._sync_pending_count()
    # One still pending, one claimed: the queue owns two, not one.
    assert q.pending_count() == 2

    # Delivering the claimed one is what subtracts it.
    q.mark_delivered("1.2.3.70")
    assert q.pending_count() == 1


# ---------------------------------------------------------------------------
# outbox.py:541 - a failed metadata publication removes the unready mirror
# ---------------------------------------------------------------------------
def test_a_failed_forward_metadata_write_rolls_the_mirror_back(q, tmp_path):
    """A blob with no `.meta` is not an item: it makes a later mirror attempt
    read as a duplicate (so the SOP is never forwarded) or lets recovery adopt
    it. The PRIMARY blob must survive untouched."""
    primary = q._dcm_path(q._pending, "1.2.3.80")
    assert q.enqueue(_ds("1.2.3.80")) is True
    assert os.path.isfile(primary)

    fwd = OutboxQueue(str(tmp_path / "forward"), max_retries=3)

    calls = {"n": 0}
    real_write = fwd._atomic_write

    def _fail_meta(path, data, binary=False):
        if str(path).endswith(".meta"):
            calls["n"] += 1
            raise OSError("disk full")
        return real_write(path, data, binary=binary)

    fwd._atomic_write = _fail_meta
    ok = fwd.enqueue_from_blob(primary, "1.2.3.80")
    assert ok is False
    assert os.listdir(tmp_path / "forward" / "pending") == [], "mirror must be gone"
    assert fwd.pending_count() == 0, "a rolled-back mirror takes no slot"
    assert os.path.isfile(primary), "the primary blob is not this queue's to remove"


def test_the_forward_slot_is_reserved_before_the_blob_is_placed(
    q, tmp_path, monkeypatch
):
    """Checking the count and incrementing it AFTER placing the blob let two
    producers both see one free slot and both publish, so the forward queue
    exceeded its cap.

    Asserted at the point that matters: while the blob is being placed, the
    slot is ALREADY accounted for. A producer that has not reserved therefore
    sees a full queue instead of a free one."""
    forwarded = OutboxQueue(str(tmp_path / "forward"), max_queue=1, max_retries=3)
    src = q._dcm_path(q._pending, "1.2.3.81")
    assert q.enqueue(_ds("1.2.3.81")) is True

    observed = {}
    real_link = forwarded._link_or_copy

    def _spy(src_path, dst_path):
        # The slot must already be counted when the blob is being placed.
        observed["count_during_placement"] = forwarded.pending_count()
        return real_link(src_path, dst_path)

    monkeypatch.setattr(forwarded, "_link_or_copy", _spy)
    assert forwarded.enqueue_from_blob(src, "1.2.3.81") is True
    assert observed["count_during_placement"] == 1, (
        "the slot was not reserved before the blob was placed - a second "
        f"producer would see a free slot (count was "
        f"{observed['count_during_placement']})"
    )
    assert forwarded.pending_count() == 1

    # And the cap therefore holds: a second publication is refused.
    assert q.enqueue(_ds("1.2.3.82")) is True
    assert (
        forwarded.enqueue_from_blob(q._dcm_path(q._pending, "1.2.3.82"), "1.2.3.82")
        == "full"
    )
    assert forwarded.pending_count() == 1
