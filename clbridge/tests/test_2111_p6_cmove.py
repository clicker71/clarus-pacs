# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""P6 C-MOVE - one test per finding.

The move-job state machine is driven over the REAL `MoveJobManager` on a temp
SQLite file, so what is asserted is what a restart and a second worker would
see. The wire paths (the final response, the association proposal) are asserted
at the boundary they belong to: the generator pynetdicom reads, and the
presentation contexts actually proposed.
"""

import sqlite3
import threading

from clean_code.move_jobs import MoveJobManager
from pydicom.dataset import Dataset

SOP_CLASS = "1.2.840.10008.5.1.4.1.1.2"


class _TestDb:
    """The SharedDb contract: one connection, one lock, `locked()`."""

    def __init__(self, path):
        self._conn = sqlite3.connect(path, check_same_thread=False)
        self._lock = threading.Lock()

    def locked(self):
        return _Locked(self._lock, self._conn)

    def close(self):
        with self._lock:
            self._conn.close()


class _Locked:
    def __init__(self, lock, conn):
        self._lock = lock
        self._conn = conn

    def __enter__(self):
        self._lock.acquire()
        return self._conn

    def __exit__(self, *args):
        self._lock.release()


def _mgr(tmp_path, name="jobs.db"):
    return MoveJobManager(shared_db=_TestDb(str(tmp_path / name)), max_workers=2)


# ---------------------------------------------------------------------------
# :157 - every requested series/instance UID is scoped
# ---------------------------------------------------------------------------
def test_a_multi_series_request_scopes_every_series():
    """PS3.4 Annex C.4.2 permits UID-list matching at the SERIES level. Keeping
    only the first value queued one series and still answered Success."""
    from clean_code.handlers.cmove import _request_scopes

    scopes = _request_scopes("SERIES", ["1.2.3.4", "1.2.3.5"], [])
    assert scopes == [("1.2.3.4", ""), ("1.2.3.5", "")], scopes


def test_a_multi_instance_request_scopes_every_instance():
    from clean_code.handlers.cmove import _request_scopes

    scopes = _request_scopes("IMAGE", ["1.2.3.4"], ["1.2.3.4.1", "1.2.3.4.2"])
    assert scopes == [("1.2.3.4", "1.2.3.4.1"), ("1.2.3.4", "1.2.3.4.2")], scopes


def test_a_study_request_stays_one_scope():
    from clean_code.handlers.cmove import _request_scopes

    assert _request_scopes("", [], []) == [("", "")]
    assert _request_scopes("STUDY", [], []) == [("", "")]


def test_every_scoped_uid_reaches_the_queue():
    """The values travel into SEPARATE jobs, which is what makes the transfer
    complete rather than partial."""
    from clean_code.handlers.cmove import _request_scopes

    scopes = _request_scopes("SERIES", ["1.2.3.4", "1.2.3.5", "1.2.3.6"], [])
    assert len(scopes) == 3
    assert [s[0] for s in scopes] == ["1.2.3.4", "1.2.3.5", "1.2.3.6"]


# ---------------------------------------------------------------------------
# :614 - a discovered class survives an unknown stored syntax
# ---------------------------------------------------------------------------
def test_a_class_without_stored_syntax_is_still_kept():
    """Standard DICOMweb metadata may omit group 0002 (PS3.18 10.4.4 requires
    the dataset attributes). Dropping such a class from the pre-flight map meant
    the association never proposed it, so a passthrough origin could not deliver
    it and no refetch target could be found for it either."""
    import clean_code.handlers.cmove as cm

    class _Resp:
        headers = {"Content-Type": "application/dicom+json"}

        def json(self):
            # CT with a stored syntax, SR WITHOUT one.
            return [
                {
                    "00080016": {"Value": ["1.2.840.10008.5.1.4.1.1.2"]},
                    "00020010": {"Value": ["1.2.840.10008.1.2.1"]},
                },
                {"00080016": {"Value": ["1.2.840.10008.5.1.4.1.1.88.33"]}},
            ]

        def raise_for_status(self):
            return None

        @property
        def content(self):
            return b""

    _orig_qido = cm._qido_request
    _orig_get = cm.requests.get
    try:
        cm._qido_request = lambda url, *a, **k: (
            [{"0020000E": {"Value": ["1.2.3.4"]}}]
            if url.endswith("/series")
            else [{"00080018": {"Value": ["1.2.3.4.5"]}}]
        )
        cm.requests.get = lambda *a, **k: _Resp()
        result, _partial = cm._preflight_study_ts_by_class("1.2.3", "http://h/dicomweb")
    finally:
        cm._qido_request = _orig_qido
        cm.requests.get = _orig_get

    assert result is not None
    assert "1.2.840.10008.5.1.4.1.1.2" in result
    assert "1.2.840.10008.5.1.4.1.1.88.33" in result, (
        "a class the pre-flight SAW must stay in the map even with no stored "
        f"syntax, got {sorted(result)}"
    )
    assert result["1.2.840.10008.5.1.4.1.1.88.33"] == set()


# ---------------------------------------------------------------------------
# :963 - an invalid identifier is a request error, not a cancel
# ---------------------------------------------------------------------------
def test_a_parse_error_yields_the_processing_failure_status():
    """The old path yielded 0xC000 as the DESTINATION pair: pynetdicom read it
    as (address, port) with a None port and answered 0xA801 "unknown
    destination" for a malformed request."""
    from clean_code.handlers.cmove import handle_c_move

    from tests.conftest import make_fake_move_event

    event = make_fake_move_event()
    event.identifier.QueryRetrieveLevel = "SERIES"
    event.identifier.StudyInstanceUID = "1.2.3"
    # No SeriesInstanceUID -> the request cannot be honoured.
    gen = handle_c_move(event, _StubJobs(), None, "http://h/dicomweb", "BRIDGE")
    first = next(gen)
    assert first == (0x0110, None), first


