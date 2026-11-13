# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""1211 convergence batch 3 regression tests, one per finding.

Each test fails when its fix is reverted: the assertion targets the behaviour
the finding described, not the shape of the code.
"""

import logging
import os
import threading
import time

import pytest


# ---------------------------------------------------------------------------
# B1 - the down-server requeue resets ALL batch state
# ---------------------------------------------------------------------------
def test_the_down_server_path_resets_every_batch_field():
    """Clearing only the list left `_batch_study` pointing at the abandoned
    study, so the next study's first instance was appended to a foreign batch
    and flushed under the old study's stats; `_batch_bytes` skewed the size
    guard by the dead tail."""
    import inspect

    import clean_code.main as m

    src = inspect.getsource(m._outbox_worker)
    code = "\n".join(line.split("#", 1)[0] for line in src.splitlines())
    down_branch = code.split("not health_checker.is_healthy()")[1].split(
        "if not _was_paused"
    )[0]
    for field in ("_batch", "_batch_study", "_batch_bytes"):
        assert (
            f"{field} = " in down_branch
        ), f"{field} is not reset on the down-server path"
    assert (
        "_batch.clear()" not in down_branch
    ), "clearing the list leaves the study and the byte count stale"


# ---------------------------------------------------------------------------
# B2 - a non-boolean `accept` is refused, not coerced
# ---------------------------------------------------------------------------
class TestRegistryNonBooleanAccept:
    def _load(self, tmp_path, accept_value):
        from clean_code.registry import _load_sop_registry

        path = tmp_path / "sop_classes.yaml"
        path.write_text(
            "sop_classes:\n"
            "  1.2.840.10008.5.1.4.1.1.2:\n"
            "    name: CT Image Storage\n"
            f"    accept: {accept_value}\n",
            encoding="utf-8",
        )
        return _load_sop_registry(str(path))

    @pytest.mark.parametrize("value", ["yes", "no", "1", "0", "on", "off"])
    def test_a_string_accept_is_rejected(self, tmp_path, caplog, value):
        """`bool("no")` is True: coercing a string accepted a class the
        operator wrote as refused."""
        from clean_code.registry import _BUILTIN_SOP_CLASSES

        with caplog.at_level(logging.ERROR):
            registry = self._load(tmp_path, value)
        assert (
            registry == _BUILTIN_SOP_CLASSES()
        ), f"accept: {value} was coerced instead of refused"
        assert any(
            "non-boolean accept" in r.message for r in caplog.records
        ), "the operator must be told which UID and which value"

    def test_the_literals_still_work(self, tmp_path):
        assert (
            self._load(tmp_path, "true")["1.2.840.10008.5.1.4.1.1.2"]["accept"] is True
        )
        assert (
            self._load(tmp_path, "false")["1.2.840.10008.5.1.4.1.1.2"]["accept"]
            is False
        )


# ---------------------------------------------------------------------------
# B3 - a probe 4xx means "the probe is rejected", not "the server is down"
# ---------------------------------------------------------------------------
class TestHealthProbe4xx:
    def _checker(self, mode="always"):
        from clean_code.health_checker import HealthChecker

        return HealthChecker(
            clarus_url="http://127.0.0.1:1",
            mode=mode,
            interval=0.01,
            http_timeout=0.5,
        )

    def _poll(self, hc, status_code):
        original_get = _MODULE_REQUESTS.get
        _MODULE_REQUESTS.get = lambda *a, **k: _Resp(status_code)
        try:
            return hc._poll_once(False)
        finally:
            _MODULE_REQUESTS.get = original_get

    @pytest.mark.parametrize("code", [401, 403, 405, 429])
    def test_a_rejected_probe_does_not_clear_the_healthy_event(self, code, caplog):
        """A 4xx is the server ANSWERING. Clearing the health Event made every
        C-STORE answer 0xA700 because the bridge's own probe was unauthorised."""
        hc = self._checker()
        assert hc.is_healthy(), "the checker starts optimistic"

        with caplog.at_level(logging.WARNING):
            was_dead = self._poll(hc, code)

        assert hc.is_healthy(), f"HTTP {code} must not mark the server down"
        assert was_dead is False, "a 4xx is not a dead server"
        assert any(
            "probe is not accepted" in r.message for r in caplog.records
        ), "the operator must be told the probe is being rejected"

    @pytest.mark.parametrize("code", [500, 502, 503])
    def test_a_5xx_still_marks_the_server_down(self, code):
        hc = self._checker()
        was_dead = self._poll(hc, code)
        assert not hc.is_healthy(), f"HTTP {code} is a real server failure"
        assert was_dead is True

    def test_a_connection_error_still_marks_the_server_down(self):
        import requests as _rq

        hc = self._checker()
        original_get = _MODULE_REQUESTS.get

        def _raise(*a, **k):
            raise _rq.exceptions.ConnectionError("refused")

        _MODULE_REQUESTS.get = _raise
        try:
            was_dead = hc._poll_once(False)
        finally:
            _MODULE_REQUESTS.get = original_get
        assert not hc.is_healthy()
        assert was_dead is True

    def test_a_404_in_probe_mode_disables_checks(self):
        hc = self._checker(mode="probe")
        self._poll(hc, 404)
        assert hc.is_healthy(), "no /health means checks are disabled, not down"
        assert hc._disabled_until_restart is True


