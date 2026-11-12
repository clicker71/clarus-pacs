# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""0909 Rule C - worker resume (C1/C1b): after an association loss the
employee re-associates EXACTLY ONCE, runs a resumed second pass that skips
already-delivered SOP UIDs, reconciles counts by union, and on a second loss
(after resume) hands the undelivered tail into (0008,0058).

These drive `_execute_cmove_job` (the resume own code added here) by
monkeypatching the network legs `_stream_and_send_study` / `_associate_for_move`
/ `_study_sop_uids`; the real socket-loss streaming path is pynetdicom and is
separately exercised. Counters and dups are checked.
"""

from unittest.mock import Mock

import clean_code.handlers.cmove as cm
import pytest

STUDY = "2.25.111122223333"
MR = "1.2.840.10008.5.1.4.1.1.4"


def _mk_stub(ops):
    """A minimal MoveJobManager stand-in exposing only what execute uses."""

    class S:
        finished = {}
        failed_sops = None

        def finish_job(self, job_id, success=False, error_message=None):
            ops["finished"] = (job_id, success, error_message)

        def record_scp_success(self, *a, **k):
            ops.setdefault("scp_ok", 0)
            ops["scp_ok"] += 1

        def record_scp_failure(self, *a, **k):
            ops.setdefault("scp_fail", 0)
            ops["scp_fail"] += 1

        def set_job_failed_sops(self, job_id, sops):
            ops["failed_sops"] = sops

        def update_job_progress(self, *a, **k):
            pass

        def is_job_cancelled(self, job_id):
            return False

    return S()


def test_resume_reassociates_once_and_delivers_each_once(monkeypatch):
    """Pass 1 delivers 3/5 and the association is lost -> the driver
    re-associates exactly ONCE, the resumed pass skips the 3 delivered and
    delivers the remaining 2 (each instance exactly once, no dups)."""
    calls = {"assoc": 0, "passes": []}
    sop_all = ["1.2.3.4.5.6.7.8.9.10.%d" % i for i in (1, 2, 3, 4, 5)]

    def fake_associate(*a, **k):
        calls["assoc"] += 1

        class A:
            is_established = True
            accepted_contexts = []

        return A(), {}

    def fake_preflight(*a, **k):
        return {}

    def fake_sops(study, url, *a, **k):
        return list(sop_all)

    def fake_caps(*a, **k):
        return []

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
        # Simulate: initial pass sends a prefix (skip none); resumed pass
        # sends the rest, skipping _skip_ids.
        calls["passes"].append(_skip_ids)
        if not _resumed:
            # deliver first three, then association lost on #4
            delivered = sop_all[:3]
            sent = [(MR, u) for u in delivered]
            return (3, 1, 4, sent, [], False, False, True)  # lost=True
        else:
            # resumed pass: skip delivered prefix, deliver the remaining two
            skip = set(_skip_ids or [])
            to_send = [u for u in sop_all if u not in skip]
            sent = [(MR, u) for u in to_send]
            return (len(to_send), 0, len(sop_all), sent, [], False, False, False)

    ops = {}
    stub = _mk_stub(ops)
    monkeypatch.setattr(cm, "_associate_for_move", fake_associate)
    monkeypatch.setattr(cm, "_preflight_study_ts_by_class", fake_preflight)
    monkeypatch.setattr(cm, "_study_sop_uids", fake_sops)
    monkeypatch.setattr(cm, "_get_clarus_transfer_syntaxes", fake_caps)
    monkeypatch.setattr(cm, "_stream_and_send_study", fake_stream)
    monkeypatch.setattr(
        cm,
        "move_jobs_counter",
        type(
            "C",
            (),
            {"labels": lambda *a, **k: type("L", (), {"inc": lambda *a, **k: None})()},
        )(),
    )

    cm._execute_cmove_job(
        99,
        STUDY,
        "SCP",
        stub,
        "http://server/dicomweb",
        5,
        "BRIDGE",
        target_host="127.0.0.1",
        target_port=11112,
        sop_registry={MR: {"accept": True}},
    )
    assert (
        calls["assoc"] == 2
    ), f"expected exactly one resume reassociate, got {calls['assoc']}"
    # pass1 skip=None, pass2 skip = the 3 delivered prefix
    assert calls["passes"] == [None, set(sop_all[:3])], calls["passes"]
    # final: union delivered = all 5 -> job success; failed tail empty
    assert ops["finished"][1] is True, ops["finished"]
    assert ops.get("failed_sops") == [], ops.get("failed_sops")


def test_resume_second_loss_gives_honest_partial_with_tail(monkeypatch):
    """Association is lost AND the resumed pass is lost again -> GIVE_UP: the
    job finishes partial and the (0008,0058) tail carries the undelivered SOP
    UIDs (preflight list minus delivered across both passes)."""
    calls = {"assoc": 0}
    sop_all = ["1.2.3.4.5.6.7.8.9.10.%d" % i for i in (1, 2, 3, 4, 5)]

    def fake_associate(*a, **k):
        calls["assoc"] += 1
        return type("A", (), {"is_established": True, "accepted_contexts": []})(), {}

    def fake_preflight(*a, **k):
        return {}

    def fake_sops(study, url, *a, **k):
        return list(sop_all)

    def fake_caps(*a, **k):
        return []

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
            sent = [(MR, sop_all[0])]
            return (1, 1, 4, sent, [], False, False, True)  # lost after 1
        skip = set(_skip_ids or [])
        to_send = [u for u in sop_all if u not in skip]
        # deliver one more then lose again
        extra = to_send[:1]
        sent = [(MR, u) for u in extra]
        return (1, 1, len(sop_all), sent, [], False, False, True)  # lost again

    ops = {}
    stub = _mk_stub(ops)
    monkeypatch.setattr(cm, "_associate_for_move", fake_associate)
    monkeypatch.setattr(cm, "_preflight_study_ts_by_class", fake_preflight)
    monkeypatch.setattr(cm, "_study_sop_uids", fake_sops)
    monkeypatch.setattr(cm, "_get_clarus_transfer_syntaxes", fake_caps)
    monkeypatch.setattr(cm, "_stream_and_send_study", fake_stream)
    monkeypatch.setattr(
        cm,
        "move_jobs_counter",
        type(
            "C",
            (),
            {"labels": lambda *a, **k: type("L", (), {"inc": lambda *a, **k: None})()},
        )(),
    )

    cm._execute_cmove_job(
        100,
        STUDY,
        "SCP",
        stub,
        "http://server/dicomweb",
        5,
        "BRIDGE",
        target_host="127.0.0.1",
        target_port=11112,
        sop_registry={MR: {"accept": True}},
    )
    assert (
        calls["assoc"] == 2
    ), f"exactly two associates total (initial+resume), got {calls['assoc']}"
    # delivered = {1st, ...first of remaining} = sop_all[0], sop_all[1]
    delivered = {sop_all[0], sop_all[1]}
    tail = [u for u in sop_all if u not in delivered]
    # Job finished FAILED/partial; the (0008,0058) tail = undelivered.
    assert ops["finished"][1] is False, ops["finished"]
    assert sorted(ops.get("failed_sops") or []) == sorted(tail), ops.get("failed_sops")


def test_preflight_failure_releases_peer_gate(monkeypatch):
    """A raise from the pre-flight (before any association) must still release
    the peer gate: the gate is taken inside the try whose finally releases it.

    The pre-flight is patched with a Mock whose side_effect raises, so the
    test proves the real function was reached (assert_called_once) AND that
    the gate was released on the exception path. With the gate taken before
    the try the raise leaves the peer in _ASSOC_BUSY for every later job to
    that peer - assert_called_once still passes but the not-busy assert
    fails, so the test is not vacuous.
    """

    ops = {}
    stub = _mk_stub(ops)
    key = ("127.0.0.1", 11112, "SCP")
    preflight = Mock(side_effect=RuntimeError("boom"))

    monkeypatch.setattr(cm, "_preflight_study_ts_by_class", preflight)
    cm._ASSOC_BUSY.pop(key, None)

    # Direct call: through `_cmove_worker` the except Exception would swallow
    # the raise and tell us nothing about the gate.
    with pytest.raises(RuntimeError, match="boom"):
        cm._execute_cmove_job(
            101,
            STUDY,
            "SCP",
            stub,
            "http://server/dicomweb",
            5,
            "BRIDGE",
            target_host="127.0.0.1",
            target_port=11112,
            sop_registry={MR: {"accept": True}},
        )

    preflight.assert_called_once()
    assert key not in cm._ASSOC_BUSY, "peer left busy after a pre-flight raise"