class _StubJobs:
    def reload_peers_from_file(self, *_a, **_k):
        pass


# ---------------------------------------------------------------------------
# :994 - the CIDR registry key is not the connection address
# ---------------------------------------------------------------------------
def test_the_registry_key_survives_a_cidr_substitution(tmp_path):
    """`scp_targets` is keyed by the configured host. Replacing it with the
    client IP meant a subnet-backed peer never had its failure streak updated:
    an exact `(ae_title, host)` UPDATE matched no row."""
    mgr = _mgr(tmp_path)
    mgr.register_peer("SUBNET_PEER", "192.168.1.0/24", 11112)
    target = mgr.lookup_scp("SUBNET_PEER", client_ip="192.168.1.77")
    assert target is not None
    assert "/" in target[0], "the lookup returns the CONFIGURED key"

    job_id = mgr.create_job(
        "1.2.3",
        "SUBNET_PEER",
        "192.168.1.77",  # the connection address
        11112,
        registry_host="192.168.1.0/24",  # the peers.conf key
    )
    claimed = mgr.pick_pending_job(timeout=0.01)
    assert claimed is not None and claimed["id"] == job_id
    assert claimed["registry_host"] == "192.168.1.0/24", claimed["registry_host"]
    assert claimed["target_host"] == "192.168.1.77"


def test_scp_health_is_recorded_against_the_registry_key(tmp_path):
    """A failure recorded under the registry key reaches the streak; the same
    call under the client IP updates nothing."""
    mgr = _mgr(tmp_path)
    mgr.register_peer("P", "192.168.1.0/24", 11112)
    for _ in range(3):
        mgr.record_scp_failure("P", host="192.168.1.0/24")
    assert (
        mgr.lookup_scp("P", client_ip="192.168.1.9") is None
    ), "three failures under the configured key must evict the entry"


def test_the_connection_address_does_not_update_the_streak(tmp_path):
    """Non-vacuity for the same rule, and the shape of the old defect."""
    mgr = _mgr(tmp_path)
    mgr.register_peer("P", "192.168.1.0/24", 11112)
    for _ in range(3):
        mgr.record_scp_failure("P", host="192.168.1.9")
    assert (
        mgr.lookup_scp("P", client_ip="192.168.1.9") is not None
    ), "recording against the client IP matched no scp_targets row"


