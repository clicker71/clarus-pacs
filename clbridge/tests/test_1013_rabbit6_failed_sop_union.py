# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""1013 rabbit pass 6, B4 - a DELIVERED instance landed in (0008,0058).

With no authoritative study SOP list the final failed set was built from the
per-pass `seen minus delivered` sets. Each set only knows what ITS OWN pass
delivered, so an instance lost in pass 1 and delivered in pass 2 stayed in
`_fs1` - a complete transfer then answered 0xB000 with the delivered UID named
as failed, and Storage Commitment was skipped. PS3.4 C.4.2.1.4.2 allows the
list only for instances that failed.
"""

from clean_code.handlers import cmove as cm

STUDY = "2.25.111122223333.9999"
MR = "1.2.840.10008.5.1.4.1.1.4"
X = "1.2.3.4.5.6.7.8.9.10.1"
Y = "1.2.3.4.5.6.7.8.9.10.2"
Z = "1.2.3.4.5.6.7.8.9.10.3"


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


class _Jobs:
    """Records the terminal outcome; optionally counts Storage Commitment."""

    def __init__(self):
        self.captured = {}

    def finish_job(self, job_id, success=False, error_message=None):
        self.captured["success"] = success
        self.captured["error_message"] = error_message
        return True

    def record_scp_success(self, *a, **k):
        pass

    def record_scp_failure(self, *a, **k):
        pass

    def set_job_failed_sops(self, job_id, sops):
        self.captured["failed_sops"] = sops

    def update_job_progress(self, *a, **k):
        pass

    def is_job_cancelled(self, job_id):
        return False


def _assoc():
    return type("A", (), {"is_established": True, "accepted_contexts": []})()


def _wire(monkeypatch, passes):
    """Install a fake `_stream_and_send_study` returning `passes` in order."""
    calls = {"n": 0}

    def fake_stream(*a, **k):
        result = passes[calls["n"]]
        calls["n"] += 1
        return result

    monkeypatch.setattr(cm, "_associate_for_move", lambda *a, **k: (_assoc(), {}))
    monkeypatch.setattr(cm, "_preflight_study_ts_by_class", lambda *a, **k: {})
    monkeypatch.setattr(cm, "_study_sop_uids", lambda *a, **k: None)
    monkeypatch.setattr(cm, "_get_clarus_transfer_syntaxes", lambda *a, **k: [])
    monkeypatch.setattr(cm, "_stream_and_send_study", fake_stream)
    return calls


def _run(monkeypatch, passes, jobs=None):
    jobs = jobs or _Jobs()
    calls = _wire(monkeypatch, passes)
    cm._execute_cmove_job(
        300,
        STUDY,
        "SCP",
        jobs,
        "http://server/dicomweb",
        5,
        "BRIDGE",
        target_host="127.0.0.1",
        target_port=11112,
        sop_registry={MR: {"accept": True}},
    )
    return jobs.captured, calls


class TestB4DeliveredIsNotFailed:
    def test_instance_delivered_in_pass_2_is_not_reported_failed(self, monkeypatch):
        """Reviewer sequence 1: pass 1 loses the association after queueing X,
        pass 2 delivers X and completes. Every instance is delivered, so the
        final must be a success with an EMPTY (0008,0058).

        Return tuple: (completed, failed, total, sent, seen-minus-delivered,
        cancelled, no_common, lost).
        """
        _stub_counters(monkeypatch)
        # Pass 1 streams X and Y, then loses the association before Y's
        # C-STORE-RSP: X is queued-and-unconfirmed, so it is in pass 1's
        # failed set. Pass 2 delivers both. Nothing is left undelivered.
        pass1 = (0, 0, 2, [], [X], False, False, True)
        pass2 = (2, 0, 2, [(MR, X), (MR, Y)], [], False, False, False)
        captured, calls = _run(monkeypatch, [pass1, pass2])

        assert calls["n"] == 2, "the resume pass must run"
        assert captured["success"] is True, captured
        assert captured["failed_sops"] == [], (
            "X was DELIVERED in pass 2; naming it in (0008,0058) turns a "
            f"complete transfer into 0xB000: {captured['failed_sops']}"
        )

    def test_instance_delivered_in_pass_1_and_skipped_in_pass_2_is_not_failed(
        self, monkeypatch
    ):
        """Reviewer sequence 2: X is delivered in pass 1 and appears in pass
        2's failed set because pass 2 skipped it (a skipped instance is not in
        pass 2's sent list). It is archived, so it is not a failure."""
        _stub_counters(monkeypatch)
        pass1 = (1, 0, 2, [(MR, X)], [Y], False, False, True)
        pass2 = (1, 0, 2, [(MR, Y)], [X], False, False, False)
        captured, calls = _run(monkeypatch, [pass1, pass2])

        assert calls["n"] == 2
        assert captured["success"] is True, captured
        assert captured["failed_sops"] == [], captured["failed_sops"]

    def test_a_genuinely_lost_instance_is_still_named(self, monkeypatch):
        """The fix subtracts only what was DELIVERED: a UID nobody delivered
        stays in the list, so the subtraction cannot hide a real loss."""
        _stub_counters(monkeypatch)
        pass1 = (0, 0, 3, [], [X], False, False, True)
        pass2 = (1, 0, 3, [(MR, Y)], [X, Z], False, False, True)
        captured, calls = _run(monkeypatch, [pass1, pass2])

        assert calls["n"] == 2
        assert captured["success"] is False, captured
        assert captured["failed_sops"] == [X, Z], captured["failed_sops"]

    def test_both_passes_lost_still_keeps_the_observed_list(self, monkeypatch):
        """Guard in the other direction: when nothing was delivered the union
        of both passes' observed failures is still what gets reported."""
        _stub_counters(monkeypatch)
        pass1 = (0, 0, 3, [], [X], False, False, True)
        pass2 = (0, 0, 3, [], [X, Y], False, False, True)
        captured, calls = _run(monkeypatch, [pass1, pass2])

        assert captured["success"] is False
        assert captured["failed_sops"] == [X, Y], captured["failed_sops"]


class TestB4UnparsedTailFloor:
    def test_unparsed_tail_floor_still_fires(self, monkeypatch):
        """Independent defect, unchanged: a resume that failed with NOTHING
        observed (an instance whose SOP UID never parsed) still reports at
        least one undelivered instance rather than 0x0000."""
        _stub_counters(monkeypatch)
        pass1 = (0, 0, 0, [], [], False, False, True)
        pass2 = (0, 0, 0, [], [], False, False, True)
        captured, calls = _run(monkeypatch, [pass1, pass2])

        assert (
            captured["success"] is False
        ), "resumed_lost with no observed failure must not answer 0x0000"
        assert "0 failed" not in (captured["error_message"] or ""), captured
