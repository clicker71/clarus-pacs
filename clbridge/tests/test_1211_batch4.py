# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false

"""1211 convergence batch 4 regression tests, one per finding.

Each test fails when its fix is reverted: the assertion targets the behaviour
the finding described, not the shape of the code.
"""

import threading

import pytest


# ---------------------------------------------------------------------------
# R1 - no lock-order inversion between _cond and the database lock
# ---------------------------------------------------------------------------
class TestLockOrder:
    def test_create_job_notifies_outside_the_db_lock(self):
        """Batch-3's B7 made `pick_pending_job` take _cond -> _db. Notifying
        from inside `_db.locked()` made `create_job` take _db -> _cond, the
        opposite order: two threads could deadlock the whole C-MOVE subsystem,
        including the metrics scrape."""
        import inspect

        import clean_code.move_jobs as mj

        src = inspect.getsource(mj.MoveJobManager.create_job)
        code = "\n".join(line.split("#", 1)[0] for line in src.splitlines())
        # The notify must NOT be inside a `with self._db.locked()` block.
        depth_in_db = 0
        notify_inside_db = False
        for line in code.splitlines():
            stripped = line.strip()
            if stripped.startswith("with self._db.locked()"):
                depth_in_db += 1
            elif stripped.startswith("with self._cond"):
                depth_in_db -= 1
            if "_cond.notify" in stripped and depth_in_db > 0:
                notify_inside_db = True
        assert not notify_inside_db, (
            "create_job notifies while holding the db lock: _db -> _cond "
            "inverts pick_pending_job's _cond -> _db"
        )

    def test_pick_pending_job_and_create_job_do_not_deadlock(self, tmp_path):
        """Two threads, the two opposite paths, many rounds: a lock-order
        inversion hangs the workers."""
        import clean_code.move_jobs as mj

        from tests.test_0911_rabbit_batch1 import _TestDb

        db = _TestDb(str(tmp_path / "jobs.db"))
        mgr = mj.MoveJobManager(shared_db=db, max_workers=2)
        stop = threading.Event()
        errors = []

        def _worker():
            try:
                while not stop.is_set():
                    mgr.pick_pending_job(timeout=0.01)
            except Exception as e:  # pragma: no cover - diagnostics only
                errors.append(e)

        def _reactor():
            try:
                for i in range(60):
                    mgr.create_job(f"1.2.3.{i}", "PEER", "10.0.0.5", 11112)
            except Exception as e:  # pragma: no cover - diagnostics only
                errors.append(e)

        workers = [threading.Thread(target=_worker, daemon=True) for _ in range(2)]
        for w in workers:
            w.start()
        reactor = threading.Thread(target=_reactor, daemon=True)
        reactor.start()

        reactor.join(timeout=15.0)
        stop.set()
        for w in workers:
            w.join(timeout=5.0)

        assert not reactor.is_alive(), "create_job blocked (lock inversion)"
        assert all(
            not w.is_alive() for w in workers
        ), "pick_pending_job blocked (lock inversion)"
        assert not errors, errors
        db.close()


# ---------------------------------------------------------------------------
# R2 - the charset undo restores each element's OWN original
# ---------------------------------------------------------------------------
def test_two_sequence_items_with_the_same_tag_each_keep_their_own_text():
    """B10 keyed the undo map by tag and kept only the first original, so the
    revert wrote item 1's text into item 2 - silently swapped clinical text."""
    from clean_code.handlers import cstore as cs
    from pydicom.dataset import Dataset
    from pydicom.sequence import Sequence

    ds = Dataset()
    ds.SpecificCharacterSet = "ISO_IR 100"

    first_text = "Òåñò îäèí"
    second_text = "Äðóãîé òåêñò"

    item1 = Dataset()
    item1.TextValue = first_text
    item2 = Dataset()
    item2.TextValue = second_text
    # (0040,A730) Content Sequence - both items carry (0040,A160) TextValue.
    ds.ContentSequence = Sequence([item1, item2])

    # An element the repair cannot examine, to force the revert branch.
    ds.SeriesDescription = "中文"

    fixed = cs._fix_cp1251_krakozyabry(ds)

    # The revert fired: the charset is untouched and both items keep their OWN
    # original text. Initially they must not have been swapped.
    assert str(fixed.get("SpecificCharacterSet", "")) == "ISO_IR 100"
    kept = [str(it.TextValue) for it in fixed.ContentSequence]
    assert kept[0] == first_text, f"item 1 got {kept[0]!r}"
    assert (
        kept[1] == second_text
    ), f"item 2 got {kept[1]!r} - the undo copied item 1's value into it"
    assert kept[0] != kept[1]