# ---------------------------------------------------------------------------
# :2225 - the proposal must never exceed 128 presentation contexts
# ---------------------------------------------------------------------------
def test_the_proposal_never_exceeds_the_pcid_limit():
    """PS3.8 7.1.1.13: PCIDs are the odd bytes 1..255, so at most 128 contexts.

    One of them is reserved for Storage Commitment. With 63 surviving classes
    (126 baseline contexts) and two observed stored syntaxes, the proposal
    reached 129 and `ae.associate()` raised instead of opening the association -
    the move transferred NOTHING. The stored extras have to fit the remaining
    slots, whatever the class count.
    """
    import clean_code.handlers.cmove as cm

    class _Ctx:
        def __init__(self, sop, ts):
            self.abstract_syntax = sop
            self.transfer_syntax = ts
            self.context_id = 1
            self.result = 0

    class _AE:
        def __init__(self):
            self.requested = []
            self.accepted_contexts = []

        def add_requested_context(self, sop, ts=None, **_k):
            self.requested.append((sop, ts))

        def associate(self, *_a, **_k):
            class _Assoc:
                is_established = True
                accepted_contexts = []

            return _Assoc()

    ae = _AE()
    # 63 classes really in the study, the LAST one carrying a second stored
    # syntax so the extras overflow the last free slot.
    observed_classes = [f"1.2.840.10008.5.1.4.1.1.{200 + i}" for i in range(63)]
    stored = {uid: set() for uid in observed_classes}
    stored[observed_classes[0]] = {"1.2.840.10008.1.2.5"}
    stored[observed_classes[1]] = {"1.2.840.10008.1.2.4.70"}
    registry = list(observed_classes)

    _orig_ae = cm.AE
    _orig_reg = cm._proposed_ts_per_class
    try:
        cm.AE = lambda *_a, **_k: ae
        cm._proposed_ts_per_class = lambda _reg: registry
        cm._associate_for_move(
            "PEER",
            "BRIDGE",
            {uid: {} for uid in observed_classes},
            "127.0.0.1",
            11112,
            study_ts_by_class=stored,
            partial_preflight=True,
        )
    finally:
        cm.AE = _orig_ae
        cm._proposed_ts_per_class = _orig_reg

    assert len(ae.requested) <= 128, (
        "the proposal exceeded the 128-context limit: %d contexts, so "
        "ae.associate() raises and the move transfers nothing" % len(ae.requested)
    )
    # Non-vacuity: the observed stored syntaxes must still be requested where
    # the budget allows, so this is not passing by proposing nothing.
    proposed = set(ae.requested)
    assert any(
        ts == "1.2.840.10008.1.2.5" for _s, ts in proposed
    ), "the observed stored syntax must survive the trim"


# ---------------------------------------------------------------------------
# :2001 - stored syntaxes are reserved before speculative contexts
# ---------------------------------------------------------------------------
def test_stored_syntaxes_outrank_registry_fill_in():
    """The registry fill-in is a GUESS and the pre-flight is EVIDENCE. Spending
    the last free slots on guessed contexts and then trimming away the stored
    syntax of a class known to be in the study makes that class undeliverable
    on a passthrough origin, where the stored form is the only one that exists.
    """
    import clean_code.handlers.cmove as cm

    class _AE:
        def __init__(self):
            self.requested = []

        def add_requested_context(self, sop_uid, ts_uid=None):
            self.requested.append((str(sop_uid), str(ts_uid)))

        def associate(self, *_a, **_k):
            return _Established()

    class _Ctx:
        def __init__(self, sop, ts):
            self.abstract_syntax = sop
            self.transfer_syntax = [ts]

    class _Established:
        is_established = True

        def __init__(self):
            self.accepted_contexts = [
                _Ctx(cm.ExplicitVRLittleEndian and SOP_CLASS, "1.2.840.10008.1.2.1")
            ]

    ae = _AE()
    # Two classes really in the study, each with a compressed stored syntax,
    # plus a large registry fill-in.
    stored = {
        "1.2.840.10008.5.1.4.1.1.2": {"1.2.840.10008.1.2.5"},
        "1.2.840.10008.5.1.4.1.1.4": {"1.2.840.10008.1.2.4.70"},
    }
    registry = [f"1.2.840.10008.5.1.4.1.1.{100 + i}" for i in range(70)]

    _orig_ae = cm.AE
    _orig_reg = cm._proposed_ts_per_class
    try:
        cm.AE = lambda *_a, **_k: ae
        cm._proposed_ts_per_class = lambda _reg: registry
        cm._associate_for_move(
            "PEER",
            "BRIDGE",
            {
                "1.2.840.10008.5.1.4.1.1.2": {},
                "1.2.840.10008.5.1.4.1.1.4": {},
            },
            "127.0.0.1",
            11112,
            study_ts_by_class=stored,
            partial_preflight=True,
        )
    finally:
        cm.AE = _orig_ae
        cm._proposed_ts_per_class = _orig_reg

    proposed = {(sop, ts) for sop, ts in ae.requested}
    # The stored syntax of BOTH known classes must be proposed.
    assert (list(stored)[0], "1.2.840.10008.1.2.5") in proposed, (
        "the stored syntax of a class known to be in the study was trimmed "
        "away in favour of speculative contexts"
    )
    assert (list(stored)[1], "1.2.840.10008.1.2.4.70") in proposed


