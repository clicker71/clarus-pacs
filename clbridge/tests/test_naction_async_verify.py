# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""M9: verification happens on the tracker's tick, not inside N-ACTION.

PS3.4 J.3.2: a successful N-ACTION-RSP says only that the SCP RECEIVED the
request. The per-SOP verdict travels in the N-EVENT-REPORT (J.3.3.1).

The handler used to confirm every referenced SOP with its own WADO round-trip
BEFORE answering, which held the DIMSE association open for one request per
instance - tens of minutes for a large batch, waiting for an acknowledgement
that carries nothing. These tests pin the replacement: the response is
immediate and empty, and the tracker still reaches the right verdict later.
"""

from unittest.mock import patch

from clean_code.commit_tracker import CommitTracker
from clean_code.handlers.naction import handle_n_action
from pydicom.uid import generate_uid

from tests.test_naction_handler import URL, make_fake_naction_event

SOP_CLASS = "1.2.840.10008.5.1.4.1.1.2"


class _Tracker:
    """Stands in for the real tracker: records what the handler registers."""

    def __init__(self):
        self.registered = []

    def register(self, tx_uid, calling_ae, host, port, sops):
        self.registered.append((tx_uid, calling_ae, host, port, list(sops)))


def _event(sop_list, calling_ae="TEST_SCU", address="10.0.0.5", port=11112):
    event = make_fake_naction_event(
        tx_uid=generate_uid(), sop_list=sop_list, calling_ae=calling_ae
    )
    Req = type("Req", (), {"ae_title": calling_ae, "address": address, "port": port})
    Assoc = type("Assoc", (), {"requestor": Req()})
    event.assoc = Assoc()
    return event


def test_the_response_is_immediate_and_carries_no_verdict():
    """A 20-SOP batch is acknowledged without a single WADO round-trip."""
    from clean_code.sop_index import SopIndexManager

    sops = [(SOP_CLASS, f"1.2.3.4.5.{i}") for i in range(20)]
    tracker = _Tracker()
    # A REAL index, so restoring the synchronous path cannot pass by blowing
    # up on a missing object instead of by making a WADO call. The SOPs are
    # IN the index so the old path would get past `lookup_sop` and reach the
    # request this test forbids.
    index = SopIndexManager(index_dir=str(_tmp_index_dir()))
    for _sop_class, sop_uid in sops:
        index.insert_sop(sop_uid, "1.2.3", "1.2.3.4")

    with patch("clean_code.handlers.naction._wado_metadata_request") as wado:
        status, rsp = handle_n_action(
            _event(sops),
            URL,
            False,
            sop_index=index,
            commit_tracker=tracker,
            resolve_scp=lambda ae, addr: ("10.0.0.5", 11112),
        )

    assert status == 0x0000, "receipt is acknowledged"
    assert rsp is None, "and nothing else: the RSP carries no per-SOP verdict"
    assert not wado.called, (
        "the handler must not probe the origin; that is what made a large "
        "batch hold its association for tens of minutes"
    )
    assert len(tracker.registered) == 1
    assert tracker.registered[0][4] == sops, "the whole batch is registered"


def _tmp_index_dir():
    import tempfile

    path = tempfile.mkdtemp(prefix="clinfer-naction-index-")
    return path


def test_the_tracker_still_reaches_the_right_verdict_later():
    """The verdict M9 moved out of the handler is still delivered.

    One SOP is stored, one is not. The tracker's own tick turns that into a
    complete-with-failures report, so the modality learns the same thing it
    used to read in the RSP - only later, and through the primitive the
    standard says carries it.
    """
    stored = "1.2.3.4.5.100"
    missing = "1.2.3.4.5.101"
    reports = []

    tracker = CommitTracker(
        verify=lambda sop_class, sop_uid: sop_uid == stored,
        report=lambda host, port, ae, tx, event_type, ok, failed: reports.append(
            (event_type, ok, failed)
        ),
        interval=0.1,
        deadline_sec=300.0,
    )
    tracker.register(
        "1.2.3.4.5.999",
        "TEST_SCU",
        "10.0.0.5",
        11112,
        [(SOP_CLASS, stored), (SOP_CLASS, missing)],
    )

    # Not yet complete: the missing SOP keeps the batch open.
    tracker._tick()
    assert tracker.pending_count() == 1
    assert reports == []

    # Deadline passes: the unconfirmed SOP is reported as a failure.
    for job in list(tracker._jobs.values()):
        job.deadline = 0.0
    tracker._tick()
    tracker.close()

    assert tracker.pending_count() == 0
    assert len(reports) == 1
    event_type, ok, failed = reports[0]
    assert event_type == 2, "complete with failures"
    assert ok == [(SOP_CLASS, stored)]
    assert [item[1] for item in failed] == [missing]
    assert failed[0][2] == 0x0110, "0110H Processing Failure (PS3.3 C.14.1.1)"