# ---------------------------------------------------------------------------
# R3 - the server-down requeue unpacks the batch tuple
# ---------------------------------------------------------------------------
def test_the_down_server_requeue_passes_a_uid_not_a_tuple():
    """`_batch` holds 5-tuples; passing one to `requeue` reached
    `_safe_uid`'s string methods and raised AttributeError for every item."""
    import inspect

    import clean_code.main as m

    src = inspect.getsource(m._outbox_worker)
    code = "\n".join(line.split("#", 1)[0] for line in src.splitlines())
    down = code.split("not health_checker.is_healthy()")[1].split("if not _was_paused")[
        0
    ]
    assert (
        "for _uid, _ds, _bp, _raw, _mod in list(_batch):" in down
    ), "the batch items must be unpacked before requeue"
    assert "outbox.requeue(_uid," in down


def test_requeue_rejects_a_tuple_by_failing_to_find_it(tmp_path):
    """A regression guard on the callee side: a tuple is not a UID, and the
    call must not look like it worked."""
    from clean_code.outbox import OutboxQueue

    q = OutboxQueue(str(tmp_path))
    with pytest.raises(Exception):
        q.requeue(("1.2.3", None, None, None, None), "server down")


# ---------------------------------------------------------------------------
# R5 - N-ACTION is refused when no tracker can honour it
# ---------------------------------------------------------------------------
class TestNactionWithoutTracker:
    def test_a_request_naming_sops_is_refused(self, monkeypatch):
        from clean_code.handlers import naction

        monkeypatch.setattr(
            naction,
            "_shutdown_requested",
            type("S", (), {"is_set": staticmethod(lambda: False)})(),
        )
        from tests.conftest import make_fake_naction_event

        event = make_fake_naction_event(
            sop_list=[("1.2.840.10008.5.1.4.1.1.2", "1.2.3.1")]
        )
        status, rsp = naction.handle_n_action(
            event,
            "http://127.0.0.1:1/dicomweb",
            False,
            None,
            commit_tracker=None,
        )
        assert status == 0x0110, "no tracker -> no N-EVENT-REPORT is possible"
        assert rsp is None

    def test_an_empty_request_is_still_acknowledged(self, monkeypatch):
        from clean_code.handlers import naction

        monkeypatch.setattr(
            naction,
            "_shutdown_requested",
            type("S", (), {"is_set": staticmethod(lambda: False)})(),
        )
        from tests.conftest import make_fake_naction_event

        event = make_fake_naction_event(sop_list=[])
        status, _rsp = naction.handle_n_action(
            event,
            "http://127.0.0.1:1/dicomweb",
            False,
            None,
            commit_tracker=None,
        )
        assert status == 0x0000