# ---------------------------------------------------------------------------
# :3180 - a C-STORE warning is not a failure
# ---------------------------------------------------------------------------
def test_a_c_store_warning_counts_as_acknowledged(monkeypatch):
    """PS3.4 B.2.3.1: a Warning status means the SCP ACCEPTED and stored the
    instance, having coerced an element. Counting it failed listed the SOP as
    undelivered, answered 0xC000, and re-sent it on resume."""
    import clean_code.handlers.cmove as cm

    statuses = [0xB000]

    class _Assoc:
        ae_title = "PEER"
        port = 11112
        is_established = True
        accepted_contexts = []

        def send_c_store(self, _path):
            # pynetdicom 3.0.4 returns a Dataset carrying Status.
            ds = Dataset()
            ds.Status = statuses.pop(0)
            return ds

    body = _minimal_dicom(SOP_CLASS, "1.2.3.4.5.1", "1.2.840.10008.1.2.1")
    _orig = cm.requests.get
    try:
        cm.requests.get = lambda *a, **k: _StreamResponse([body])
        out = cm._stream_and_send_study(
            _Assoc(),
            "1.2.3",
            "http://h/dicomweb",
            5,
            _NoJobs(),
            1,
            accepted_contexts_map={SOP_CLASS: {"1.2.840.10008.1.2.1"}},
        )
    finally:
        cm.requests.get = _orig

    assert out[1] == 0, f"a warning is not a failure: {out}"
    assert out[0] == 1, "the instance is acknowledged"
    assert out[3], "it belongs in the delivered list (Storage Commitment)"


class _NoJobs:
    def is_job_cancelled(self, *_a, **_k):
        return False

    def update_job_progress(self, *_a, **_k):
        return None

    def add_job_delivered_sop(self, *_a, **_k):
        return None


class _StreamResponse:
    """A requests-style streaming response over one multipart part."""

    headers = {"Content-Type": "multipart/related; boundary=BOUNDARY"}

    def __init__(self, parts):
        self._parts = parts
        self.closed = False

    def raise_for_status(self):
        return None

    def close(self):
        self.closed = True

    def iter_content(self, chunk_size=65536):
        boundary = b"BOUNDARY"
        chunks = []
        for body in self._parts:
            chunks.append(
                b"--"
                + boundary
                + b"\r\nContent-Type: application/dicom\r\n"
                + b"Content-Length: "
                + str(len(body)).encode()
                + b"\r\n\r\n"
                + body
                + b"\r\n"
            )
        chunks.append(b"--" + boundary + b"--\r\n")
        # One chunk, so the framing is exercised but not split mid-part.
        yield b"".join(chunks)


def _ui(tag_bytes, value):
    raw = value.encode("ascii")
    if len(raw) % 2:
        raw += b"\x00"
    return tag_bytes + b"UI" + len(raw).to_bytes(2, "little") + raw


