# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""DCST4 1.13.0 rerun fixes: Q3 C-MOVE QidoPage regression + Q1 C-STORE guard.

Q3: `_qido_request` now returns a `QidoPage` (not a `list`); the C-MOVE
pre-flight and the study walk must accept it or every C-MOVE degrades to the
registry-wide proposal and drops the late-sorting SOP classes.

Q1: the outbox answers C-STORE 0x0000 before the STOW verdict exists, so the
bridge must refuse a study it already holds under a DIFFERENT non-empty
PatientID, synchronously, before the enqueue.
"""

from clean_code.handlers import cmove as cm
from clean_code.handlers import cstore as cs
from clean_code.http_helpers import QidoPage

from tests.conftest import make_fake_cstore_event

CT = "1.2.840.10008.5.1.4.1.1.2"
TS_470 = "1.2.840.10008.1.2.4.70"
URL = "http://127.0.0.1:8024/dicomweb"


class _Resp:
    def __init__(self, payload):
        self._payload = payload
        self.status_code = 200

    def json(self):
        return self._payload

    def raise_for_status(self):
        pass


def _series_row(suid):
    return {"0020000E": {"vr": "UI", "Value": [suid]}}


def _inst_row(sop):
    return {"00080018": {"vr": "UI", "Value": [sop]}}


def _class_ts_row(ts=TS_470):
    return [
        {
            "00080016": {"vr": "UI", "Value": [CT]},
            "00020010": {"vr": "UI", "Value": [ts]},
        }
    ]


# ---------------------------------------------------------------------------
# Q3 - the pre-flight must accept a QidoPage
# ---------------------------------------------------------------------------
def test_preflight_accepts_qidopage_series_and_instance_rows(monkeypatch):
    """`_qido_request` returns a QidoPage since the QIDO fix; the pre-flight
    must not fall into the `isinstance(x, list)` refusal or every C-MOVE
    loses its stored-TS proposal."""
    cm._preflight_cache.clear()
    monkeypatch.setattr(
        cm,
        "_qido_request",
        lambda url, *a, **k: (
            QidoPage([_series_row("2.25.1")], False)
            if url.endswith("/series")
            else QidoPage([_inst_row("1.2.3")], False)
        ),
    )
    monkeypatch.setattr(cm.requests, "get", lambda url, **k: _Resp(_class_ts_row()))
    out, _partial = cm._preflight_study_ts_by_class("2.25.20", "http://x/dicomweb")
    assert out == {CT: {TS_470}}, "pre-flight must not fail on a QidoPage"


def test_study_sop_uids_accepts_qidopage(monkeypatch):
    """The study walk must enumerate SOP UIDs from a QidoPage, not return []."""
    cm._study_sop_uids_cache.clear()
    monkeypatch.setattr(
        cm,
        "_qido_request",
        lambda url, *a, **k: (
            QidoPage([_series_row("2.25.1")], False)
            if url.endswith("/series")
            else QidoPage([_inst_row("1.2.3"), _inst_row("1.2.4")], False)
        ),
    )
    uids = cm._study_sop_uids("2.25.30", "http://x/dicomweb")
    assert uids == ["1.2.3", "1.2.4"], "must enumerate SOP UIDs from a QidoPage"


# ---------------------------------------------------------------------------
# Q1 - synchronous study->PatientID guard at C-STORE time
# ---------------------------------------------------------------------------
class _FakeOutbox:
    def __init__(self):
        self.max_queue = 5000
        self.max_queue_bytes = 0
        self.enqueued = []

    def pending_count(self):
        return 0

    def enqueue(self, dataset, modifications=None):
        self.enqueued.append(dataset.SOPInstanceUID)
        return True


def _cstore(patient_id, sop_uid, study_uid, outbox):
    ev = make_fake_cstore_event(
        sop_instance_uid=sop_uid,
        study_uid=study_uid,
    )
    ev.dataset.PatientID = patient_id
    return cs.handle_c_store(ev, URL, False, outbox=outbox)


def test_second_patient_under_a_held_study_is_refused_synchronously(
    mock_shutdown_not_set,
):
    """(1) P1 -> 0x0000; (2) P2 -> 0xC000 BEFORE enqueue; (3) P1 -> 0x0000;
    (4) an empty PatientID on a study the bridge has never seen is accepted
    (Type 2 "not stated" -> no conflict)."""
    cs._study_patient_map.clear()
    outbox = _FakeOutbox()

    # (1) first patient of study S is accepted and recorded.
    assert _cstore("P1", "1.2.3.4.5.1", "1.2.3", outbox) == 0x0000
    assert cs._study_patient_get("1.2.3") == "P1"

    # (2) a different patient for the SAME study is refused before the
    # enqueue: the fire-and-forget outbox would otherwise answer 0x0000 and
    # the STOW verdict (409 -> 0xC000) would never reach the modality.
    assert _cstore("P2", "1.2.3.4.5.2", "1.2.3", outbox) == 0xC000
    assert "1.2.3.4.5.2" not in outbox.enqueued, "the conflict must not enqueue"

    # (3) the study's own patient is still accepted.
    assert _cstore("P1", "1.2.3.4.5.3", "1.2.3", outbox) == 0x0000

    # (4) empty PatientID is Type 2 "not stated": it must not be refused as a
    # conflict. The bridge coerces it to AUTOPAT_<study> before the guard (a
    # coercion WARNING 0xB000, not a refusal), so this is exercised on a study
    # the bridge has never held - there is nothing to conflict with.
    status4 = _cstore("", "1.2.3.4.5.4", "9.9.9", outbox)
    assert status4 != 0xC000, "empty PatientID must not be refused as a conflict"
    assert outbox.enqueued == ["1.2.3.4.5.1", "1.2.3.4.5.3", "1.2.3.4.5.4"]


def test_study_patient_map_is_bounded(monkeypatch):
    """The map is a bounded guard cache: on overflow the oldest entry is
    evicted, so a long-running forwarder never grows one entry per study."""
    cs._study_patient_map.clear()
    monkeypatch.setattr(cs, "_STUDY_PATIENT_CAP", 3)
    for i in range(5):
        cs._study_patient_put(f"study-{i}", f"P{i}")
    assert len(cs._study_patient_map) == 3, "the map must not exceed the cap"
    assert "study-0" not in cs._study_patient_map, "the oldest entry is evicted"
    assert cs._study_patient_get("study-4") == "P4"
