# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""True-mode Storage Commitment: verdicts come from the ORIGIN's STC resource.

The migration's acceptance checklist is the report's wire-contract table
(analysis/2409_architect_bridge_stc_migration_REPORT.md section 6):

  | Point                  | true-mode                                  |
  | N-ACTION-RSP status    | 0x0000, status-only (no sequences)         |
  | Verdict source         | origin STC resource                        |
  | N-EVENT trigger        | outbox delivered for all, or 300 s         |
  | N-EVENT content        | server's committed / failed(0110/0112)     |
  | 404/405 at origin      | all failed 0x0110 + WARN, no fallback      |
  | 409 duplicate TX       | GET stored result, report it               |
  | Push-model announcement| unchanged                                  |

Each test below pins one row.
"""

import http.server
import json
import socketserver
import threading

from clean_code.commit_tracker import CommitTracker
from clean_code.handlers.naction import handle_n_action
from clean_code.http_helpers import (
    _STC_REASON_MAP,
    STC_NO_RESOURCE,
    _stc_check_request,
    _stc_commit_request,
)

URL = "http://127.0.0.1:1/dicomweb"  # never contacted: the fake origin answers

SOP_CLASS = "1.2.840.10008.5.1.4.1.1.2"
SOP_A = "1.2.840.10008.5.1.4.1.1.2.101"
SOP_B = "1.2.840.10008.5.1.4.1.1.2.102"
# The Transaction UID the modality supplied in its N-ACTION. The bridge
# carries it unchanged into the commit's resource path, so a test can name it.
TXN_UID = "1.2.826.0.1.3680043.10.2509.7001"


def _sop_item(sop_class, sop_uid):
    return {
        "00081150": {"vr": "UI", "Value": [sop_class]},
        "00081155": {"vr": "UI", "Value": [sop_uid]},
    }


def _failed_item(sop_class, sop_uid, reason):
    item = _sop_item(sop_class, sop_uid)
    item["00081197"] = {"vr": "US", "Value": [reason]}
    return item


def test_stc_reason_map_is_total_and_enumerated():
    """Every http->DIMSE Failure Reason maps to a PS3.3 C.14.1.1 value."""
    allowed = {0x0110, 0x0112, 0x0213, 0x0122, 0x0119, 0x0131}
    assert set(_STC_REASON_MAP.values()) <= allowed


def _verdict_body(committed, failed):
    """A Commit/Check 200 body: committed + failed(reason) instance lists."""
    body = {}
    if committed:
        body["00081199"] = {
            "vr": "SQ",
            "Value": [_sop_item(c, u) for c, u in committed],
        }
    if failed:
        body["00081198"] = {
            "vr": "SQ",
            "Value": [_failed_item(c, u, r) for c, u, r in failed],
        }
    return json.dumps(body).encode()


class _FakeOrigin(http.server.BaseHTTPRequestHandler):
    """A committed/failed verdict server for the bridge's POST and GET.

    The class attributes are the script: `post_status`, `get_status` and
    `body`. Every request is counted so a test can assert EXACTLY one POST
    (and zero WADO metadata requests - the bridge must not probe when the
    origin owns the verdict).

    The PATH and BODY of each POST are recorded too, because the promotion
    this mode drives depends on the commit being the standard PS3.18 13.4
    Instance-form request: the Transaction UID in the RESOURCE PATH and the
    referenced SOPs in (0008,1199). Counting POSTs alone would not notice a
    commit sent somewhere else, or sent without the instances.
    """

    post_status = 200
    get_status = 200
    body = b"{}"
    posts = 0
    gets = 0
    metadata_gets = 0
    post_paths = []
    post_bodies = []
    lock = threading.Lock()

    def do_POST(self):  # noqa: N802 (http.server naming)
        length = int(self.headers.get("Content-Length", 0))
        payload = self.rfile.read(length)
        type(self).posts += 1
        type(self).post_paths.append(self.path)
        type(self).post_bodies.append(payload)
        self._answer(type(self).post_status, type(self).body)

    def do_GET(self):  # noqa: N802
        if "/metadata" in self.path:
            type(self).metadata_gets += 1
        else:
            type(self).gets += 1
        self._answer(type(self).get_status, type(self).body)

    def _answer(self, status, body):
        self.send_response(status)
        self.send_header("Content-Type", "application/dicom+json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):  # silence
        pass


class _Origin:
    """A running fake origin with a mutable script."""

    def __init__(self):
        self.server = socketserver.TCPServer(("127.0.0.1", 0), _FakeOrigin)
        self.server.allow_reuse_address = True
        self.port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def url(self):
        return f"http://127.0.0.1:{self.port}/dicomweb"

    def script(self, post_status=200, get_status=200, body=b"{}"):
        _FakeOrigin.post_status = post_status
        _FakeOrigin.get_status = get_status
        _FakeOrigin.body = body
        _FakeOrigin.posts = 0
        _FakeOrigin.gets = 0
        _FakeOrigin.metadata_gets = 0
        _FakeOrigin.post_paths = []
        _FakeOrigin.post_bodies = []

    def counts(self):
        return (_FakeOrigin.posts, _FakeOrigin.gets, _FakeOrigin.metadata_gets)

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class _Outbox:
    """The minimum of OutboxQueue the tracker calls: `delivered`."""

    def __init__(self, delivered):
        self._delivered = set(delivered)

    def delivered(self, sop_uid):
        return sop_uid in self._delivered


# -- The mapping itself ----------------------------------------------------


def test_commit_maps_committed_and_failed_verdicts():
    origin = _Origin()
    try:
        origin.script(
            body=_verdict_body(
                committed=[(SOP_CLASS, SOP_A)],
                failed=[(SOP_CLASS, SOP_B, "0112")],
            )
        )
        committed, failed, sentinel = _stc_commit_request(
            origin.url, "1.2.3", [(SOP_CLASS, SOP_A), (SOP_CLASS, SOP_B)]
        )
        assert sentinel is None
        assert committed == [(SOP_CLASS, SOP_A)]
        assert failed == [(SOP_CLASS, SOP_B, 0x0112)]
    finally:
        origin.close()


def test_commit_reads_failure_reason_as_a_string():
    """DICOM JSON renders numeric VRs as strings; 0110/0213 map too."""
    origin = _Origin()
    try:
        origin.script(
            body=_verdict_body(
                committed=[],
                failed=[
                    (SOP_CLASS, SOP_A, "0110"),
                    (SOP_CLASS, SOP_B, "0213"),
                ],
            )
        )
        _c, failed, _s = _stc_commit_request(origin.url, "1.2.3", [])
        assert (SOP_CLASS, SOP_A, 0x0110) in failed
        assert (SOP_CLASS, SOP_B, 0x0213) in failed
    finally:
        origin.close()


def test_an_unenumerated_reason_is_reported_as_processing_failure():
    """Never invent a code: anything outside C.14.1.1 becomes 0x0110."""
    origin = _Origin()
    try:
        origin.script(
            body=_verdict_body(committed=[], failed=[(SOP_CLASS, SOP_A, "C2FF")])
        )
        _c, failed, _s = _stc_commit_request(origin.url, "1.2.3", [])
        assert failed == [(SOP_CLASS, SOP_A, 0x0110)]
    finally:
        origin.close()


def test_a_sop_absent_from_both_sequences_is_failed_0110():
    origin = _Origin()
    try:
        origin.script(body=_verdict_body(committed=[(SOP_CLASS, SOP_A)], failed=[]))
        _c, failed, _s = _stc_commit_request(
            origin.url, "1.2.3", [(SOP_CLASS, SOP_A), (SOP_CLASS, SOP_B)]
        )
        assert failed == [(SOP_CLASS, SOP_B, 0x0110)]
    finally:
        origin.close()


# -- Transport outcomes ----------------------------------------------------


def test_a_404_origin_reports_every_sop_failed_and_no_resource():
    """T3: no STC resource -> all failed 0x0110 + the sentinel, no fallback."""
    origin = _Origin()
    try:
        origin.script(post_status=404)
        committed, failed, sentinel = _stc_commit_request(
            origin.url, "1.2.3", [(SOP_CLASS, SOP_A), (SOP_CLASS, SOP_B)]
        )
        assert committed == []
        assert failed == [
            (SOP_CLASS, SOP_A, 0x0110),
            (SOP_CLASS, SOP_B, 0x0110),
        ]
        assert sentinel == STC_NO_RESOURCE
        # Exactly one POST: no retry loop on a 4xx, no local probe.
        assert origin.counts()[0] == 1
    finally:
        origin.close()


def test_a_405_origin_is_the_same_as_404():
    origin = _Origin()
    try:
        origin.script(post_status=405)
        _c, failed, sentinel = _stc_commit_request(
            origin.url, "1.2.3", [(SOP_CLASS, SOP_A)]
        )
        assert sentinel == STC_NO_RESOURCE
        assert failed == [(SOP_CLASS, SOP_A, 0x0110)]
    finally:
        origin.close()


def test_a_409_replays_the_stored_verdict_over_one_get():
    """T6: the POST is refused as a duplicate; the stored result is reported."""
    origin = _Origin()
    try:
        origin.script(
            post_status=409,
            get_status=200,
            body=_verdict_body(
                committed=[(SOP_CLASS, SOP_A)],
                failed=[(SOP_CLASS, SOP_B, "0112")],
            ),
        )
        committed, failed, sentinel = _stc_commit_request(
            origin.url, "1.2.3", [(SOP_CLASS, SOP_A), (SOP_CLASS, SOP_B)]
        )
        assert sentinel is None
        assert committed == [(SOP_CLASS, SOP_A)]
        assert failed == [(SOP_CLASS, SOP_B, 0x0112)]
        posts, gets, _meta = origin.counts()
        assert posts == 1
        assert gets == 1  # the single replay GET
    finally:
        origin.close()


def test_a_409_whose_result_was_swept_fails_everything():
    origin = _Origin()
    try:
        origin.script(post_status=409, get_status=404)
        _c, failed, sentinel = _stc_commit_request(
            origin.url, "1.2.3", [(SOP_CLASS, SOP_A)]
        )
        assert sentinel is None
        assert failed == [(SOP_CLASS, SOP_A, 0x0110)]
    finally:
        origin.close()


def test_a_400_reports_every_sop_failed_without_retrying():
    origin = _Origin()
    try:
        origin.script(post_status=400)
        _c, failed, sentinel = _stc_commit_request(
            origin.url, "1.2.3", [(SOP_CLASS, SOP_A)]
        )
        assert failed == [(SOP_CLASS, SOP_A, 0x0110)]
        assert sentinel is None
        assert origin.counts()[0] == 1  # a client error is an answer, not weather
    finally:
        origin.close()


def test_a_check_404_reports_every_sop_failed():
    origin = _Origin()
    try:
        origin.script(get_status=404)
        _c, failed, _s = _stc_check_request(origin.url, "1.2.3", [(SOP_CLASS, SOP_A)])
        assert failed == [(SOP_CLASS, SOP_A, 0x0110)]
    finally:
        origin.close()


# -- T5: the deadline path -------------------------------------------------


def test_a_straggler_at_the_deadline_is_reported_with_the_origin_verdict():
    """T5: one SOP never delivered; the clock passes 300 s; the POST fires.

    The origin answers 0112 for the straggler, and the N-EVENT reports it.
    The 0x0111 unconfirmed reason must NOT appear anywhere - true-mode has no
    such code.
    """
    origin = _Origin()
    try:
        origin.script(
            body=_verdict_body(
                committed=[(SOP_CLASS, SOP_A)],
                failed=[(SOP_CLASS, SOP_B, "0112")],
            )
        )
        events = []
        now = [1000.0]
        outbox = _Outbox({SOP_A})  # SOP_B never delivered

        tracker = CommitTracker(
            verify=lambda c, u: outbox.delivered(u),
            report=lambda *a, **k: None,
            resolve=lambda job: events.append(_resolve(job, origin.url)),
            interval=0.1,
            deadline_sec=300.0,
            now=lambda: now[0],
        )
        tracker.register(
            "1.2.3",
            "TEST_SCU",
            "127.0.0.1",
            11112,
            [(SOP_CLASS, SOP_A), (SOP_CLASS, SOP_B)],
        )

        tracker._tick()
        assert events == [], "not delivered yet: no verdict asked for"

        now[0] += 301.0  # the deadline
        tracker._tick()
        assert len(events) == 1
        committed, failed = events[0]
        assert committed == [(SOP_CLASS, SOP_A)]
        assert failed == [(SOP_CLASS, SOP_B, 0x0112)]
        assert all(reason != 0x0111 for _c, _u, reason in failed)
    finally:
        origin.close()


def _resolve(job, url):
    committed, failed, _sentinel = _stc_commit_request(url, job.tx_uid, job.sops)
    return committed, failed


# -- The knob --------------------------------------------------------------


def test_the_knob_defaults_to_false_and_reads_bool_conventions():
    """T4: absent -> false; true/on/1/yes -> true; garbage -> false."""
    import os
    import tempfile

    from clean_code.config import ConfParser

    cases = [
        ("", False),
        ("storage_commitment_presence = true\n", True),
        ("storage_commitment_presence = on\n", True),
        ("storage_commitment_presence = 1\n", True),
        ("storage_commitment_presence = yes\n", True),
        ("storage_commitment_presence = garbage\n", False),
        ("storage_commitment_presence = false\n", False),
    ]
    for body, expected in cases:
        fd, path = tempfile.mkstemp(suffix=".conf")
        with os.fdopen(fd, "w") as fh:
            fh.write("[dicomweb]\nclarus_url = http://127.0.0.1:8024/dicomweb\n" + body)
        try:
            cfg = ConfParser(path)
            assert (
                cfg.get_bool("dicomweb", "storage_commitment_presence", default=False)
                is expected
            ), body
        finally:
            os.unlink(path)


# -- The handler's true-mode shape -----------------------------------------


def _naction_event(sop_list):
    """A stand-in for pynetdicom's N-ACTION request event.

    The request payload is exposed as `action_information`, which is the real
    event property for an N-ACTION's Action Information (PS3.7 9.3.2): the
    generic `dataset` property belongs to C-STORE and reads the `DataSet`
    parameter, which an N-ACTION-RQ does not carry. A fixture built on
    `.dataset` therefore asserts a signal the wire never delivers.
    """
    from pydicom import Dataset

    ds = Dataset()
    ds.TransactionUID = "1.2.3.4.5"
    ref = []
    for sop_class, sop_uid in sop_list:
        item = Dataset()
        item.ReferencedSOPClassUID = sop_class
        item.ReferencedSOPInstanceUID = sop_uid
        ref.append(item)
    ds.ReferencedSOPSequence = ref

    Req = type("Req", (), {"ae_title": "TEST_SCU", "address": "10.0.0.5", "port": 11112})
    Assoc = type("Assoc", (), {"requestor": Req()})

    class _Event:
        # No `dataset`: accessing it on a real N-ACTION event raises
        # AttributeError, and the parser must fall back to the real property.
        def __init__(self):
            self.action_information = ds
            self.assoc = Assoc()
            self.action_type = 1

    return _Event()


def test_true_mode_answers_a_status_only_rsp_and_probes_nothing():
    """T1 (handler half): no sequence in the RSP, no WADO request at all."""
    from unittest.mock import patch

    origin = _Origin()
    try:
        origin.script(
            body=_verdict_body(
                committed=[(SOP_CLASS, SOP_A)], failed=[(SOP_CLASS, SOP_B, "0112")]
            )
        )
        events = []
        tracker = CommitTracker(
            verify=lambda c, u: True,
            report=lambda *a, **k: None,
            resolve=lambda job: events.append(_resolve(job, origin.url)),
        )
        sop_index = _NoIndex()
        with patch("clean_code.handlers.naction._shutdown_requested") as shutdown:
            shutdown.is_set.return_value = False
            status, rsp = handle_n_action(
                _naction_event([(SOP_CLASS, SOP_A), (SOP_CLASS, SOP_B)]),
                origin.url,
                False,
                sop_index,
                commit_tracker=tracker,
                stc_presence=True,
                # PS3.8: the listening port comes from the peer config, never
                # from the association's ephemeral source port.
                resolve_scp=lambda ae, addr: ("10.0.0.5", 11112),
            )
        assert status == 0x0000
        assert rsp is None, "true-mode RSP is status-only"
        assert origin.counts() == (0, 0, 0), "nothing asked until readiness"
        assert tracker.pending_count() == 1

        tracker._tick()
        assert len(events) == 1
        committed, failed = events[0]
        assert committed == [(SOP_CLASS, SOP_A)]
        assert failed == [(SOP_CLASS, SOP_B, 0x0112)]
        posts, _gets, metadata = origin.counts()
        assert posts == 1, "exactly ONE POST for the whole batch"
        assert metadata == 0, "no WADO metadata probe in true-mode"
    finally:
        origin.close()


class _NoIndex:
    """A sop_index that fails loudly if the true-mode path touches it."""

    def lookup_sop(self, sop_uid):
        raise AssertionError("true-mode must not consult the local SOP index")


def test_the_commit_is_the_standard_instance_form_request_in_the_path():
    """T10 (wire half): the POST is PS3.18 13.4 with the Transaction UID in
    the RESOURCE PATH and the SOPs in (0008,1199).

    Clarus advances Input Readiness on this request, so its shape is load
    bearing rather than cosmetic: a commit that named the transaction only in
    the body, or carried no instance list, would be a different resource and
    would promote nothing.
    """
    origin = _Origin()
    try:
        origin.script(body=_verdict_body(committed=[(SOP_CLASS, SOP_A)], failed=[]))
        events = []
        tracker = CommitTracker(
            verify=lambda c, u: True,
            report=lambda *a, **k: None,
            resolve=lambda job: events.append(_resolve(job, origin.url)),
        )
        tracker.register(TXN_UID, "TEST_SCU", "127.0.0.1", 11112, [(SOP_CLASS, SOP_A)])
        tracker._tick()
        assert len(events) == 1

        assert len(_FakeOrigin.post_paths) == 1, "exactly one commit POST"
        path = _FakeOrigin.post_paths[0]
        assert (
            path == f"/dicomweb/commitment-requests/{TXN_UID}"
        ), f"the Transaction UID belongs in the resource path, got {path!r}"

        # The body is the Instance form: Referenced SOP Sequence (0008,1199),
        # each item naming the class and the instance.
        body = json.loads(_FakeOrigin.post_bodies[0])
        assert "00081199" in body, "the commit carries a Referenced SOP Sequence"
        items = body["00081199"]["Value"]
        assert [i["00081155"]["Value"][0] for i in items] == [SOP_A]
        assert [i["00081150"]["Value"][0] for i in items] == [SOP_CLASS]
        assert "00081110" not in body, "the Instance form carries no study sequence"
    finally:
        origin.close()


def test_the_commit_waits_for_every_sop_of_the_batch_to_be_delivered():
    """T11: the POST fires only once `outbox.delivered` is true for ALL.

    Clarus's coverage rule reads the commit as "I sent everything", so a
    commit sent before the sender's bytes have arrived would be a claim the
    archive cannot yet honour. This pins the ORDERING, which is the half the
    promotion depends on: while one SOP is still undelivered the bridge asks
    the origin nothing, and the commit only travels once the last one lands.
    """
    origin = _Origin()
    try:
        origin.script(
            body=_verdict_body(
                committed=[(SOP_CLASS, SOP_A), (SOP_CLASS, SOP_B)], failed=[]
            )
        )
        events = []
        delivered = {SOP_A}
        tracker = CommitTracker(
            verify=lambda c, u: u in delivered,
            report=lambda *a, **k: None,
            resolve=lambda job: events.append(_resolve(job, origin.url)),
        )
        tracker.register(
            TXN_UID,
            "TEST_SCU",
            "127.0.0.1",
            11112,
            [(SOP_CLASS, SOP_A), (SOP_CLASS, SOP_B)],
        )

        tracker._tick()
        assert origin.counts() == (0, 0, 0), "nothing asked while a SOP is pending"
        assert events == []

        # The second SOP lands: now the whole batch is delivered, so the very
        # next tick commits.
        delivered.add(SOP_B)
        tracker._tick()
        assert origin.counts()[0] == 1, "the commit travels once the batch is delivered"
        assert len(events) == 1
        committed, _failed = events[0]
        assert committed == [(SOP_CLASS, SOP_A), (SOP_CLASS, SOP_B)]
    finally:
        origin.close()
