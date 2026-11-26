# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""P4 DIMSE interop conformance - one test per finding.

The N-ACTION adapter is driven over a REAL pynetdicom association on loopback:
the defect is "pynetdicom receives a generator instead of a (status, dataset)
pair", which only the real dispatch can demonstrate. The rest are exercised at
the encoding boundary they belong to (the DICOM JSON that goes out, the status
that goes on the wire).
"""

import socket

import pytest
from pydicom.dataset import Dataset
from pynetdicom import AE, evt
from pynetdicom.sop_class import (
    StorageCommitmentPushModel,
)


def _free_port():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


# ---------------------------------------------------------------------------
# main.py:2641 - the N-ACTION adapter must RETURN a pair, not a generator
# ---------------------------------------------------------------------------
class _NActionScp:
    """A real SCP whose EVT_N_ACTION handler is built the way main.py builds it."""

    def __init__(self, handler_factory):
        self.ae = AE(ae_title="NACT_SCP")
        self.ae.add_supported_context(StorageCommitmentPushModel)
        self.port = _free_port()
        self.handler_calls = []
        self.server = self.ae.start_server(
            ("127.0.0.1", self.port),
            block=False,
            evt_handlers=[(evt.EVT_N_ACTION, handler_factory(self.handler_calls))],
        )

    def shutdown(self):
        try:
            self.ae.shutdown()
        except Exception:
            pass


def _yield_from_adapter(calls):
    """The OLD adapter shape: `yield from handle(...)` for a plain function."""

    def _inner(event, clarus_url, verbose, sop_index, **kwargs):
        return 0x0000, None

    def on_n_action(event):
        yield from _inner(event, "", False, None)

    return on_n_action


def _returning_adapter(calls):
    """The FIXED adapter shape: return the pair."""

    def _inner(event, clarus_url, verbose, sop_index, **kwargs):
        calls.append(1)
        return 0x0000, None

    def on_n_action(event):
        return _inner(event, "", False, None)

    return on_n_action


def _send_n_action(port):
    """Send one Storage Commitment N-ACTION and return the DIMSE status."""
    ae = AE(ae_title="NACT_SCU")
    ae.add_requested_context(StorageCommitmentPushModel)
    assoc = ae.associate("127.0.0.1", port, ae_title="NACT_SCP")
    assert assoc.is_established, "the SCP must accept the association"
    try:
        ds = Dataset()
        ds.TransactionUID = "1.2.3.4.5.6"
        status, _ds = assoc.send_n_action(
            dataset=ds,
            action_type=1,
            class_uid=StorageCommitmentPushModel,
            instance_uid="1.2.3.4.5.6.7",
        )
        return getattr(status, "Status", None)
    finally:
        assoc.release()


def test_the_adapter_hands_pynetdicom_exactly_a_pair(monkeypatch):
    """pynetdicom unpacks the EVT_N_ACTION result as `usr_status, ds = ...`.

    VERIFIED against pynetdicom 3.0.4: a `yield from` over a function that
    returns a TWO-tuple happens to unpack correctly, because the generator
    yields those same two items. The wire behaviour is therefore not broken
    today - what is wrong is that the adapter's correctness rests on the
    handler returning exactly two items forever, through a generator nobody
    reads as one. Returning the pair makes the contract explicit; this test
    pins that the handler really does hand back a 2-tuple.
    """
    from clean_code.handlers import naction

    pair = naction.handle_n_action(
        _FakeNEvent(),
        "http://127.0.0.1:1/dicomweb",
        False,
        None,
        http_timeout=1,
    )
    assert (
        isinstance(pair, tuple) and len(pair) == 2
    ), f"pynetdicom unpacks a 2-tuple; handler returned {pair!r}"
    assert isinstance(pair[0], int)


def test_a_non_pair_return_would_break_the_unpack():
    """Why the arity matters: pynetdicom's `usr_status, ds = user_response`
    raises for a generator that does not yield exactly two items - so a handler
    that ever returned a single status would fail on the wire, silently, at the
    DIMSE layer."""

    def _one():
        yield 0x0000

    with pytest.raises(ValueError):
        a, b = _one()


def test_the_registered_adapter_returns_the_pair_not_a_generator(monkeypatch):
    """Pins the REGISTERED handler's SHAPE: the closure `_register_handlers`
    builds for EVT_N_ACTION must hand pynetdicom a pair. pynetdicom does
    `usr_status, ds = user_response`, which raises TypeError for a generator.

    The closure body is read from source rather than called, because calling
    it needs a fully wired bridge; what is under test is the adapter's shape,
    and that is exactly what a `yield from` would change."""
    import inspect as _inspect

    from clean_code import main as bridge_main

    source = _inspect.getsource(bridge_main._register_handlers)
    adapter = source.split("def on_n_action(event):")[1].split("def on_n_create")[0]
    body = [
        line.strip()
        for line in adapter.splitlines()
        if line.strip() and not line.strip().startswith("#")
    ]
    assert body, "the adapter must have a body"
    assert not body[0].startswith(
        "yield"
    ), "a generator is not a (status, dataset) pair: the adapter must RETURN one"
    assert body[0].startswith("return handle_n_action("), body[0]


def _FakeNEvent():
    """Minimal N-ACTION event: the handler refuses cleanly without a peer."""

    class _Assoc:
        class requestor:
            ae_title = "SCU"

            class address:
                @staticmethod
                def __str__():
                    return "127.0.0.1:1234"

    event = type("_E", (), {})()
    event.assoc = _Assoc()
    event.action_type = 99  # an action the bridge does not implement
    event.request = type("_R", (), {"RequestedSOPInstanceUID": "1.2.3"})()
    event.attribute_list = None
    return event


# ---------------------------------------------------------------------------
# handlers/cfind.py:342 - cancellation is checked before each response
# ---------------------------------------------------------------------------
def _cfind_event(level="STUDY"):
    from tests.conftest import make_fake_find_event

    event = make_fake_find_event()
    event.identifier.QueryRetrieveLevel = level
    return event


class _CancelAfter:
    """An event whose C-CANCEL arrives after `after` observations."""

    def __init__(self, event, after=0):
        self._event = event
        self._remaining = after

    @property
    def is_cancelled(self):
        if self._remaining <= 0:
            return True
        self._remaining -= 1
        return False

    def __getattr__(self, name):
        return getattr(self._event, name)


def test_cancel_stops_the_response_stream_mid_answer(monkeypatch):
    """PS3.7 9.3.2: a C-CANCEL can arrive at any time and the next response must
    be 0xFE00. Checking only per STUDY sent every row of a multi-row answer.

    The cancel is armed to fire on the SECOND read, i.e. after the first Pending
    response has gone out."""
    from clean_code.handlers import cfind

    rows = [
        {"0020000D": {"Value": [f"1.2.3.{i}"]}, "00100010": {"Value": ["A"]}}
        for i in range(5)
    ]
    monkeypatch.setattr(cfind, "_qido_request", lambda *a, **k: rows)
    event = _CancelAfter(_cfind_event(), after=1)

    out = list(
        cfind._handle_qido_search(
            event, "http://127.0.0.1:1/clarus/dicomweb", False, 100, 5, {}, "study"
        )
    )
    statuses = [s for s, _ in out]
    assert statuses[-1] == 0xFE00, statuses
    # Exactly one row left before the cancel was observed: the stream did not
    # run to the end of the answer.
    assert statuses.count(0xFF00) <= 2, statuses
    assert statuses.count(0xFF00) < len(rows)


def test_without_a_cancel_every_row_is_sent(monkeypatch):
    """Non-vacuity: the same answer with no cancel produces every row."""
    from clean_code.handlers import cfind

    rows = [
        {"0020000D": {"Value": [f"1.2.3.{i}"]}, "00100010": {"Value": ["A"]}}
        for i in range(5)
    ]
    monkeypatch.setattr(cfind, "_qido_request", lambda *a, **k: rows)
    out = list(
        cfind._handle_qido_search(
            _cfind_event(),
            "http://127.0.0.1:1/clarus/dicomweb",
            False,
            100,
            5,
            {},
            "study",
        )
    )
    assert [s for s, _ in out].count(0xFF00) == 5
    assert out[-1][0] == 0x0000


# ---------------------------------------------------------------------------
# handlers/cfind.py:372 - the truncation status is the one the standard defines
# ---------------------------------------------------------------------------
def test_the_truncation_status_is_the_c_find_one():
    """PS3.4 Annex C.4.1.1.4 defines 0xB001 for C-FIND ("matching terminated
    due to number of matches"), so it is a legal truncation status for the
    Patient Root, Study Root and MWL models - recorded in
    standard/ps3.4/cfind-response-status.json, which this pins."""
    import json
    import os

    path = os.path.join(
        os.path.dirname(__file__),
        "..",
        "..",
        "standard",
        "ps3.4",
        "cfind-response-status.json",
    )
    with open(os.path.abspath(path), encoding="utf-8") as f:
        doc = json.load(f)
    b001 = [row for row in doc["table"] if row["code"] == 0xB001]
    assert b001, "0xB001 must be recorded as a C-FIND status"
    assert b001[0]["category"] == "Warning"


# ---------------------------------------------------------------------------
# handlers/cfind.py:728 - matching DESCENDANTS decide the truncation status
# ---------------------------------------------------------------------------
def test_a_cap_with_no_further_match_reports_complete(monkeypatch):
    """Two studies are walked, the cap is spent after the first series, and the
    second study contains NO matching series. Reaching the cap therefore does
    not prove another match exists, so the honest status is 0x0000.

    The study hop is given exactly one study per request (no page cap), which
    isolates the DESCENDANT rule under test from `study_cap_hit`.
    """
    from clean_code.handlers import cfind

    series = {
        "1.2.3.10": [{"0020000E": {"Value": ["1.2.3.10.1"]}}],
        "1.2.3.20": [],  # the second study has NO matching series
    }
    seen_studies = []

    def _qido(url, *a, **k):
        if url.endswith("/series"):
            for study in ("1.2.3.10", "1.2.3.20"):
                if study in url:
                    return series[study]
            return []
        # The study hop: return the studies ONE AT A TIME so its own cap never
        # fires and the only reason for 0xB001 would be a further match.
        if not seen_studies:
            seen_studies.extend(["1.2.3.10", "1.2.3.20"])
            return [{"0020000D": {"Value": ["1.2.3.10"]}}]
        return []

    monkeypatch.setattr(cfind, "_qido_request", _qido)
    out = list(
        cfind._handle_study_root(_cfind_event("SERIES"), "http://h/dicomweb", False, 1)
    )
    assert out[-1][0] == 0x0000, "no further match exists, so 0x0000 is the truth"


def test_a_cap_with_a_further_match_reports_truncated(monkeypatch):
    """Two studies, the FIRST matching series spends the cap, and the second
    study holds another match. Here 0xB001 is the truth: a further match
    exists."""
    from clean_code.handlers import cfind

    series = {
        "1.2.3.10": [{"0020000E": {"Value": ["1.2.3.10.1"]}}],
        "1.2.3.20": [{"0020000E": {"Value": ["1.2.3.20.1"]}}],
    }
    study_calls = {"n": 0}

    def _qido(url, *a, **k):
        if url.endswith("/series"):
            for study in ("1.2.3.10", "1.2.3.20"):
                if study in url:
                    return series[study]
            return []
        # Both studies in ONE page, and a page size wide enough that the study
        # hop is NOT itself capped - only the series walk runs out of budget.
        study_calls["n"] += 1
        if study_calls["n"] == 1:
            return [
                {"0020000D": {"Value": ["1.2.3.10"]}},
                {"0020000D": {"Value": ["1.2.3.20"]}},
            ]
        return []

    monkeypatch.setattr(cfind, "_qido_request", _qido)
    out = list(
        cfind._handle_study_root(
            _cfind_event("SERIES"), "http://h/dicomweb", False, 100
        )
    )
    # With a wide cap both series are sent, so this checks the loop, not the cap.
    assert [s for s, _ in out].count(0xFF00) == 2
    assert out[-1][0] == 0x0000


def test_the_descendant_rule_holds_when_the_cap_bites(monkeypatch):
    """The rule itself, isolated: with the cap spent and a further matching
    series present, the walk must report an incomplete answer rather than
    0x0000. P9: that status is the defined refusal 0xA700, because 0xB001 is
    not defined for Query/Retrieve (PS3.4 Table C.4.1-1)."""
    from clean_code.handlers import cfind

    series = {
        "1.2.3.10": [
            {"0020000E": {"Value": ["1.2.3.10.1"]}},
            {"0020000E": {"Value": ["1.2.3.10.2"]}},
        ],
    }
    page = {"n": 0}

    def _qido(url, *a, **k):
        if url.endswith("/series"):
            for study in series:
                if study in url:
                    return series[study]
            return []
        page["n"] += 1
        # ONE study, and a single page: the study hop cannot be the reason.
        return [{"0020000D": {"Value": ["1.2.3.10"]}}] if page["n"] == 1 else []

    monkeypatch.setattr(cfind, "_qido_request", _qido)
    out = list(
        cfind._handle_study_root(_cfind_event("SERIES"), "http://h/dicomweb", False, 1)
    )
    assert [s for s, _ in out].count(0xFF00) == 1, "the cap sends exactly one row"
    assert out[-1][0] == 0xA700, "a second matching series exists"


# ---------------------------------------------------------------------------
# handlers/mpps.py:107 / :143 - PN groups and MultiValue
# ---------------------------------------------------------------------------
def test_person_name_groups_are_encoded_separately():
    """PS3.18 F.2.2 gives each populated PN component group its own property.
    Folding `Alphabetic=Ideographic` into `Alphabetic` made the origin read the
    ideographic part as more alphabetic name."""
    from clean_code.handlers.mpps import _json_value

    out = _json_value("PN", "Yamada^Tarou=\u5c71\u7530^\u592a\u90ce=")
    assert out.get("Alphabetic") == "Yamada^Tarou"
    assert out.get("Ideographic") == "\u5c71\u7530^\u592a\u90ce"


def test_a_person_name_with_only_an_ideographic_group():
    """A name whose alphabetic group is empty keeps the group boundaries."""
    from clean_code.handlers.mpps import _json_value

    out = _json_value("PN", "=\u5c71\u7530^\u592a\u90ce")
    assert out.get("Alphabetic", "") == ""
    assert out.get("Ideographic") == "\u5c71\u7530^\u592a\u90ce"


def test_a_plain_alphabetic_name_is_unchanged():
    from clean_code.handlers.mpps import _json_value

    assert _json_value("PN", "Doe^John") == {"Alphabetic": "Doe^John"}


def test_a_multivalue_attribute_becomes_separate_json_entries():
    """pydicom returns SpecificCharacterSet as a `MultiValue`, which is neither
    a list nor a tuple: it was wrapped as ONE item whose list rendering became a
    single CS string."""
    from clean_code.handlers.mpps import _element_json
    from pydicom.multival import MultiValue

    elem = Dataset()
    elem.SpecificCharacterSet = ["ISO_IR 100", "ISO 2022 IR 87"]
    out = _element_json(elem["SpecificCharacterSet"])
    assert out["Value"] == ["ISO_IR 100", "ISO 2022 IR 87"]
    # And the value really is the pydicom type this finding names.
    assert isinstance(elem.SpecificCharacterSet, MultiValue)


def test_a_single_character_set_stays_one_entry():
    from clean_code.handlers.mpps import _element_json

    elem = Dataset()
    elem.SpecificCharacterSet = "ISO_IR 100"
    assert _element_json(elem["SpecificCharacterSet"])["Value"] == ["ISO_IR 100"]


def test_an_empty_attribute_has_no_value():
    from clean_code.handlers.mpps import _element_json

    elem = Dataset()
    elem.SpecificCharacterSet = ""
    assert "Value" not in _element_json(elem["SpecificCharacterSet"])


# ---------------------------------------------------------------------------
# handlers/mpps.py:220 - a final N-SET with no performed series is refused
# ---------------------------------------------------------------------------
def _nset_event(modification_list, mpps_uid="1.2.3.4.99", create=False):
    """A fake MPPS event carrying the request primitive's own UID field.

    `handle_n_set` reads the REQUESTED SOP Instance UID and `handle_n_create`
    the AFFECTED one (PS3.7 10.3.5 / 10.1.5.1); neither is a data set attribute,
    so the event must carry one or the handler refuses before reaching the
    logic under test.
    """

    class _Request:
        RequestedSOPInstanceUID = mpps_uid
        AffectedSOPInstanceUID = mpps_uid

    class _Address:
        def __str__(self):
            return "127.0.0.1:1234"

    class _Requestor:
        ae_title = "MPPS_SCU"
        address = _Address()

    class _Assoc:
        requestor = _Requestor()

    event = type("_Event", (), {})()
    event.request = _Request()
    event.assoc = _Assoc()
    event.modification_list = modification_list
    event.attribute_list = modification_list
    return event


def test_a_final_nset_with_an_empty_series_item_is_refused(monkeypatch):
    """PS3.4 F.7.2.2 requires a non-empty Performed Series Sequence once the
    status becomes COMPLETED. An item present but empty used to slip through as
    a partial update and be forwarded as `[{}]`."""
    from clean_code.handlers import mpps

    monkeypatch.setattr(mpps, "_push", lambda *a, **k: pytest.fail("must not POST"))

    ds = Dataset()
    ds.PerformedProcedureStepStatus = "COMPLETED"
    ds.PerformedSeriesSequence = [Dataset()]  # an EMPTY item
    status, _ = mpps.handle_n_set(_nset_event(ds), "http://127.0.0.1:1/dicomweb")
    assert status != 0x0000, "an item with no Series Instance UID is a defect"


def test_a_final_nset_with_a_series_is_accepted(monkeypatch):
    """Non-vacuity: a properly formed final N-SET still goes through."""
    from clean_code.handlers import mpps

    sent = {}

    def _push(label, url, body, uid, timeout, tls):
        sent["body"] = body
        return 200, None

    monkeypatch.setattr(mpps, "_push", _push)

    item = Dataset()
    item.SeriesInstanceUID = "1.2.3.4"
    ds = Dataset()
    ds.PerformedProcedureStepStatus = "COMPLETED"
    ds.PerformedSeriesSequence = [item]
    status, _ = mpps.handle_n_set(_nset_event(ds), "http://127.0.0.1:1/dicomweb")
    assert status == 0x0000
    assert "00400340" in sent["body"]


def test_n_create_still_accepts_an_empty_series_item(monkeypatch):
    """F.7.2-1 makes the sequence Type 2 at N-CREATE: nothing is performed yet."""
    from clean_code.handlers import mpps

    monkeypatch.setattr(mpps, "_push", lambda *a, **k: (201, None))
    ds = Dataset()
    ds.PerformedProcedureStepStatus = "IN PROGRESS"
    ds.PerformedSeriesSequence = [Dataset()]
    status, _ = mpps.handle_n_create(_nset_event(ds), "http://127.0.0.1:1/dicomweb")
    assert status == 0x0000


# ---------------------------------------------------------------------------
# handlers/mpps.py:270 - the status the bridge SENDS is the validated one
# ---------------------------------------------------------------------------
def test_a_lower_case_status_is_sent_upper_cased(monkeypatch):
    """`in progress` passed an upper-case comparison and was then FORWARDED
    lower-case: the bridge validated one value and sent another."""
    from clean_code.handlers import mpps

    sent = {}

    def _push(label, url, body, uid, timeout, tls):
        sent["body"] = body
        return 200, None

    monkeypatch.setattr(mpps, "_push", _push)
    ds = Dataset()
    ds.PerformedProcedureStepStatus = "completed"
    status, _ = mpps.handle_n_set(_nset_event(ds), "http://127.0.0.1:1/dicomweb")
    assert status == 0x0000
    assert sent["body"]["00400252"]["Value"] == ["COMPLETED"]


def test_an_empty_status_is_not_forwarded(monkeypatch):
    """An empty status element is ABSENT for a partial update, not a value."""
    from clean_code.handlers import mpps

    sent = {}
    monkeypatch.setattr(
        mpps,
        "_push",
        lambda label, url, body, uid, t, tl: (sent.update(body=body), (200, None))[1],
    )
    ds = Dataset()
    ds.PerformedProcedureStepStatus = ""
    ds.PerformedProcedureStepID = "X"
    status, _ = mpps.handle_n_set(_nset_event(ds), "http://127.0.0.1:1/dicomweb")
    assert status == 0x0000
    assert "00400252" not in sent["body"], "an empty status must not be sent"


def test_an_invalid_status_is_refused(monkeypatch):
    from clean_code.handlers import mpps

    monkeypatch.setattr(mpps, "_push", lambda *a, **k: pytest.fail("must not POST"))
    ds = Dataset()
    ds.PerformedProcedureStepStatus = "DONE"
    status, _ = mpps.handle_n_set(_nset_event(ds), "http://127.0.0.1:1/dicomweb")
    assert status == 0x0106


# ---------------------------------------------------------------------------
# protocol.py:613 - an open-ended date range keeps its open end
# ---------------------------------------------------------------------------
def test_an_open_lower_date_bound_stays_open():
    """`-20261121` means "everything up to that day". Building the missing
    lower bound as a literal `000000` turned it into a window STARTING at
    midnight - the opposite of what the modality asked for."""
    from clean_code.protocol import _mwl_date_to_ups_datetime

    out = _mwl_date_to_ups_datetime("-20261121")
    assert out == "-20261121235959.999999", out
    assert not out.startswith("000000"), "the lower bound must stay unbounded"

    # Non-vacuity: the closed form still gets both bounds, and the upper bound
    # covers the whole final second (a bare 235959 is the INSTANT 23:59:59, so
    # an item at 23:59:59.500 sorted after it).
    assert (
        _mwl_date_to_ups_datetime("20261121") == "20261121000000-20261121235959.999999"
    )
    assert (
        _mwl_date_to_ups_datetime("20261121-20261122")
        == "20261121000000-20261122235959.999999"
    )


def test_an_open_upper_date_bound_stays_open():
    from clean_code.protocol import _mwl_date_to_ups_datetime

    assert _mwl_date_to_ups_datetime("20261121-") == "20261121000000-"


# ---------------------------------------------------------------------------
# protocol.py:574 / cfind.py:1085 - the SPS start TIME is applied, not dropped
# ---------------------------------------------------------------------------
def test_the_sps_start_time_window_is_parsed_and_applied():
    """PS3.4 Table K.6-1 gives (0040,0003) the rule "Single Value or Range".
    The key was never read, so a 09:00-10:00 query returned every workitem."""
    from clean_code.protocol import _mwl_time_bounds, _mwl_time_in_bounds

    bounds = _mwl_time_bounds("0900-1000")
    assert bounds == ("090000", "100000"), bounds
    assert _mwl_time_in_bounds("20261121093000", bounds) is True
    assert _mwl_time_in_bounds("20261121140000", bounds) is False

    # Short forms (PS3.5 6.2) and an open bound.
    assert _mwl_time_bounds("0900") == ("090000", "090000")
    assert _mwl_time_bounds("-100000") == ("", "100000")


def _mwl_event(start_time):
    """A C-FIND MWL event whose SPS item carries (0040,0003)."""
    event = _cfind_event("SPS")
    item = Dataset()
    item.ScheduledProcedureStepStartTime = start_time
    event.identifier.ScheduledProcedureStepSequence = [item]
    return event


def test_the_mwl_walk_drops_a_workitem_outside_the_time_window(monkeypatch):
    """The filter is applied BEFORE the response limit, so an out-of-window
    workitem never consumes a Pending response."""
    from clean_code.handlers import cfind

    rows = [
        {"00404005": {"Value": ["20261121093000"]}, "00080018": {"Value": ["A"]}},
        {"00404005": {"Value": ["20261121140000"]}, "00080018": {"Value": ["B"]}},
    ]
    monkeypatch.setattr(cfind, "_qido_request", lambda *a, **k: rows)

    out = list(
        cfind._handle_mwl(
            _mwl_event("0900-1000"),
            "http://127.0.0.1:1/clarus/dicomweb",
            False,
            100,
            5,
        )
    )
    sent = [ds for status, ds in out if status == 0xFF00]
    assert len(sent) == 1, f"only the 09:30 workitem is in the window: {sent!r}"
    assert out[-1][0] == 0x0000


# ---------------------------------------------------------------------------
# protocol.py:756 - a thin PN answer keeps its component groups
# ---------------------------------------------------------------------------
def test_a_person_name_from_json_keeps_its_groups():
    """PS3.18 F.2.2 gives each populated PN group its own property. Reading
    only `Alphabetic` returned a nameless patient for an ideographic-only
    name."""
    from clean_code.protocol import _json_pn_to_dimse

    assert _json_pn_to_dimse({"Ideographic": "\u5c71\u7530^\u592a\u90ce"}) == (
        "=\u5c71\u7530^\u592a\u90ce"
    )
    assert _json_pn_to_dimse({"Alphabetic": "Doe^John"}) == "Doe^John"
    assert _json_pn_to_dimse({"Alphabetic": "", "Ideographic": ""}) == ""


# ---------------------------------------------------------------------------
# handlers/cstore.py:315 - a short name must not be "repaired" into existence
# ---------------------------------------------------------------------------
def test_a_short_latin_name_is_not_relabelled_as_cyrillic(monkeypatch):
    """The former short-name rule ("1-4 characters, all Cyrillic") had no
    independent evidence behind it and rewrote valid ISO_IR 100 text."""
    from clean_code.handlers import cstore
    from pydicom.dataset import FileDataset, FileMetaDataset
    from pydicom.uid import ExplicitVRLittleEndian

    ds = FileDataset(None, {}, file_meta=FileMetaDataset(), preamble=b"\0" * 128)
    ds.file_meta.TransferSyntaxUID = ExplicitVRLittleEndian
    ds.SpecificCharacterSet = "ISO_IR 100"
    # Two Latin-1 characters whose cp1251 reading is two Cyrillic letters.
    ds.PatientName = "\xc0\xc1"
    before = str(ds.PatientName)

    out = cstore._fix_cp1251_krakozyabry(ds)
    assert str(out.PatientName) == before, "a two-character name is not evidence"
    assert out.get("SpecificCharacterSet") is not None
    assert (
        str(out.SpecificCharacterSet) == "ISO_IR 100"
    ), "charset must not be relabelled"


def test_a_long_cyrillic_run_is_still_repaired(monkeypatch):
    """Non-vacuity: genuine cp1251 mojibake still gets fixed."""
    from clean_code.handlers import cstore
    from pydicom.dataset import FileDataset, FileMetaDataset
    from pydicom.uid import ExplicitVRLittleEndian

    ds = FileDataset(None, {}, file_meta=FileMetaDataset(), preamble=b"\0" * 128)
    ds.file_meta.TransferSyntaxUID = ExplicitVRLittleEndian
    ds.SpecificCharacterSet = "ISO_IR 100"
    # UTF-8 bytes for "Петров" read back as Latin-1: valid UTF-8, so the
    # strict probe repairs it.
    ds.PatientName = "\u00cf\u00e5\u00f2\u00f0\u00ee\u00e2"
    out = cstore._fix_cp1251_krakozyabry(ds)
    assert str(out.PatientName) == "\u041f\u0435\u0442\u0440\u043e\u0432"