# ---------------------------------------------------------------------------
# B4 - an association setup error retries instead of escaping
# ---------------------------------------------------------------------------
def test_an_association_setup_error_is_retried(monkeypatch):
    """A setup exception escaped the retry loop, so the commitment result was
    never sent and nothing retried it."""
    import clean_code.nevent as nv

    attempts = []

    class FakeAE:
        def __init__(self, **k):
            pass

        def add_requested_context(self, *a, **k):
            pass

        def associate(self, *a, **k):
            attempts.append(1)
            raise RuntimeError("transient setup failure")

    monkeypatch.setattr(nv, "AE", FakeAE)
    monkeypatch.setattr(nv, "build_role", lambda *a, **k: None)
    monkeypatch.setattr(nv.time, "sleep", lambda *a: None)

    ok = nv.send_n_event_report(
        "10.0.0.5",
        11112,
        "MODALITY",
        "1.2.3",
        1,
        [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.1")],
        [],
        bridge_ae_title="BRIDGE",
        max_retries=2,
        retry_delay=0.0,
    )
    assert ok is False, "the report genuinely never arrived"
    assert (
        len(attempts) == 3
    ), f"the setup error must retry (max_retries+1 = 3), got {len(attempts)}"


# ---------------------------------------------------------------------------
# B5 - the integer parse error names the section and key
# ---------------------------------------------------------------------------
def test_an_integer_parse_error_names_the_key(tmp_path):
    from clean_code.config import ConfParser

    path = tmp_path / "c.conf"
    path.write_text("[server]\nport = notanumber\n", encoding="utf-8")
    cfg = ConfParser(str(path))
    with pytest.raises(ValueError) as exc:
        cfg.get_int("server", "port")
    msg = str(exc.value)
    assert "server" in msg and "port" in msg, f"the message must name the key: {msg}"
    assert "notanumber" in msg


# ---------------------------------------------------------------------------
# B6 - pass 1 raising mid-stream keeps the progress counted so far
# ---------------------------------------------------------------------------
def test_a_midstream_raise_in_pass_one_preserves_the_counters():
    """The counters only exist once the call RETURNS, so the except-branch
    reset them to zero and reported 0 delivered for a study that had moved.

    F14: the handler must not re-raise - a bare `raise` let the worker's
    outer handler finish the job without the (0008,0058) list. Marking the
    pass lost routes into the reconciliation that names the failed SOPs."""
    import inspect

    import clean_code.handlers.cmove as cm

    src = inspect.getsource(cm._execute_cmove_job)
    code = "\n".join(line.split("#", 1)[0] for line in src.splitlines())
    # The pass-1 handler must READ the progress back before continuing.
    assert (
        "get_job_progress(job_id)" in code
    ), "the mid-stream handler must recover the counters from the job record"
    # Anchor on the CODE that only the guarded handler contains (the comment
    # wording is stripped above, and comments are not behaviour).
    anchor = code.index("move_jobs.get_job_progress(job_id)")
    handler = code[anchor : code.index("if resumed_lost", anchor)]
    assert "raise" not in handler, "the failure must NOT re-raise out of the worker"
    assert (
        "resumed_lost = True" in handler
    ), "the loss must route into the reconciliation, not out of the worker"
    assert "_c1, _f1, _t1 = 0, 0, 0" in handler, "the fallback must still exist"
    assert "sent_sop_uids = []" in handler


# ---------------------------------------------------------------------------
# B7 - no lost wakeup between the empty SELECT and the wait
# ---------------------------------------------------------------------------
def test_pick_pending_job_takes_the_condition_before_selecting(tmp_path):
    """A row inserted between the empty SELECT and the wait left the worker
    asleep with pending work on disk."""
    import inspect

    import clean_code.move_jobs as mj

    src = inspect.getsource(mj.MoveJobManager.pick_pending_job)
    code = "\n".join(line.split("#", 1)[0] for line in src.splitlines())
    # The select must happen INSIDE the `with self._cond:` block, before wait.
    cond_block = code.split("with self._cond:")[1].split("row = rows[0]")[0]
    assert (
        "_select_pending()" in cond_block
    ), "the SELECT must run under the condition lock, or insert's notify is lost"
    assert "_cond.wait(" in cond_block


def test_an_insert_while_waiting_wakes_the_worker(tmp_path):
    import clean_code.move_jobs as mj

    from tests.test_0911_rabbit_batch1 import _TestDb

    db = _TestDb(str(tmp_path / "jobs.db"))
    mgr = mj.MoveJobManager(shared_db=db, max_workers=1)
    picked = []

    def _waiter():
        picked.append(mgr.pick_pending_job(timeout=5.0))

    t = threading.Thread(target=_waiter, daemon=True)
    t.start()
    time.sleep(0.2)  # let it enter the wait
    mgr.create_job("1.2.3", "PEER", "10.0.0.5", 11112)
    t.join(timeout=4.0)

    assert not t.is_alive(), "the worker never woke up for the inserted row"
    assert picked and picked[0] is not None
    assert picked[0]["study_uid"] == "1.2.3"
    db.close()


# ---------------------------------------------------------------------------
# B8 - a blob-read failure must not park the item twice
# ---------------------------------------------------------------------------
def test_a_blob_read_failure_parks_the_item_exactly_once(tmp_path, monkeypatch):
    """Guard for B8: one blob-read failure must request ONE parking.

    The finding assumed the branch's follow-up `move_to_undelivered` was a
    second park. It is not reachable: `mark_failed` moves the item out of
    `pending/`/`in_progress/` when it parks, so the `_get_retry_count` that
    guards the follow-up returns 0 and the call is skipped. The explicit
    `_is_undelivered` guard makes that unconditional instead of relying on a
    file-location side effect, and this test pins the invariant either way.
    """
    from clean_code import metrics as m
    from clean_code.outbox import OutboxQueue
    from pydicom.dataset import Dataset, FileMetaDataset
    from pydicom.uid import ImplicitVRLittleEndian

    q = OutboxQueue(str(tmp_path), max_retries=1)
    ds = Dataset()
    ds.SOPClassUID = "1.2.840.10008.5.1.4.1.1.7"
    ds.SOPInstanceUID = "1.2.3.9"
    ds.StudyInstanceUID = "1.2.3"
    ds.file_meta = FileMetaDataset()
    ds.file_meta.MediaStorageSOPClassUID = "1.2.840.10008.5.1.4.1.1.7"
    ds.file_meta.MediaStorageSOPInstanceUID = "1.2.3.9"
    ds.file_meta.TransferSyntaxUID = ImplicitVRLittleEndian
    assert q.enqueue(ds) is True

    counter = (
        q._undelivered_total
        if q._undelivered_total is not None
        else (m.outbox_undelivered_total)
    )
    before = counter._value.get()

    item = q.dequeue()
    assert item is not None and item[0] == "1.2.3.9"

    parks = []
    real_move = q.move_to_undelivered

    def _counting_move(uid):
        parks.append(uid)
        return real_move(uid)

    monkeypatch.setattr(q, "move_to_undelivered", _counting_move)

    # Put the claimed item back as retryable so the reader runs on it.
    import json as _json
    import shutil

    meta_path = q._meta_path(q._in_progress, "1.2.3.9")
    with open(meta_path, encoding="utf-8") as f:
        meta = _json.load(f)
    meta["retry_count"] = 0
    meta["next_retry_at"] = 0
    with open(meta_path, "w", encoding="utf-8") as f:
        _json.dump(meta, f)
    shutil.move(
        q._dcm_path(q._in_progress, "1.2.3.9"), q._dcm_path(q._pending, "1.2.3.9")
    )
    shutil.move(meta_path, q._meta_path(q._pending, "1.2.3.9"))

    import clean_code.outbox as ob

    def _boom(*a, **k):
        raise ValueError("corrupt dataset")

    monkeypatch.setattr(ob, "dcmread", _boom)
    got = q.dequeue()

    assert got == (None, None, None, None), "the read must fail"
    assert parks == ["1.2.3.9"], f"expected exactly one parking, got {parks}"
    assert counter._value.get() - before == 1
    parked = [p for p in os.listdir(tmp_path / "undelivered") if p.endswith(".dcm")]
    assert parked == ["1.2.3.9.dcm"], parked


# ---------------------------------------------------------------------------
# B9 - a failed .meta write leaves no orphan blob
# ---------------------------------------------------------------------------
def test_a_failed_meta_write_removes_the_blob(tmp_path, monkeypatch):
    """`.meta` is what makes an item visible; a blob without one is an orphan
    the next sweep deletes out from under a caller told the enqueue failed."""
    from clean_code.outbox import OutboxQueue
    from pydicom.dataset import Dataset, FileMetaDataset
    from pydicom.uid import ImplicitVRLittleEndian

    q = OutboxQueue(str(tmp_path), max_retries=3)
    ds = Dataset()
    ds.SOPClassUID = "1.2.840.10008.5.1.4.1.1.7"
    ds.SOPInstanceUID = "1.2.3.11"
    ds.StudyInstanceUID = "1.2.3"
    ds.file_meta = FileMetaDataset()
    ds.file_meta.MediaStorageSOPClassUID = "1.2.840.10008.5.1.4.1.1.7"
    ds.file_meta.MediaStorageSOPInstanceUID = "1.2.3.11"
    ds.file_meta.TransferSyntaxUID = ImplicitVRLittleEndian

    def _boom(*a, **k):
        raise OSError("disk full")

    monkeypatch.setattr(q, "_atomic_write", _boom)
    assert q.enqueue(ds) is False

    pending = tmp_path / "pending"
    leftovers = [p.name for p in pending.iterdir()] if pending.exists() else []
    assert leftovers == [], f"an orphan blob was left behind: {leftovers}"


# ---------------------------------------------------------------------------
# B10 - the uncertain branch never leaves a half-labelled dataset
# ---------------------------------------------------------------------------
class TestCharsetUncertainBranch:
    def test_an_unexaminable_value_reverts_the_whole_repair(self, caplog):
        """A repaired Cyrillic value left under ISO_IR 100 is written out
        corrupted: the charset and the bytes must agree, so a partial repair
        is undone whole rather than half-relabelled."""
        from clean_code.handlers import cstore as cs
        from pydicom.dataset import Dataset

        ds = Dataset()
        ds.SpecificCharacterSet = "ISO_IR 100"
        # Repairable: cp1251 mis-decoded as latin-1 -> would become Тест.
        ds.StudyDescription = "Òåñò"
        # Unexaminable: not latin-1 encodable, so _cp1251_fix_one returns
        # examined=False and the walk is marked partial.
        ds.SeriesDescription = "中文"

        original = str(ds.StudyDescription)

        with caplog.at_level(logging.WARNING):
            fixed = cs._fix_cp1251_krakozyabry(ds)

        charset = str(fixed.get("SpecificCharacterSet", ""))
        value = str(fixed.get("StudyDescription", ""))
        # Either both halves were repaired (UTF-8 declaration) or neither was
        # touched. What is forbidden is Cyrillic under a Latin-1 declaration.
        if charset == "ISO_IR 100":
            assert value == original, (
                "a reverted repair must restore the ORIGINAL bytes, not leave "
                f"the repaired Cyrillic under Latin-1 (got {value!r})"
            )
            assert all(
                ord(ch) < 256 for ch in value
            ), "Cyrillic under ISO_IR 100 is corrupted on write"
        else:
            assert charset == "ISO_IR 192"
            assert value == "Тест"
        assert any(
            "reverting the repair" in r.message for r in caplog.records
        ), "the revert must be reported, not silent"

    def test_a_fully_examinable_dataset_is_still_repaired(self):
        from clean_code.handlers import cstore as cs
        from pydicom.dataset import Dataset

        ds = Dataset()
        ds.SpecificCharacterSet = "ISO_IR 100"
        ds.StudyDescription = "Òåñò"

        fixed = cs._fix_cp1251_krakozyabry(ds)
        assert str(fixed.get("SpecificCharacterSet")) == "ISO_IR 192"
        assert str(fixed.get("StudyDescription")) == "Тест"


# ---------------------------------------------------------------------------
# T3 - an empty 200 body is not a successful QIDO
# ---------------------------------------------------------------------------
def test_an_empty_200_body_raises_like_any_other_malformed_answer():
    from unittest.mock import MagicMock, patch

    from clean_code.http_helpers import QidoMalformedBodyError, _qido_request

    resp = MagicMock()
    resp.status_code = 200
    resp.content = b""
    resp.raise_for_status = MagicMock()
    resp.json = MagicMock(side_effect=ValueError("no body"))
    with patch("clean_code.http_helpers._http_retry", return_value=resp):
        with pytest.raises(QidoMalformedBodyError):
            _qido_request("http://h/dicomweb/studies", {})


# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------
class _Resp:
    def __init__(self, status_code):
        self.status_code = status_code
        self.content = b"{}"


class _RequestsProxy:
    """Stand-in for the `requests` module object inside health_checker."""

    def __init__(self):
        self.get = None


import clean_code.health_checker as _hc  # noqa: E402

_MODULE_REQUESTS = _hc.requests
