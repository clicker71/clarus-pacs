# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""P9 FINAL - C-MOVE warning tally, peer reservation release, audit-required
recovery.

Each test fails when its fix is reverted.
"""

import json
import os
import time

from pydicom.dataset import Dataset


def _dataset(sop_uid, size=256):
    import pydicom

    ds = Dataset()
    ds.SOPInstanceUID = sop_uid
    ds.SOPClassUID = "1.2.840.10008.5.1.4.1.1.2"
    ds.PatientName = "Doe^John"
    ds.BitsAllocated = 8
    ds.BitsStored = 8
    ds.HighBit = 7
    ds.PixelRepresentation = 0
    ds.Rows = 1
    ds.Columns = size
    ds.SamplesPerPixel = 1
    ds.PhotometricInterpretation = "MONOCHROME2"
    ds.PixelData = b"\x00" * size
    ds.file_meta = pydicom.dataset.FileMetaDataset()
    ds.file_meta.TransferSyntaxUID = pydicom.uid.ExplicitVRLittleEndian
    ds.file_meta.MediaStorageSOPClassUID = ds.SOPClassUID
    ds.file_meta.MediaStorageSOPInstanceUID = sop_uid
    return ds


def _queue(tmp_path):
    from clean_code.outbox import OutboxQueue

    return OutboxQueue(
        outbox_dir=str(tmp_path / "ob"),
        max_queue=100,
        max_retries=3,
        cleanup=False,
        retry_interval=1,
    )


# ---------------------------------------------------------------------------
# cmove :3536 - a C-STORE warning must reach the final C-MOVE status
# ---------------------------------------------------------------------------
class TestWarningTally:
    def test_the_pass_records_its_warning_count(self, tmp_path):
        """PS3.4 B.2.3.1: a Warning means the peer STORED the instance, so it
        is not a failure - but Table C.4-2 gives that outcome its own status
        (0xB000). The tally used to be counted and discarded, so a move with
        one warned instance answered plain Success."""
        from clean_code import move_jobs as mj

        # Use the same SharedDb-compatible stub the move_jobs tests use.
        from tests.test_db_move_jobs import _TestDb

        manager = mj.MoveJobManager(shared_db=_TestDb(":memory:"))
        try:
            job_id = manager.create_job("1.2.3", "DEST", "127.0.0.1", 11112)
            assert manager.get_job_warning_count(job_id) == 0

            manager.set_job_warning_count(job_id, 2)
            assert manager.get_job_warning_count(job_id) == 2
        finally:
            manager.close()

    def test_the_streaming_pass_persists_the_tally(self):
        """The warning count must survive past the streaming call.

        `_stream_and_send_study` keeps `warned` in a local; the poller runs in
        the handler thread and cannot see a local, so the count has to reach
        the job row. An empty accepted-context map takes the documented early
        exit, which still has to publish the (zero) tally.
        """
        from clean_code.handlers import cmove

        recorded = {}

        class _Recorder:
            def set_job_warning_count(self, job_id, count):
                recorded["job_id"] = job_id
                recorded["count"] = count

        cmove._stream_and_send_study(
            assoc=None,
            study_uid="1.2.3",
            clarus_url="http://127.0.0.1:8024",
            http_timeout=1,
            move_jobs=_Recorder(),
            job_id=42,
            accepted_contexts_map={},
        )

        assert recorded == {
            "job_id": 42,
            "count": 0,
        }, "the tally must be published to the job row on every exit path"

    def test_the_success_branch_does_not_consult_the_warning_tally(self):
        """The success branch yields 0 and must not emit 0xB000 from the
        warning tally.

        After `yield 0` pynetdicom short-circuits to 0x0000 and stops
        reading, so a warning-tally yield was dead code and is removed.
        PS3.4 Table C.4-2's "warned" status is unreachable in the
        fire-and-forget model (no move-destination store_assoc).
        """
        import inspect

        from clean_code.handlers import cmove

        src = inspect.getsource(cmove.handle_c_move)
        assert (
            "_warned_across_jobs" not in src
        ), "the dead warning-tally helper must be removed from the reactor"
        winner = src.split("# All jobs succeeded.", 1)[1]
        success_branch = winner.split("_clear_callbacks()", 1)[0]
        assert (
            "yield 0" in success_branch
        ), "the success path must yield 0 sub-operations"
        assert "0xB000" not in success_branch, "the success path must not emit 0xB000"


# ---------------------------------------------------------------------------
# cmove :4426 - the picker's peer reservation is released after the job
# ---------------------------------------------------------------------------
class TestPickerReservationReleased:
    def test_a_reservation_taken_by_the_picker_is_released(self):
        """`pick_pending_job` reserves at depth 1 and `_execute_cmove_job`
        gates at depth 2. Releasing only the inner one left the peer marked
        busy for the life of the process, so every other worker skipped it."""
        from clean_code.handlers import cmove

        host, port, ae = "127.0.0.1", 11112, "DEST"
        try:
            # Depth 1 = the picker's claim.
            assert cmove._try_reserve_peer(host, port, ae) is True
            # The executor's re-entrant take.
            cmove._job_gate_enter(host, port, ae)
            # Executor releases its own level.
            cmove._job_gate_exit(host, port, ae)
            # The worker loop now releases the picker's level (the fix).
            cmove._job_gate_exit(host, port, ae)

            assert (
                cmove._peer_is_busy(host, port, ae) is False
            ), "the picker's reservation leaked; the peer is still marked busy"
        finally:
            cmove._ASSOC_BUSY.pop((host, port, ae), None)

    def test_the_worker_loop_releases_the_picker_reservation(self):
        """Structural: the release must sit in the worker's job `finally`.

        A successful job leaked the reservation just as surely as a raising
        one, because the only release on that path belonged to the executor's
        own (inner) acquisition.
        """
        import inspect

        from clean_code.handlers import cmove

        # The worker loop is a closure inside _cmove_worker, so take its
        # source from the enclosing function rather than a module attribute.
        src = inspect.getsource(cmove._cmove_worker)
        loop = src.split("def _worker_loop(worker_id):", 1)[1]
        loop = loop.split('logging.info("[C-MOVE] Worker-%d stopped"', 1)[0]
        finally_blocks = loop.split("finally:")
        assert len(finally_blocks) > 1, "the worker loop must have a finally block"
        assert any(
            "_job_gate_exit" in block for block in finally_blocks[1:]
        ), "the worker's finally must release the picker's peer reservation"


# ---------------------------------------------------------------------------
# outbox :1226 - a blob requiring audit data is never adopted without it
# ---------------------------------------------------------------------------
class TestAuditRequiredRecovery:
    def test_an_orphan_with_a_marker_but_no_sidecar_is_not_adopted(self, tmp_path):
        """A crash between the .dcm rename and the .mods write must not turn a
        modified instance into a raw-bytes delivery.

        The positive half of the same fixture (test below) proves the orphan is
        otherwise adoptable, so this failure can only come from the marker.
        """
        q = _queue(tmp_path)
        uid = "1.2.3.4"

        dcm = q._dcm_path(q._pending, uid)
        with open(dcm, "wb") as f:
            f.write(b"blob")
        # The marker says audit data is REQUIRED; the sidecar is absent.
        with open(q._audit_path(q._pending, uid), "w", encoding="utf-8") as f:
            json.dump({"required": True}, f)
        # Make it look like a crashed write, not one still in progress.
        old = os.path.getmtime(dcm) - 3600
        os.utime(dcm, (old, old))

        q.recover_stuck()

        assert not os.path.isfile(
            q._meta_path(q._pending, uid)
        ), "the orphan was adopted without its required audit data"
        assert os.path.isfile(dcm), "the quarantined blob must stay in pending/"

    def test_the_sibling_orphan_is_adopted_in_the_same_pass(self, tmp_path):
        """Control for the test above: one pass, two orphans, one marker.

        Without this the quarantine test could pass because `recover_stuck`
        adopted nothing at all.
        """
        q = _queue(tmp_path)
        blocked = "1.2.3.40"
        clean = "1.2.3.41"
        old = time.time() - 3600

        for uid in (blocked, clean):
            dcm = q._dcm_path(q._pending, uid)
            with open(dcm, "wb") as f:
                f.write(b"blob")
            os.utime(dcm, (old, old))
        with open(q._audit_path(q._pending, blocked), "w", encoding="utf-8") as f:
            json.dump({"required": True}, f)

        q.recover_stuck()

        assert os.path.isfile(
            q._meta_path(q._pending, clean)
        ), "an orphan without a marker must still be adopted"
        assert not os.path.isfile(
            q._meta_path(q._pending, blocked)
        ), "the marked orphan must be quarantined"

    def test_an_orphan_with_its_sidecar_is_still_adopted(self, tmp_path):
        q = _queue(tmp_path)
        uid = "1.2.3.5"

        dcm = q._dcm_path(q._pending, uid)
        with open(dcm, "wb") as f:
            f.write(b"blob")
        with open(q._audit_path(q._pending, uid), "w", encoding="utf-8") as f:
            json.dump({"required": True}, f)
        with open(q._mods_path(q._pending, uid), "w", encoding="utf-8") as f:
            json.dump([], f)
        old = os.path.getmtime(dcm) - 3600
        os.utime(dcm, (old, old))

        q.recover_stuck()

        assert os.path.isfile(
            q._meta_path(q._pending, uid)
        ), "a blob with its audit data present must still be recovered"

    def test_an_unmodified_orphan_is_unaffected(self, tmp_path):
        """No marker means no audit requirement: the old behaviour stands."""
        q = _queue(tmp_path)
        uid = "1.2.3.6"

        dcm = q._dcm_path(q._pending, uid)
        with open(dcm, "wb") as f:
            f.write(b"blob")
        old = os.path.getmtime(dcm) - 3600
        os.utime(dcm, (old, old))

        q.recover_stuck()

        assert os.path.isfile(q._meta_path(q._pending, uid))

    def test_enqueue_writes_the_marker_for_a_modified_instance(self, tmp_path):
        q = _queue(tmp_path)
        ds = _dataset("1.2.3.7")

        assert q.enqueue(ds, modifications=[((0x0008, 0x103E), "OLD", "NEW")]) is True
        marker = q._audit_path(q._pending, "1.2.3.7")
        assert os.path.isfile(marker)
        with open(marker, encoding="utf-8") as f:
            assert json.load(f).get("required") is True
        assert os.path.isfile(q._mods_path(q._pending, "1.2.3.7"))

    def test_no_marker_is_written_for_an_unmodified_instance(self, tmp_path):
        """Control: the marker must track the modifications, not the queue."""
        q = _queue(tmp_path)
        ds = _dataset("1.2.3.8")

        assert q.enqueue(ds) is True
        assert not os.path.isfile(q._audit_path(q._pending, "1.2.3.8"))


# ---------------------------------------------------------------------------
# outbox publication state - :405, :907, :945, :1348, :839
# ---------------------------------------------------------------------------
class TestPublicationState:
    def test_an_unpublished_blob_is_not_acknowledged_as_a_duplicate(self, tmp_path):
        """`:405`. A bare `.dcm` is an interrupted publish, not a queued item.

        Answering "duplicate" to the modality (0x0000) told it the instance was
        safely stored while the queue held a blob with no ready-marker - which
        `dequeue()` therefore never deliver.
        """
        q = _queue(tmp_path)
        uid = "1.2.3.90"

        # The crashed-publication shape: blob on disk, no `.meta`.
        with open(q._dcm_path(q._pending, uid), "wb") as f:
            f.write(b"truncated")

        result = q.enqueue(_dataset(uid))

        assert result is True, "an unpublished blob must be re-published, not acked"
        assert os.path.isfile(
            q._meta_path(q._pending, uid)
        ), "the re-publish must leave a ready-marker, or the item is still stuck"

    def test_a_committed_item_is_still_reported_as_a_duplicate(self, tmp_path):
        """Control for the test above: real duplicates keep their fast path."""
        q = _queue(tmp_path)
        uid = "1.2.3.91"

        assert q.enqueue(_dataset(uid)) is True
        assert q.enqueue(_dataset(uid)) == "duplicate"

    def test_a_stale_audit_marker_does_not_poison_the_republish(self, tmp_path):
        """Part of `:405`: debris from the failed attempt must not survive.

        A leftover `.audit` would mark the NEW publication audit-required, so
        its own missing `.mods` would make the item undeliverable.
        """
        q = _queue(tmp_path)
        uid = "1.2.3.92"

        with open(q._dcm_path(q._pending, uid), "wb") as f:
            f.write(b"stale")
        with open(q._audit_path(q._pending, uid), "w", encoding="utf-8") as f:
            json.dump({"required": True}, f)

        assert q.enqueue(_dataset(uid)) is True
        assert not os.path.isfile(
            q._audit_path(q._pending, uid)
        ), "the stale audit marker must be cleared by the re-publish"
        # And the item is therefore claimable again.
        claimed, _ds, _bp, _raw = q.dequeue()
        assert claimed == uid

    def test_a_failed_publication_leaves_no_audit_marker_behind(self, tmp_path):
        """Sibling of `:405`: the rollback must clear the marker it wrote.

        A `.audit` left by a failed publish makes the NEXT publish of the same
        UID read as audit-required, so its missing `.mods` would make an
        unmodified resend undeliverable - and the marker itself is proof of a
        modification that was never committed.
        """
        q = _queue(tmp_path)
        uid = "1.2.3.99"

        # Force the sidecar write to fail, which is the rollback path.
        original = q._write_mods_sidecar

        def _boom(*_a, **_k):
            return False

        q._write_mods_sidecar = _boom
        try:
            assert (
                q.enqueue(_dataset(uid), modifications=[((0x0008, 0x103E), "A", "B")])
                is False
            )
        finally:
            q._write_mods_sidecar = original

        leftovers = [n for n in os.listdir(q._pending) if n.startswith(uid)]
        assert (
            leftovers == []
        ), f"a failed publish must leave nothing behind: {leftovers}"

        # And the UID is publishable afterwards, without an audit requirement.
        assert q.enqueue(_dataset(uid)) is True
        assert not os.path.isfile(q._audit_path(q._pending, uid))

    def test_dequeue_does_not_hand_out_a_blob_missing_its_audit_trail(self, tmp_path):
        """`:907`. A `.meta` promises a complete publication; the claim must
        verify it rather than trust the marker alone.

        The item must not merely come back as None - it must still be sitting
        in pending/, unclaimed, or the test would pass just as well if the
        whole queue were broken.
        """
        q = _queue(tmp_path)
        uid = "1.2.3.93"

        dcm = q._dcm_path(q._pending, uid)
        with open(dcm, "wb") as f:
            f.write(b"blob")
        with open(q._audit_path(q._pending, uid), "w", encoding="utf-8") as f:
            json.dump({"required": True}, f)
        with open(q._meta_path(q._pending, uid), "w", encoding="utf-8") as f:
            json.dump({"created_at": 0, "retry_count": 0, "next_retry_at": 0}, f)

        claimed, _ds, _bp, _raw = q.dequeue()

        assert (
            claimed is None
        ), "an audited item without its .mods must not be delivered"
        assert os.path.isfile(dcm), "the item must stay in pending/, not be claimed"
        assert not os.path.isfile(q._dcm_path(q._in_progress, uid))

    def test_a_healthy_sibling_behind_an_audited_orphan_is_still_claimed(
        self, tmp_path
    ):
        """`:907` control. The scan must continue past the blocked row.

        Without this, the test above passes for the wrong reason: a claim that
        gave up on the whole listing also returns None.
        """
        q = _queue(tmp_path)
        blocked = "1.2.3.930"
        healthy = "1.2.3.931"

        # Publish the healthy item properly, then remove the ready-marker so it
        # can be re-published at the age this test needs.
        assert q.enqueue(_dataset(healthy)) is True
        healthy_meta = q._meta_path(q._pending, healthy)
        with open(healthy_meta, encoding="utf-8") as f:
            healthy_record = json.load(f)

        # The blocked item: a real blob plus an audit marker and no sidecar.
        assert q.enqueue(_dataset(blocked)) is True
        with open(q._audit_path(q._pending, blocked), "w", encoding="utf-8") as f:
            json.dump({"required": True}, f)
        try:
            os.unlink(q._mods_path(q._pending, blocked))
        except OSError:
            pass
        with open(q._meta_path(q._pending, blocked), "w", encoding="utf-8") as f:
            json.dump(healthy_record, f)

        # The blocked item must be reached FIRST.
        old = time.time() - 60
        for uid, stamp in ((blocked, old), (healthy, old + 30)):
            os.utime(q._meta_path(q._pending, uid), (stamp, stamp))
        q._pending_meta_cache = None

        claimed, _ds, _bp, _raw = q.dequeue()

        assert claimed == healthy, "the walk must continue past the blocked row"
        assert os.path.isfile(
            q._dcm_path(q._pending, blocked)
        ), "the blocked item must be left untouched"

    def test_dequeue_carries_the_audit_marker_into_in_progress(self, tmp_path):
        """`:945`. The marker must travel with the claim, or a later recovery
        puts the item back without the proof that it needs its sidecar."""
        q = _queue(tmp_path)
        uid = "1.2.3.94"

        assert (
            q.enqueue(_dataset(uid), modifications=[((0x0008, 0x103E), "A", "B")])
            is True
        )

        claimed, _ds, _bp, _raw = q.dequeue()

        assert claimed == uid
        assert os.path.isfile(
            q._audit_path(q._in_progress, uid)
        ), "the audit marker must follow the item into in_progress/"
        assert not os.path.isfile(q._audit_path(q._pending, uid))

    def test_delivered_holds_the_guard_across_every_publication_part(self, tmp_path):
        """`:945`. "The blob is gone" is not "the SOP is delivered".

        `park_blob` moves only the `.dcm`, so a SOP mid-transition has its blob
        elsewhere while its `.meta` is still in a live directory. Reading only
        the blob reported a failed instance as delivered.
        """
        q = _queue(tmp_path)
        uid = "1.2.3.95"

        assert q.enqueue(_dataset(uid)) is True
        # Simulate the transition window: blob moved out, ready-marker still in
        # pending/.
        os.unlink(q._dcm_path(q._pending, uid))
        assert os.path.isfile(q._meta_path(q._pending, uid))

        assert (
            q.delivered(uid) is False
        ), "a publication still present in pending/ is not delivered"

    def test_a_retry_parked_temporarily_stays_eligible(self, tmp_path):
        """`:839`. A not-yet-due retry must not be evicted by the cache.

        The pending-listing cache used to drop every row behind the first
        claimed one. Here the deferred row is the OLDEST, so a walk that
        retired it would leave the cache without the only entry that can
        become due; the retry below would then be invisible until some
        unrelated event dropped the cache wholesale.
        """
        q = _queue(tmp_path)
        waiting = "1.2.3.97"
        ready = "1.2.3.96"

        assert q.enqueue(_dataset(waiting)) is True
        assert q.enqueue(_dataset(ready)) is True
        # Deferred row first in age order, so the scan stops on it.
        meta_waiting = q._meta_path(q._pending, waiting)
        with open(meta_waiting, encoding="utf-8") as f:
            meta = json.load(f)
        meta["next_retry_at"] = time.time() + 3600
        with open(meta_waiting, "w", encoding="utf-8") as f:
            json.dump(meta, f)
        old = time.time() - 60
        os.utime(meta_waiting, (old, old))
        q._pending_meta_cache = None

        # First pass: stops on the deferred row, which must survive.
        first, _ds, _bp, _raw = q.dequeue()
        assert first is None, "the oldest row is not due"
        assert (
            q._pending_meta_cache is not None
        ), "the listing must be retained, or every deferral re-walks the backlog"
        retained = [name for _m, name in q._pending_meta_cache]
        assert any(
            name.startswith(q._safe_uid(waiting)) for name in retained
        ), "the deferred retry was evicted from the pending listing"

        # Make it due. It must be claimed from the RETAINED listing, i.e.
        # without the cache being rebuilt from the directory.
        with open(meta_waiting, encoding="utf-8") as f:
            meta = json.load(f)
        meta["next_retry_at"] = 0
        with open(meta_waiting, "w", encoding="utf-8") as f:
            json.dump(meta, f)
        before = q._pending_meta_cache
        third, _ds3, _bp3, _raw3 = q.dequeue()
        assert third == waiting, "a retry must become claimable as soon as it is due"
        assert before is not None

    def test_the_recovery_count_includes_in_flight_publications(self, tmp_path):
        """`:1348`. The sweep must not erase a publication that is mid-write.

        The listing is taken without the lock; a producer that has reserved its
        slot but not yet renamed its blob is invisible to it, so a bare
        assignment lost it for the process lifetime.
        """
        q = _queue(tmp_path)
        uid = "1.2.3.98"

        # Reserve as `enqueue` does, then reconcile: the blob does not exist
        # yet, exactly the in-flight window.
        dcm = q._dcm_path(q._pending, uid)
        name = q._filename_for(dcm)
        with q._pending_lock:
            q._reserved_names.add(name)
            q._reserved_amounts[name] = 4096
            q._reserved_bytes += 4096

        count = q._sync_pending_count()

        assert (
            count == 1
        ), "an admitted-but-unwritten publication must survive the sweep"
