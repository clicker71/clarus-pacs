# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""1013 rabbit pass 5 - wire-honesty defects (D1, D2) in the C-MOVE final.

D1: the terminal sub-operation count must never be 0 on a failure path.
`move_jobs` progress rows are written by `_stream_and_send_study`, which sets
`total = idx + 1` per instance WHILE IT STREAMS. Mid-stream `completed ==
total` therefore holds for a job that delivered every instance it sent and
then lost the association - the progress row is a streaming artefact, not a
completion proof. Deriving the count from it yielded 0, and pynetdicom answers
0x0000 Success and stops reading, so the real 0xB000/0xC000 never reached the
viewer.

D2: the Identifier of a partial final must carry a non-empty (0008,0058)
Failed SOP Instance UID List (PS3.4 C.4.2.1.4.2). When both passes were lost
the code discarded the per-instance failures pass 1 had OBSERVED.
"""

import pytest
from clean_code.handlers import cmove as cm
from clean_code.move_jobs import MoveJobManager

STUDY = "2.25.111122223333.9999"
MR = "1.2.840.10008.5.1.4.1.1.4"
SOP_ALL = ["1.2.3.4.5.6.7.8.9.10.%d" % i for i in (1, 2, 3, 4)]


class _Db:
    """SharedDb-shaped wrapper: one connection, one lock."""

    def __init__(self):
        import sqlite3
        import threading

        self._conn = sqlite3.connect(":memory:", check_same_thread=False)
        self._lock = threading.Lock()

    def locked(self):
        class _L:
            def __init__(s, lock, conn):
                s._lock = lock
                s._conn = conn

            def __enter__(s):
                s._lock.acquire()
                return s._conn

            def __exit__(s, *a):
                s._lock.release()

        return _L(self._lock, self._conn)


@pytest.fixture
def move_jobs():
    """A real MoveJobManager: D1 is about what lands in the progress ROW."""
    m = MoveJobManager(shared_db=_Db())
    m.register_peer("SCP", "127.0.0.1", 11112)
    yield m
    m.close()


def _stub_counters(monkeypatch):
    monkeypatch.setattr(
        cm,
        "move_jobs_counter",
        type(
            "C",
            (),
            {"labels": lambda *a, **k: type("L", (), {"inc": lambda *a, **k: None})()},
        )(),
    )


def _drive_cmove_generator(move_jobs, monkeypatch, poll_timeout=None):
    """Run `handle_c_move` far enough to pull its terminal yields.

    The worker never runs here, so the job's progress row is whatever the
    test puts in it - which is exactly the D1 mechanism: `completed == total`
    with `status == "failed"`.
    """
    from tests.conftest import make_fake_move_event

    event = make_fake_move_event(
        study_uid=STUDY, move_dest="SCP", calling_ae="SCP", calling_ip="127.0.0.1"
    )
    gen = cm.handle_c_move(
        event,
        move_jobs,
        peers_file="",
        clarus_url="http://127.0.0.1:8024/dicomweb",
        bridge_ae_title="BRIDGE",
        move_poll_timeout=poll_timeout,
    )
    assert next(gen) == ("127.0.0.1", 11112), "first yield is the destination"
    return gen


def test_a_failed_job_whose_counters_match_never_advertises_zero(move_jobs, monkeypatch):
    """D1: progress row has completed == total, status failed -> the count
    must be NON-zero and the status must be the honest 0xB000.

    Mechanism under test: `total` is written as `idx + 1` while streaming, so
    a job that delivered everything it sent and then lost the association
    reports matching counters. A 0 count makes pynetdicom send 0x0000 Success
    and stop reading, and the viewer is told the study arrived.
    """
    _stub_counters(monkeypatch)
    gen = _drive_cmove_generator(move_jobs, monkeypatch)

    # Delivered 4 of 4 sent, then the association died on the next one.
    move_jobs.update_job_progress(1, completed=4, failed=0, total=4)
    move_jobs.set_job_failed_sops(1, [SOP_ALL[0]])
    move_jobs.finish_job(1, success=False)

    pulled = list(gen)
    assert len(pulled) == 2, pulled
    count, status = pulled
    assert count != 0, (
        "a failure path must never advertise 0 sub-operations: pynetdicom "
        "answers 0x0000 Success and drops the real status"
    )
    assert status[0] == 0xB000, hex(status[0])


def test_a_timed_out_job_never_advertises_zero(move_jobs, monkeypatch):
    """D1, poll-timeout branch: same rule - the count is never 0 there."""
    _stub_counters(monkeypatch)
    gen = _drive_cmove_generator(move_jobs, monkeypatch, poll_timeout=0)

    # The worker reported progress and then stalled; `completed == total`.
    move_jobs.update_job_progress(1, completed=2, failed=0, total=2)

    pulled = list(gen)
    assert len(pulled) == 2, pulled
    count, status = pulled
    assert count != 0, "a poll timeout must never advertise 0 sub-operations"
    assert status[0] == 0xB000, hex(status[0])


def test_both_passes_lost_keeps_the_observed_failed_list(monkeypatch):
    """D2: both passes lost, pass 1 observed one failure -> the final
    (0008,0058) list must still name it.

    The old code used `[]` whenever `resumed_lost` was set, so a 0xB000
    carried an EMPTY Failed SOP Instance UID List, which PS3.4 C.4.2.1.4.2
    does not allow.
    """
    _stub_counters(monkeypatch)
    observed = [SOP_ALL[0]]

    def fake_associate(*a, **k):
        return type("A", (), {"is_established": True, "accepted_contexts": []})(), {}

    def fake_stream(
        assoc,
        study,
        clarus_url,
        http_timeout,
        move_jobs,
        job_id,
        accepted_contexts_map=None,
        server_ts_list=None,
        sop_registry=None,
        tls=None,
        spill_threshold=0,
        peer_ae="",
        _skip_ids=None,
        _resumed=False,
        _report_lost=False,
        _prior_completed=0,
        _prior_failed=0,
        _prior_total=0,
        request_level="",
        request_series_uid="",
        request_sop_uid="",
        expose_rejected=False,
        qa_url="",
    ):
        if not _resumed:
            # Lost after delivering one instance; one OBSERVED failure.
            return (1, 1, 4, [(MR, SOP_ALL[1])], list(observed), False, False, True)
        # Resumed pass lost again with nothing observed.
        return (0, 0, 4, [], [], False, False, True)

    monkeypatch.setattr(cm, "_associate_for_move", fake_associate)
    monkeypatch.setattr(cm, "_preflight_study_ts_by_class", lambda *a, **k: {})
    monkeypatch.setattr(cm, "_study_sop_uids", lambda *a, **k: None)
    monkeypatch.setattr(cm, "_get_clarus_transfer_syntaxes", lambda *a, **k: [])
    monkeypatch.setattr(cm, "_stream_and_send_study", fake_stream)

    captured = {}

    class _Jobs:
        def finish_job(self, job_id, success=False, error_message=None):
            captured["success"] = success

        def record_scp_success(self, *a, **k):
            pass

        def record_scp_failure(self, *a, **k):
            pass

        def set_job_failed_sops(self, job_id, sops):
            captured["failed_sops"] = sops

        def update_job_progress(self, *a, **k):
            pass

        def is_job_cancelled(self, job_id):
            return False

    cm._execute_cmove_job(
        200,
        STUDY,
        "SCP",
        _Jobs(),
        "http://server/dicomweb",
        5,
        "BRIDGE",
        target_host="127.0.0.1",
        target_port=11112,
        sop_registry={MR: {"accept": True}},
    )

    assert captured["success"] is False, "the job must finish partial"
    assert captured["failed_sops"], (
        "a 0xB000/0xC000 final must carry a non-empty (0008,0058); pass 1 "
        f"OBSERVED {observed} as failed, got {captured['failed_sops']}"
    )
    assert observed[0] in captured["failed_sops"], captured["failed_sops"]