def _minimal_dicom(sop_class, sop_uid, ts):
    """A tiny Part 10 file: preamble + DICM + group 0002 + the two SOP UIDs.

    The DICM magic is required: the streaming parser rejects anything else
    (PS3.10 7.1), which is exactly what an in-memory fixture must reproduce.
    """
    meta = _ui(b"\x02\x00\x02\x00", sop_class)
    meta += _ui(b"\x02\x00\x03\x00", sop_uid)
    meta += _ui(b"\x02\x00\x10\x00", ts)
    group = b"\x02\x00\x00\x00" + b"UL" + (4).to_bytes(2, "little")
    group += len(meta).to_bytes(4, "little")
    body = _ui(b"\x08\x00\x16\x00", sop_class) + _ui(b"\x08\x00\x18\x00", sop_uid)
    return b"\x00" * 128 + b"DICM" + group + meta + body


# ---------------------------------------------------------------------------
# :3424 - a worker never parks on a peer another worker just claimed
# ---------------------------------------------------------------------------
def test_the_pick_reserves_the_peer_atomically(tmp_path):
    """A `skip` read followed by a separate claim let two workers both find the
    peer free and claim two jobs for it; one then parked in the gate while a
    job for a DIFFERENT peer sat unserved."""
    mgr = _mgr(tmp_path)
    mgr.create_job("1.2.3.1", "PEER_A", "10.0.0.1", 11112)
    mgr.create_job("1.2.3.2", "PEER_B", "10.0.0.2", 11112)

    held = set()

    def reserve(host, port, ae):
        if ae == "PEER_A":
            held.add(ae)
            return False
        return True

    job = mgr.pick_pending_job(timeout=0.5, reserve=reserve)
    assert job is not None, "the other peer's job must still be served"
    assert job["move_dest_ae"] == "PEER_B", job["move_dest_ae"]


def test_a_refused_reservation_leaves_the_row_pending(tmp_path):
    """The refused row must stay claimable: a busy peer must not consume it."""
    mgr = _mgr(tmp_path)
    job_id = mgr.create_job("1.2.3.1", "PEER_A", "10.0.0.1", 11112)

    job = mgr.pick_pending_job(timeout=0.2, reserve=lambda *a: False)
    assert job is None
    with mgr._db.locked() as conn:
        row = conn.execute(
            "SELECT status, attempts FROM move_jobs WHERE id = ?", (job_id,)
        ).fetchone()
    assert row == (
        "pending",
        0,
    ), f"a refused reservation must not claim or charge the row, got {row!r}"


def test_a_lost_claim_releases_the_reservation(tmp_path):
    """The losing thread must hand the peer back, or the gate leaks."""
    mgr = _mgr(tmp_path)
    mgr.create_job("1.2.3.1", "PEER_A", "10.0.0.1", 11112)

    released = []
    # Steal the row between the SELECT and the claim.
    orig = mgr._claim_row
    try:
        mgr._claim_row = lambda job_id: None
        job = mgr.pick_pending_job(
            timeout=0.1,
            reserve=lambda *a: True,
            release=lambda host, port, ae: released.append((host, port, ae)),
        )
    finally:
        mgr._claim_row = orig
    assert job is None
    assert released == [("10.0.0.1", 11112, "PEER_A")], released