# ---------------------------------------------------------------------------
# R6 - a duplicate active Transaction UID is refused
# ---------------------------------------------------------------------------
def test_a_duplicate_transaction_uid_is_refused(tmp_path):
    from tests.test_commit_tracker_persistence import _Db, _tracker

    tracker = _tracker(_Db(str(tmp_path / "c.db")))
    sops = [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.1")]
    tracker.register("1.2.3.9", "MODALITY", "10.0.0.5", 104, sops)
    first = tracker._jobs["1.2.3.9"]

    with pytest.raises(ValueError) as exc:
        tracker.register("1.2.3.9", "MODALITY", "10.0.0.5", 104, sops)
    assert "already active" in str(exc.value)
    assert tracker._jobs["1.2.3.9"] is first
    tracker.close()


# ---------------------------------------------------------------------------
# R8 / R10 - abandoned state and the frozen verdict survive a restart
# ---------------------------------------------------------------------------
def test_the_frozen_verdict_survives_a_restart(tmp_path):
    """A restart after a report was sent but its response was lost rebuilt a
    possibly different verdict and reported a conflicting result."""
    from tests.test_commit_tracker_persistence import _Db, _tracker

    path = str(tmp_path / "c.db")
    tracker = _tracker(_Db(path))
    sops = [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.1")]
    tracker.register("1.2.3.7", "MODALITY", "10.0.0.5", 104, sops)
    job = tracker._jobs["1.2.3.7"]
    # Expire it so the tick resolves with everything unconfirmed.
    job.deadline = 0.0
    sent = []
    tracker._resolve = lambda j: (sent.append(j.frozen_verdict), False)[1]
    tracker._tick()
    frozen = job.frozen_verdict
    assert frozen is not None
    tracker.close()

    restarted = _tracker(_Db(path))
    try:
        revived = restarted._jobs["1.2.3.7"]
        assert (
            revived.frozen_verdict == frozen
        ), "the restored batch must re-send the SAME verdict"
    finally:
        restarted.close()


def test_an_abandoned_batch_is_not_revived_with_a_fresh_budget(tmp_path):
    from clean_code.commit_tracker import MAX_REPORT_ATTEMPTS

    from tests.test_commit_tracker_persistence import _Db, _tracker

    path = str(tmp_path / "c.db")
    tracker = _tracker(_Db(path))
    sops = [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.1")]
    tracker.register("1.2.3.8", "MODALITY", "10.0.0.5", 104, sops)
    job = tracker._jobs["1.2.3.8"]
    job.deadline = 0.0
    job.report_attempts = MAX_REPORT_ATTEMPTS - 1
    tracker._resolve = lambda j: False
    tracker._tick()
    assert "1.2.3.8" not in tracker._jobs
    tracker.close()

    restarted = _tracker(_Db(path))
    try:
        revived = restarted._abandoned["1.2.3.8"]
        assert revived.abandoned is True
        assert revived.report_attempts >= MAX_REPORT_ATTEMPTS
        assert (
            "1.2.3.8" not in restarted._jobs
        ), "R10: an abandoned batch is kept for inspection, not in the queue"
    finally:
        restarted.close()


# ---------------------------------------------------------------------------
# R11 - a rejected probe must not pin the gate shut
# ---------------------------------------------------------------------------
def test_a_rejected_probe_after_a_timeout_restores_the_gate(monkeypatch):
    """A timeout cleared the health Event; a later 401 (server back, probe
    unauthorised) never restored it, so every C-STORE stayed 0xA700."""
    import clean_code.health_checker as hc_mod
    import requests

    checker = hc_mod.HealthChecker(
        clarus_url="http://127.0.0.1:1",
        mode="always",
        interval=0.01,
        http_timeout=0.5,
    )

    original = hc_mod.requests.get

    def _timeout(*a, **k):
        raise requests.exceptions.Timeout("slow")

    hc_mod.requests.get = _timeout
    try:
        checker._poll_once(False)
    finally:
        hc_mod.requests.get = original
    assert not checker.is_healthy(), "a timeout is a real failure"

    class _Resp:
        status_code = 401
        content = b""

    hc_mod.requests.get = lambda *a, **k: _Resp()
    try:
        checker._poll_once(True)
    finally:
        hc_mod.requests.get = original

    healthy, status, _reason = checker.check_before_stow()
    assert healthy is True, "the server ANSWERED; C-STORE must not be blocked"
    assert status == 0x0000


# ---------------------------------------------------------------------------
# R12 - an empty body must not block on peek
# ---------------------------------------------------------------------------
def test_an_empty_body_does_not_peek_the_socket():
    """`Content-Length: 0` plus `peek(0)` could block to the socket timeout."""
    import inspect

    import clean_code.http_server as hs

    src = inspect.getsource(hs)
    code = "\n".join(line.split("#", 1)[0] for line in src.splitlines())
    assert "remaining > 0" in code, "the peek must be gated on a non-empty body"


# ---------------------------------------------------------------------------
# R13 - IPv4-mapped addresses compare equal to their IPv4 form
# ---------------------------------------------------------------------------
@pytest.mark.parametrize(
    "mapped,plain",
    [
        ("::ffff:10.0.0.5", "10.0.0.5"),
        ("::ffff:192.168.1.20", "192.168.1.20"),
        ("::ffff:127.0.0.1", "127.0.0.1"),
    ],
)
def test_ipv4_mapped_addresses_compare_equal(mapped, plain):
    """A dual-stack listener reports an IPv4 peer as ::ffff:a.b.c.d; peers.conf
    says a.b.c.d - comparing them literally never matches."""
    from clean_code.utils import normalize_host

    assert normalize_host(mapped) == normalize_host(plain)


def test_a_real_ipv6_address_is_not_mangled():
    from clean_code.utils import normalize_host

    assert normalize_host("2001:db8::1") == "2001:db8::1"


# ---------------------------------------------------------------------------
# R14 / R16 - C-FIND status mapping
# ---------------------------------------------------------------------------
def _cfind_event():
    """A C-FIND event with an empty identifier: the default params apply."""
    from pydicom import Dataset

    Req = type("Req", (), {"ae_title": "TEST_SCU", "address": "10.0.0.5"})

    class _Ev:
        identifier = Dataset()
        assoc = type("A", (), {"requestor": Req()})()

    return _Ev()


class TestCfindStatusMapping:
    def test_429_is_transient_not_a_bad_query(self, monkeypatch):
        """Rate-limiting was mapped to 0xC000 ("your query is wrong"), so the
        SCU never retried a query that would succeed a moment later."""
        import clean_code.handlers.cfind as cf
        import httpx

        def _raise(*a, **k):
            req = httpx.Request("GET", "http://x/studies")
            resp = httpx.Response(429, request=req)
            raise httpx.HTTPStatusError("429", request=req, response=resp)

        monkeypatch.setattr(cf, "_qido_request", _raise)
        statuses = list(
            cf._handle_qido_search(
                _cfind_event(), "http://x", False, 5, 5, {"PatientName": "*"}, "C-FIND"
            )
        )
        assert (
            statuses and statuses[0][0] == 0xA700
        ), f"429 must be transient 0xA700, got {statuses}"

    def test_a_malformed_body_yields_a700_instead_of_escaping(self, monkeypatch):
        """QidoMalformedBodyError is a ValueError; uncaught it killed the
        generator and the SCU got an arbitrary status."""
        import clean_code.handlers.cfind as cf
        from clean_code.http_helpers import QidoMalformedBodyError

        def _raise(*a, **k):
            raise QidoMalformedBodyError("proxy 200 with HTML")

        monkeypatch.setattr(cf, "_qido_request", _raise)
        statuses = list(
            cf._handle_qido_search(
                _cfind_event(), "http://x", False, 5, 5, {"PatientName": "*"}, "C-FIND"
            )
        )
        assert statuses and statuses[0][0] == 0xA700


# ---------------------------------------------------------------------------
# R15 - the batch result list follows payload order
# ---------------------------------------------------------------------------
def test_unmatched_failure_results_follow_payload_order():
    """The per-SOP results were built from a set, so the outbox - which
    matches by index - associated outcomes with the wrong instances."""
    import inspect

    import clean_code.http_helpers as hh

    src = inspect.getsource(hh.stow_batch_instances)
    code = "\n".join(line.split("#", 1)[0] for line in src.splitlines())
    # Take the text between the `unmatched` test and the return it guards.
    after = code.split("if unmatched:")[1]
    branch = after.split("return")[1].split("], None")[0]
    assert "for uid, _ds, _raw in live_payloads" in branch, (
        f"the unmatched branch must iterate the payloads actually sent, "
        f"not the `sent` set: {branch!r}"
    )


# ---------------------------------------------------------------------------
# Part 4 - print SCU conformance (D1-D8)
# ---------------------------------------------------------------------------
class _FakeStatus:
    def __init__(self, code):
        self.Status = code


class _FakeAssoc:
    """Records every N-* call and replays canned responses."""

    def __init__(self, image_box_uids=("1.2.3.4.10", "1.2.3.4.11")):
        self.calls = []
        self.released = False
        self.is_established = True
        self._image_box_uids = image_box_uids

    def _record(self, kind, *a, **k):
        self.calls.append((kind, a, k))
        return _FakeStatus(0x0000)

    def send_n_create(self, ds, class_uid, instance_uid=None, msg_id=1, meta_uid=None):
        self.calls.append(
            ("n_create", (ds, class_uid, instance_uid), {"meta_uid": meta_uid})
        )
        if str(class_uid).startswith("1.2.840.10008.5.1.1.2"):
            from pydicom import Dataset

            rsp = Dataset()
            seq = []
            for uid in self._image_box_uids:
                item = Dataset()
                item.ReferencedSOPClassUID = "1.2.840.10008.5.1.1.4"
                item.ReferencedSOPInstanceUID = uid
                seq.append(item)
            rsp.ReferencedImageBoxSequence = seq
            return _FakeStatus(0x0000), rsp
        return _FakeStatus(0x0000), None

    def send_n_set(self, ds, class_uid, instance_uid, msg_id=1, meta_uid=None):
        self.calls.append(
            ("n_set", (ds, class_uid, instance_uid), {"meta_uid": meta_uid})
        )
        return _FakeStatus(0x0000), None

    def send_n_action(
        self, ds, action_type, class_uid, instance_uid, msg_id=1, meta_uid=None
    ):
        self.calls.append(
            (
                "n_action",
                (ds, action_type, class_uid, instance_uid),
                {"meta_uid": meta_uid},
            )
        )
        return _FakeStatus(0x0000), None

    def send_n_delete(self, class_uid, instance_uid):
        self.calls.append(("n_delete", (class_uid, instance_uid), {}))
        return _FakeStatus(0x0000), None

    def release(self):
        self.released = True


def _image_bytes():
    import io

    from pydicom import Dataset, dcmwrite

    ds = Dataset()
    ds.SOPClassUID = "1.2.840.10008.5.1.4.1.1.7"
    ds.SOPInstanceUID = "1.2.3.4.5"
    ds.SamplesPerPixel = 1
    ds.PhotometricInterpretation = "MONOCHROME2"
    ds.Rows = 1
    ds.Columns = 1
    ds.BitsAllocated = 8
    ds.BitsStored = 8
    ds.HighBit = 7
    ds.PixelRepresentation = 0
    ds.PixelData = b"\x00"
    buf = io.BytesIO()
    dcmwrite(buf, ds, implicit_vr=False, little_endian=True, enforce_file_format=False)
    return buf.getvalue()


def _run_print(monkeypatch, assoc=None, images=None):
    import sys
    from pathlib import Path

    import clean_code  # noqa: F401  (ensures the package is importable)

    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
    from drivers import generic_dicom as gd

    assoc = assoc or _FakeAssoc()
    created = {}

    class _AE:
        def __init__(self, **k):
            created["ae"] = self
            self.contexts = []
            self.ae_title = k.get("ae_title")

        def add_requested_context(self, *a, **k):
            self.contexts.append(a[0] if a else k.get("abstract_syntax"))

        def associate(self, host, port, ae_title=None):
            created["called"] = ae_title
            created["host"] = host
            return assoc

    monkeypatch.setattr(gd, "AE", _AE)

    from pydicom import Dataset

    film_session = Dataset()
    film_session.NumberOfCopies = 1
    film_session.ImageDisplayFormat = "STANDARD\\1,1"

    result = gd.GenericDicomPrintDriver().print(
        film_session=film_session,
        images=images if images is not None else [_image_bytes()],
        printer_host="10.0.0.9",
        printer_port=11112,
        calling_aet="CLARUS_BRIDGE",
        called_aet="PRINTER_AE",
    )
    return result, assoc, created


class TestPrintDriverConformance:
    def test_d1_the_called_ae_is_the_printer_not_us(self, monkeypatch):
        _result, _assoc, created = _run_print(monkeypatch)
        assert (
            created["called"] == "PRINTER_AE"
        ), "PS3.8 9.3.2.1: the Called AE Title is the printer's"
        assert created["ae"].ae_title == "CLARUS_BRIDGE", "our own SCU identity"

    def test_d8_the_meta_sop_class_is_negotiated_and_used(self, monkeypatch):
        from pynetdicom.sop_class import BasicGrayscalePrintManagementMeta

        _result, assoc, created = _run_print(monkeypatch)
        assert (
            BasicGrayscalePrintManagementMeta in created["ae"].contexts
        ), "PS3.4 H.3: the Print Management Meta SOP Class must be proposed"
        # Every N-* primitive must carry the meta UID so the SCP can resolve
        # the operation against the negotiated meta class.
        checked = 0
        for kind, args, kwargs in assoc.calls:
            if kind in ("n_create", "n_set", "n_action"):
                assert (
                    kwargs.get("meta_uid") == BasicGrayscalePrintManagementMeta
                ), f"{kind} did not pass the meta SOP class: {kwargs}"
                checked += 1
        assert checked >= 3, f"expected create/set/action, saw {checked}"

    def test_d7_the_film_session_carries_only_session_attributes(self, monkeypatch):
        _result, assoc, _created = _run_print(monkeypatch)
        first = assoc.calls[0]
        fs_ds = first[1][0]
        assert hasattr(fs_ds, "NumberOfCopies")
        assert not hasattr(
            fs_ds, "ImageDisplayFormat"
        ), "ImageDisplayFormat is a Film BOX attribute (PS3.4 H.4.1.2)"

    def test_d3_d4_nset_uses_the_printer_image_boxes_and_sets_position(
        self, monkeypatch
    ):
        assoc = _FakeAssoc(image_box_uids=("9.9.9.1", "9.9.9.2"))
        _result, assoc, _created = _run_print(monkeypatch, assoc=assoc)
        sets = [c for c in assoc.calls if c[0] == "n_set"]
        assert len(sets) == 1
        ds = sets[0][1][0]
        assert (
            ds.ImageBoxPosition == 1
        ), "PS3.4 H.4.1.2.4: ImageBoxPosition is required in the N-SET"
        assert sets[0][1][2] == "9.9.9.1", (
            "the N-SET must address the Image Box the printer created, not a "
            "locally generated UID"
        )

    def test_d5_the_print_action_addresses_the_film_box(self, monkeypatch):
        from pynetdicom.sop_class import BasicFilmBox

        _result, assoc, _created = _run_print(monkeypatch)
        actions = [c for c in assoc.calls if c[0] == "n_action"]
        assert len(actions) == 1
        class_uid = actions[0][1][2]
        assert (
            class_uid == BasicFilmBox
        ), "PS3.4 H.4.1.2.3: the Print N-ACTION addresses the Film Box"

    def test_d2_no_images_accepted_means_no_print(self, monkeypatch):
        """An unpopulated film must not be N-ACTIONed and must not be counted
        as printed."""
        assoc = _FakeAssoc()

        def _reject(*a, **k):
            return _FakeStatus(0xC000), None

        assoc.send_n_set = _reject
        result, assoc, _created = _run_print(monkeypatch, assoc=assoc)
        assert result.film_boxes_printed == 0
        assert not [
            c for c in assoc.calls if c[0] == "n_action"
        ], "an empty film must not be printed"

    def test_d6_a_warning_status_does_not_abort_the_job(self, monkeypatch):
        """PS3.7 Annex C: 0xBxxx is WARNING - the primitive completed."""
        assoc = _FakeAssoc()
        real_create = assoc.send_n_create

        def _warn(ds, class_uid, instance_uid=None, msg_id=1, meta_uid=None):
            _status, rsp = real_create(ds, class_uid, instance_uid, msg_id, meta_uid)
            return _FakeStatus(0xB600), rsp

        assoc.send_n_create = _warn
        result, assoc, _created = _run_print(monkeypatch, assoc=assoc)
        assert (
            result.film_boxes_printed == 1
        ), "a warning on N-CREATE must not be read as a failure"

    def test_a_failure_status_still_aborts(self, monkeypatch):
        assoc = _FakeAssoc()

        def _fail(*a, **k):
            return _FakeStatus(0xC000), None

        assoc.send_n_create = _fail
        result, _assoc, _created = _run_print(monkeypatch, assoc=assoc)
        assert result.film_boxes_printed == 0
        assert result.status != 0x0000
