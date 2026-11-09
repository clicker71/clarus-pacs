# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""0709 honest C-MOVE final statuses - regression (D1).

D1a: zero slices delivered -> C-MOVE-RSP Failure 0xC000 with (0000,0902)
     Error Comment, not 0xB000 "Sub-operations Complete" (a lie at zero).
D1b: Failed/Cancel/Warning carry the (0008,0058) Failed SOP Instance UID
     List (PS3.4 C.4.2.1.4.2).
D1c: an empty accepted-context map no longer leaves 0xC001 only in the log;
     the job fails with zero slices and the generator reports a real 0xC000.

The C-MOVE generator is a state machine over the DB, so these are driven
deterministically with a real MoveJobManager (no live peer, no live server):
the reactor's per-instance-count HTTP and its 0.5s poll sleeps are stubbed.
"""

# import sqlite3 helper for an in-memory MoveJobManager (mirrors the handler
# test suite's SharedDb-compatible wrapper)
import sqlite3
import threading

import pytest
from clean_code.handlers.cmove import handle_c_move
from clean_code.move_jobs import MoveJobManager

from tests.conftest import make_fake_move_event


class _TestDb:
    def __init__(self, path):
        self._conn = sqlite3.connect(path, check_same_thread=False)
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._lock = threading.Lock()

    def locked(self):
        class _L:
            def __init__(s, lock, conn):
                s._lock, s._conn = lock, conn

            def __enter__(s):
                s._lock.acquire()
                return s._conn

            def __exit__(s, *a):
                s._lock.release()

        return _L(self._lock, self._conn)

    def close(self):
        with self._lock:
            try:
                self._conn.close()
            except Exception:
                pass


@pytest.fixture
def move_jobs():
    db = _TestDb(":memory:")
    m = MoveJobManager(shared_db=db)
    m.register_peer("DEST_AE", "127.0.0.1", 11112)
    yield m
    m.close()
    db.close()


def _start_generator(move_jobs, monkeypatch):
    """Create a handle_c_move generator for one DEST_AE study.

    RSP policy (e): the generator yields the destination then holds the DIMSE
    dialog with a DB poll until the job is terminal (the poll removes the old
    per-instance QIDO HTTP and the bare pending-count yield). Poll sleeps are
    stubbed to no-op so tests drive terminal state directly. Returns the
    generator, already past the destination yield (the terminal int/status
    yields are emitted only once the test finishes the job).
    """
    import clean_code.handlers.cmove as cm

    monkeypatch.setattr(cm.time, "sleep", lambda *a: None)
    event = make_fake_move_event(
        study_uid="1.2.3.4.5",
        move_dest="DEST_AE",
        calling_ae="DEST_AE",
        calling_ip="127.0.0.1",
    )
    gen = handle_c_move(
        event,
        move_jobs,
        peers_file="",
        clarus_url="http://127.0.0.1:1",  # refused fast; reactor makes no HTTP
        bridge_ae_title="TEST_BRIDGE",
        move_poll_timeout=30,
    )
    addr = next(gen)
    assert addr == ("127.0.0.1", 11112)
    return gen


def _drain_final(gen):
    """Advance the generator to its terminal (status, dataset) yield.

    The reactor is stubbed to sleep immediately, so it runs until the poll
    sees the job(s) finished and emits the final status. Returns (status,
    identifier) of the last C-MOVE-RSP (or None status pair)."""
    last = None
    finished = False
    while not finished:
        try:
            item = next(gen)
        except StopIteration:
            break
        # address/pending ints before the final yield are consumed by
        # _start_generator; here only pending-progress 0xFF00 int/dicts and
        # the final (status, dataset).
        if isinstance(item, tuple) and len(item) == 2 and isinstance(item[0], int):
            last = item
            # after the final status the generator returns
        # single ints are pending sub-op counts (0xFF00) - keep draining
    return last


def test_zero_delivered_failure_is_c000_with_error_comment(move_jobs, monkeypatch):
    """D1a: a job that finished failed with ZERO slices delivered yields
    Failure 0xC000 with an (0000,0902) Error Comment, never 0xB000."""
    gen = _start_generator(move_jobs, monkeypatch)
    job = move_jobs.pick_pending_job(timeout=0)
    jid = job["id"]
    move_jobs.update_job_progress(jid, 0, 0, 0)
    move_jobs.set_job_failed_sops(jid, [])
    move_jobs.finish_job(jid, success=False, error_message="Cannot associate")
    status, identifier = _drain_final(gen)
    assert status == 0xC000, hex(status)
    assert identifier is not None
    assert (0x0000, 0x0902) in identifier, "C000 Error Comment (0000,0902) expected"


def test_partial_failure_b000_with_failed_sop_uid_list(move_jobs, monkeypatch):
    """D1b: a job with 3/5 delivered then failed carries (0008,0058) with the
    two failed SOP Instance UIDs on the Warning (0xB000) status."""
    gen = _start_generator(move_jobs, monkeypatch)
    job = move_jobs.pick_pending_job(timeout=0)
    jid = job["id"]
    uid_a = "1.3.6.1.4.1.9.1"
    uid_b = "1.3.6.1.4.1.9.2"
    move_jobs.update_job_progress(jid, 3, 2, 5)
    move_jobs.set_job_failed_sops(jid, [uid_a, uid_b])
    move_jobs.finish_job(jid, success=False, error_message="2 sent, 2 failed")
    status, identifier = _drain_final(gen)
    assert status == 0xB000, hex(status)  # some sub-ops completed -> Warning
    assert identifier is not None
    failed = set(getattr(identifier.get((0x0008, 0x0058)), "value", []))
    assert failed == {uid_a, uid_b}, failed


def test_no_common_ts_reaches_viewer_as_c000(move_jobs, monkeypatch):
    """D1c: an empty accepted-context (no common transfer syntax) job fails
    with zero delivered and the generator reports a real 0xC000 - 0xC001 must
    not stay a log-only claim while the viewer receives it."""
    gen = _start_generator(move_jobs, monkeypatch)
    job = move_jobs.pick_pending_job(timeout=0)
    jid = job["id"]
    move_jobs.update_job_progress(jid, 0, 0, 0)
    move_jobs.set_job_failed_sops(jid, [])
    move_jobs.finish_job(
        jid,
        success=False,
        error_message="No common transfer syntax between viewer and server",
    )
    status, identifier = _drain_final(gen)
    assert status == 0xC000, hex(status)
    assert identifier is not None


def test_cfind_tm_colon_value_normalized_canonical(monkeypatch, caplog):
    """P4: a QIDO Time value in the display "HH:MM:SS" form used to make
    pydicom warn 'Invalid value for VR TM' on every C-FIND. The converter now
    strips the colons to the canonical DICOM form (PS3.5 6.2) - warning-free
    and the attribute keeps its value."""
    import logging

    from clean_code.protocol import QIDO_KEY_TO_TAG, _json_map_to_dataset

    json_obj = {"00080030": {"vr": "TM", "Value": ["11:25:01"]}}
    with caplog.at_level(logging.WARNING, logger="pydicom"):
        ds = _json_map_to_dataset(json_obj, QIDO_KEY_TO_TAG)
    # canonical value, no warning about an invalid TM
    elem = ds.get((0x0008, 0x0030))
    assert elem is not None and str(elem.value) == "112501"
    assert "Invalid value for VR TM" not in caplog.text