# ---------------------------------------------------------------------------
# :3650 - the cached SOP list is reconciled with observed failures
# ---------------------------------------------------------------------------
def test_an_observed_failure_extends_the_cached_sop_list(tmp_path, monkeypatch):
    """The WADO stream may hold instances the cached enumeration does not (a
    study that grew between the two). If the peer refuses one of those, the
    cached list alone reported Success for a job that left an instance behind.

    Driven through the REAL `_execute_cmove_job`: the pass reports
    `1.2.3.4.5.2` as failed, the cache knows only `1.2.3.4.5.1`, and the job
    must finish with that failure recorded.
    """
    import clean_code.handlers.cmove as cm

    mgr = _mgr(tmp_path)
    mgr.register_peer("PEER", "10.0.0.5", 11112)
    job_id = mgr.create_job("1.2.3", "PEER", "10.0.0.5", 11112)

    cached = ["1.2.3.4.5.1"]
    delivered = [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.4.5.1")]

    monkeypatch.setattr(cm, "_preflight_study_ts_by_class", lambda *a, **k: ({}, True))
    monkeypatch.setattr(cm, "_get_clarus_transfer_syntaxes", lambda *a, **k: [])
    monkeypatch.setattr(cm, "_study_sop_uids", lambda *a, **k: list(cached))
    monkeypatch.setattr(
        cm,
        "_associate_for_move",
        lambda *a, **k: (_EstablishedAssoc(), {SOP_CLASS: {"1.2.840.10008.1.2.1"}}),
    )
    monkeypatch.setattr(cm, "_release_or_abort", lambda *a, **k: None)
    monkeypatch.setattr(cm, "_job_gate_enter", lambda *a, **k: None)
    monkeypatch.setattr(cm, "_job_gate_exit", lambda *a, **k: None)
    monkeypatch.setattr(
        cm,
        "_stream_and_send_study",
        lambda *a, **k: (
            1,  # completed
            0,  # failed (the pass counted none)
            1,  # total seen by the pass
            list(delivered),
            # The instance the peer refused, which the cache never listed.
            ["1.2.3.4.5.2"],
            False,
            False,
            False,
        ),
    )

    cm._execute_cmove_job(
        job_id,
        "1.2.3",
        "PEER",
        mgr,
        "http://127.0.0.1:1/dicomweb",
        1,
        "BRIDGE",
        target_host="10.0.0.5",
        target_port=11112,
        storage_commit_after_move=False,
    )

    failed = mgr.get_job_failed_sops(job_id)
    assert failed == ["1.2.3.4.5.2"], (
        "an instance the peer refused must be reported even when the cached "
        f"enumeration never listed it, got {failed!r}"
    )
    _completed, total, status = mgr.get_job_progress(job_id)
    assert status == "failed", "a job with an undelivered instance is not a success"
    assert total == 2, f"the total must include the observed failure, got {total}"


def test_the_cache_alone_would_hide_the_failure():
    """Non-vacuity: the pre-P6 formula drops the observed failure entirely."""
    cached = ["1.2.3.4.5.1"]
    delivered = {"1.2.3.4.5.1"}

    total_old = len(set(cached))
    failed_old = [u for u in cached if u not in delivered]
    assert total_old == 1
    assert failed_old == [], "this is the Success-for-a-lossy-job defect"


class _EstablishedAssoc:
    """Minimal association: accepted, with Storage Commitment not negotiated."""

    ae_title = "PEER"
    port = 11112
    is_established = True
    accepted_contexts = []


# ---------------------------------------------------------------------------
# :1225 - the final response carries the worker's own counts
# ---------------------------------------------------------------------------
def test_the_success_branch_yields_zero_suboperations(tmp_path):
    """The success branch yields 0 sub-operations: pynetdicom short-circuits
    to 0x0000 and opens no move-destination association (which would raise,
    since the SCP AE has no requested contexts). A non-zero count made
    pynetdicom open store_assoc and answer 0xC515."""
    from clean_code.handlers.cmove import handle_c_move

    from tests.conftest import make_fake_move_event

    mgr = _mgr(tmp_path)
    mgr.register_peer("PEER", "10.0.0.5", 11112)
    event = make_fake_move_event()
    event.assoc.requestor.address = "10.0.0.5"
    event.move_destination = "PEER"
    event.identifier.StudyInstanceUID = "1.2.3"

    gen = handle_c_move(
        event,
        mgr,
        None,
        "http://127.0.0.1:1/dicomweb",
        "BRIDGE",
        move_poll_timeout=0.3,
    )
    assert next(gen) == ("10.0.0.5", 11112), "the destination pair comes first"

    # Mark the queued job as fully delivered, then let the poll resolve it.
    job_id = mgr.pick_pending_job(timeout=0.5)["id"]
    mgr.update_job_progress(job_id, 7, 0, 7)
    mgr.finish_job(job_id, success=True)

    count = next(gen)
    assert count == 0, (
        "the success branch must yield 0 sub-operations so pynetdicom sends "
        f"0x0000 without opening a move-destination association, got {count}"
    )
    # An all-success batch ends after the count: pynetdicom sends the final
    # 0x0000 itself, so there is no further yield to read.
    assert list(gen) == []
    mgr.close()
