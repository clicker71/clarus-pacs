# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""P10 triage - one falsifiable test per fix.

Each test fails when its guard is reverted. Refs in the docstrings name the
finding the test covers.
"""

import os
import threading
import time

import pytest
from pydicom.dataset import Dataset


def _ds(**kwargs):
    ds = Dataset()
    for key, value in kwargs.items():
        setattr(ds, key, value)
    return ds


# ---------------------------------------------------------------------------
# config :296
# ---------------------------------------------------------------------------
class TestPeersConfHost:
    def test_a_peer_with_an_empty_host_slot_is_refused(self, tmp_path):
        """`CT :104` parses cleanly through `rsplit(":", 1)`.

        Accepting it registered a peer whose host is "", so every later lookup
        compared against "" and never matched: the peer occupied a slot and
        accepted nothing. It must go through the malformed-address handler.
        """
        from clean_code.config import parse_peers_conf

        peers = tmp_path / "peers.conf"
        peers.write_text("GOOD_SCP  10.0.0.5:104\nEMPTY_SCP  :104\n", encoding="utf-8")
        rows = list(parse_peers_conf(str(peers)))

        assert [r[0] for r in rows] == [
            "GOOD_SCP"
        ], "a peer with no host must not be registered"

    def test_a_named_host_still_parses(self, tmp_path):
        """Control: the guard must not reject a well-formed line."""
        from clean_code.config import parse_peers_conf

        peers = tmp_path / "peers.conf"
        peers.write_text("SCP  10.0.0.5:11112\n", encoding="utf-8")

        assert list(parse_peers_conf(str(peers))) == [("SCP", "10.0.0.5", 11112)]


# ---------------------------------------------------------------------------
# assoc_tracker :118
# ---------------------------------------------------------------------------
class TestAssocTracker:
    def test_an_instance_without_a_study_uid_is_still_counted(self):
        """A C-STORE can carry a SOP Instance UID and no Study Instance UID.

        The early return left both counters at zero, so the release log said
        "no instances received" for an association that had stored several.
        The study UID only decides whether a STUDY BUCKET is updated.
        """
        from clean_code import assoc_tracker as at

        assoc_id = 424242
        at.register_assoc_accepted(assoc_id, "TEST_AE", "127.0.0.1")
        try:
            first = at.record_cstore(assoc_id, "", "CT")
            assert first is False, "an unnamed study is never a 'new study'"
            at.record_cstore(assoc_id, "", "CT")

            info = at._assoc_registry[assoc_id]
            assert (
                info.instance_count == 2
            ), "instances with no StudyInstanceUID must still be counted"
            assert info.studies == {}, "no phantom bucket may be created"
        finally:
            at.register_assoc_released(assoc_id)
            at._assoc_registry.pop(assoc_id, None)


# ---------------------------------------------------------------------------
# utils :288 / :500
# ---------------------------------------------------------------------------
class TestHostNormalisation:
    def test_an_expanded_ipv6_wildcard_is_the_same_peer_as_the_compressed_form(
        self,
    ):
        """`::` mapped to 127.0.0.1 but `0:0:0:0:0:0:0:0` did not."""
        from clean_code.utils import normalize_host

        assert normalize_host("::") == "127.0.0.1"
        assert normalize_host("0:0:0:0:0:0:0:0") == normalize_host("::"), (
            "the expanded spelling of the wildcard must normalise like the "
            "compressed one"
        )

    def test_a_specific_address_is_untouched(self):
        """Control: the wildcard rule must not collapse real addresses."""
        from clean_code.utils import normalize_host

        assert normalize_host("0:0:0:0:0:0:0:1") == "127.0.0.1"  # that IS ::1
        assert normalize_host("10.0.0.5") == "10.0.0.5"


class TestMaskKeyLock:
    def test_the_key_decision_runs_under_an_interprocess_lock(self, tmp_path):
        """A creator that pauses must not let a second process publish its own.

        Two live processes holding DIFFERENT keys mask the same PatientID to
        different tokens, so the logs cannot be correlated exactly when an
        operator is reading both.
        """
        from clean_code import utils

        utils._mask_key_dir_override = str(tmp_path)
        utils._mask_key_cache = None
        try:
            real = utils._load_or_create_key_locked
            order = []

            def _spy(root, path):
                order.append("locked-body")
                return real(root, path)

            utils._load_or_create_key_locked = _spy
            utils._mask_key()

            key_path = tmp_path / ".patient_id_mask.key"
            assert key_path.exists()
            assert order == ["locked-body"]
            # The lock file exists, which is what the context manager holds.
            assert (
                tmp_path / ".patient_id_mask.key.lock"
            ).exists(), "the interprocess lock file must be created beside the key"
        finally:
            utils._load_or_create_key_locked = real
            utils._mask_key_dir_override = ""
            utils._mask_key_cache = None

    def test_two_threads_reach_the_same_key(self, tmp_path):
        """Non-vacuity: the sequence still produces ONE usable key."""
        from clean_code import utils

        utils._mask_key_dir_override = str(tmp_path)
        utils._mask_key_cache = None
        try:
            keys = []
            lock = threading.Lock()

            def _grab():
                value = utils._mask_key()
                with lock:
                    keys.append(value)

            threads = [threading.Thread(target=_grab) for _ in range(6)]
            for t in threads:
                t.start()
            for t in threads:
                t.join()

            assert len(set(keys)) == 1, "every process must settle on one key"
            assert len(keys[0]) == 32
        finally:
            utils._mask_key_dir_override = ""
            utils._mask_key_cache = None


# ---------------------------------------------------------------------------
# health_checker :123 / :230
# ---------------------------------------------------------------------------
class TestHealthChecker:
    def _checker(self, mode="probe"):
        from clean_code.health_checker import HealthChecker

        return HealthChecker(
            clarus_url="http://127.0.0.1:1", mode=mode, interval=999, http_timeout=1
        )

    def _resp(self, status):
        class _R:
            status_code = status

        return _R()

    def test_a_disabled_probe_reports_no_outage_time(self, monkeypatch):
        """503 then 404 exits the poller and reopens STOW admission.

        The healthy Event stays as the 503 left it, so reading it alone
        reported an outage that grew for the life of the process.
        """
        checker = self._checker("probe")
        checker._server_healthy.clear()
        checker._last_ok_time = time.monotonic() - 100

        # A 404 in probe mode disables checks permanently.
        monkeypatch.setattr(
            "clean_code.health_checker.requests.get",
            lambda *a, **k: self._resp(404),
        )
        checker._poll_once(True)
        assert checker._disabled_until_restart is True

        assert checker.dead_seconds() == 0.0, (
            "a disabled checker is not reporting an outage and must not report "
            "outage TIME either"
        )

    def test_a_rejected_probe_clears_the_was_dead_flag(self, monkeypatch):
        """503 then 401 then 503 produced one log entry for two outages.

        The 401 reopens the gate but returning `was_dead` left the flag set, so
        the next 503 took the "already down" path with no new-outage warning.
        """
        checker = self._checker("always")
        monkeypatch.setattr(
            "clean_code.health_checker.requests.get",
            lambda *a, **k: self._resp(401),
        )
        after_rejection = checker._poll_once(True)
        assert after_rejection is False, "the gate was reopened, so the outage is over"

    def test_a_5xx_still_reports_the_outage(self, monkeypatch):
        """Control: a genuine failure keeps the flag and re-reports."""
        checker = self._checker("always")
        monkeypatch.setattr(
            "clean_code.health_checker.requests.get",
            lambda *a, **k: self._resp(503),
        )
        assert checker._poll_once(False) is True


# ---------------------------------------------------------------------------
# http_server :166 / :457
# ---------------------------------------------------------------------------
class TestPrintAttr:
    def test_a_numeric_string_for_a_binary_vr_becomes_an_integer(self):
        """PS3.5 6.2: `"1"` is valid input for US, but the binary writer packs
        an integer and cannot encode text."""
        from clean_code.http_server import _print_attr_value

        assert _print_attr_value("US", "1") == 1
        assert _print_attr_value("UL", "4294967295") == 4294967295

    def test_a_string_vr_keeps_its_text_form(self):
        """Control: IS/DS are decimal text on the wire and must stay strings."""
        from clean_code.http_server import _print_attr_value

        assert _print_attr_value("IS", "1") == "1"
        assert _print_attr_value("DS", "1.5") == "1.5"

    def test_a_value_outside_the_vr_range_is_refused(self):
        from clean_code.http_server import _print_attr_value

        with pytest.raises(ValueError):
            _print_attr_value("US", "70000")
        with pytest.raises(ValueError):
            _print_attr_value("US", "-1")

    def test_a_non_numeric_string_for_a_binary_vr_is_refused(self):
        from clean_code.http_server import _print_attr_value

        with pytest.raises(ValueError):
            _print_attr_value("US", "abc")


class TestHeadTimeoutStatus:
    def test_the_408_is_answered_as_http_1_0_when_the_version_is_unknown(self):
        """The stdlib suppresses status line AND headers in HTTP/0.9 mode, so
        `send_error(408)` emitted no status line at all."""
        import socket

        from clean_code.http_server import make_bridge_handler

        handler_cls = make_bridge_handler(None, None, "BRIDGE")

        class _Sock:
            def __init__(self):
                self.sent = b""

            def sendall(self, data):
                self.sent += data

            def makefile(self, *a, **k):
                return open(os.devnull, "rb")

            def gettimeout(self):
                return None

            def settimeout(self, _v):
                pass

            def shutdown(self, *a):
                pass

            def close(self):
                pass

        class _Req:
            def __init__(self):
                self.raw_requestline = b""
                self.command = None
                self.request_version = "HTTP/0.9"
                self.requestline = ""
                self.headers = None
                self.close_connection = False

        req = _Req()
        sock = _Sock()
        req.connection = sock
        req.default_request_version = "HTTP/0.9"

        seen = {}

        def _send_error(code, message=None, explain=None):
            # The handler must have raised `request_version` out of the 0.9
            # default BEFORE calling send_error: in 0.9 mode the stdlib
            # suppresses the status line and every header, so the peer
            # received a bare body and no 408 at all.
            seen["version"] = req.request_version
            seen["code"] = code

        req.send_error = _send_error
        req.log_error = lambda *a, **k: None
        req.log_request = lambda *a, **k: None

        def _boom(*a, **k):
            raise socket.timeout("head budget")

        req._read_head_with_deadline = _boom

        class _Wfile:
            def __init__(self):
                self.data = b""

            def write(self, b):
                self.data += b

            def flush(self):
                pass

        wfile = _Wfile()
        req.wfile = wfile

        handler_cls.handle_one_request(req)

        assert seen.get("code") == 408, seen
        assert seen.get("version") == "HTTP/1.0", (
            "a head timeout happens before the request version is known, so "
            "the 408 must be answered as HTTP/1.0 - in the HTTP/0.9 default the "
            "stdlib suppresses the status line and every header, and the peer "
            "received no 408 at all (saw %r)" % (seen.get("version"),)
        )


# ---------------------------------------------------------------------------
# mpps :125 / :174 / :249 / :310 / :508
# ---------------------------------------------------------------------------
class TestMppsJson:
    def test_a_ul_private_element_serialises_as_a_json_number(self):
        """PS3.18 Table F.2.3-1: UL maps to Number, not String."""
        from clean_code.handlers.mpps import _element_json
        from pydicom.dataelem import DataElement

        elem = DataElement(0x00710001, "UL", 7)
        out = _element_json(elem)
        assert out["Value"] == [7], out
        assert isinstance(out["Value"][0], int)

    def test_multivalued_empty_positions_are_preserved(self):
        """PS3.18 F.2.5: a multivalued attribute keeps `null` in a position
        that is empty; only a wholly empty attribute omits `Value`."""
        from clean_code.handlers.mpps import _element_json
        from pydicom.dataelem import DataElement

        elem = DataElement(0x00080005, "CS", ["", "ISO 2022 IR 87"])
        out = _element_json(elem)
        assert out["Value"] == [None, "ISO 2022 IR 87"], out

    def test_a_wholly_empty_attribute_has_no_value_member(self):
        """Control for the rule above."""
        from clean_code.handlers.mpps import _element_json
        from pydicom.dataelem import DataElement

        elem = DataElement(0x00080005, "CS", ["", ""])
        assert "Value" not in _element_json(elem)


class TestMppsValidation:
    def test_an_explicitly_empty_final_series_sequence_is_refused(self):
        """PS3.4 F.7.2.2 requires a non-empty Performed Series Sequence once
        the status is final."""
        from clean_code.handlers.mpps import _check_performed_series, _Refusal

        ds = Dataset()
        seq = Dataset()
        seq.PerformedSeriesSequence = []
        ds.PerformedSeriesSequence = seq.PerformedSeriesSequence
        with pytest.raises(_Refusal):
            _check_performed_series(ds, final=True)

    def test_omission_is_still_legal_on_a_partial_update(self):
        """Control: an ABSENT sequence must not be refused, and an empty one
        is still allowed on N-CREATE (the series has not been performed yet).
        """
        from clean_code.handlers.mpps import _check_performed_series

        _check_performed_series(Dataset(), final=True)
        ds = Dataset()
        ds.PerformedSeriesSequence = []
        _check_performed_series(ds, final=False)

    def test_in_progress_must_be_spelled_exactly(self):
        """PS3.4 F.7.2.1 defines the enumerator in upper case; posting the
        lower-case spelling sends the origin a term it may reject."""
        from clean_code.handlers.mpps import _check_status_for_create, _Refusal

        good = Dataset()
        good.PerformedProcedureStepStatus = "IN PROGRESS"
        _check_status_for_create(good)

        bad = Dataset()
        bad.PerformedProcedureStepStatus = "in progress"
        with pytest.raises(_Refusal):
            _check_status_for_create(bad)

    def test_an_invalid_status_create_carries_an_attribute_list(self, monkeypatch):
        """PS3.4 F.7.2.1 requires the Attribute List on a 0106H refusal."""
        from clean_code.handlers import mpps

        class _Event:
            class context:
                abstract_syntax = "1.2.840.10008.3.1.2.3.3"

            class assoc:
                class requestor:
                    ae_title = "MOD"

            class request:
                AffectedSOPInstanceUID = "1.2.3.4"

            attribute_list = None

        ds = Dataset()
        ds.PerformedProcedureStepStatus = "COMPLETED"
        event = _Event()
        event.attribute_list = ds

        monkeypatch.setattr(mpps, "_calling_ae", lambda _e: "MOD")
        status, attrs = mpps.handle_n_create(event, "http://127.0.0.1:1")
        assert status == 0x0106, hex(status)
        assert attrs is not None, "the 0106H refusal must carry an Attribute List"
        assert str(attrs.PerformedProcedureStepStatus) == "IN PROGRESS"


# ---------------------------------------------------------------------------
# streaming :80 / :162 / :233 / :337 / :352
# ---------------------------------------------------------------------------
def _part(boundary, body, ts="1.2.840.10008.1.2.1", extra=None):
    head = (
        f"--{boundary}\r\nContent-Type: application/dicom; "
        f"transfer-syntax={ts}\r\nContent-Length: {len(body)}\r\n"
    )
    if extra:
        head += extra
    return (head + "\r\n").encode("ascii") + body


def _dicom(suffix=b""):
    return b"\x00" * 128 + b"DICM" + suffix


class _FakeResponse:
    def __init__(self, payload, boundary, chunk_size_hint=1):
        self.headers = {"Content-Type": f'multipart/related; boundary="{boundary}"'}
        self._payload = payload
        self._n = chunk_size_hint

    def iter_content(self, chunk_size=1):
        step = max(1, self._n)
        for i in range(0, len(self._payload), step):
            yield self._payload[i : i + step]


class TestStreamingFraming:
    def test_a_negative_content_length_is_refused(self):
        """A negative length inverts the slice arithmetic: the buffer check
        passes and Python returns everything but the last byte."""
        from clean_code.streaming import _parse_content_length

        assert _parse_content_length(b"Content-Length: -6") is None
        assert _parse_content_length(b"Content-Length: +6") is None
        assert _parse_content_length(b"Content-Length: 10") == 10

    def test_consecutive_parts_parse(self):
        """Baseline the other framing tests build on."""
        from clean_code.streaming import _stream_multipart_instances

        b = "bnd42"
        payload = (
            _part(b, _dicom(b"one"))
            + _part(b, _dicom(b"two"))
            + f"\r\n--{b}--\r\n".encode("ascii")
        )
        got = list(_stream_multipart_instances(_FakeResponse(payload, b, 65536)))
        assert len(got) == 2

    def test_a_garbage_delimiter_between_parts_is_a_framing_error(self):
        """The old parser treated the junk as extra header lines, so a
        corrupted delimiter between A and B produced BOTH parts."""
        from clean_code.streaming import (
            MultipartFramingError,
            _stream_multipart_instances,
        )

        b = "bnd42"
        first = _part(b, _dicom(b"one"))
        # Body followed by junk instead of the boundary line.
        payload = first + b"\r\nWRONG\r\nContent-Length: 5\r\n\r\n" + _dicom(b"two")
        with pytest.raises((MultipartFramingError, ValueError)):
            list(_stream_multipart_instances(_FakeResponse(payload, b, 65536)))

    def test_a_corrupted_closing_delimiter_is_not_a_clean_finish(self):
        """`\\r\\n--b--garbage\\r\\n` matched the old prefix test."""
        from clean_code.streaming import (
            MultipartFramingError,
            _stream_multipart_instances,
        )

        b = "bnd42"
        payload = _part(b, _dicom(b"one")) + f"\r\n--{b}--garbage\r\n".encode("ascii")
        with pytest.raises(MultipartFramingError):
            list(_stream_multipart_instances(_FakeResponse(payload, b, 65536)))

    def test_a_delimiter_split_across_chunks_still_parses(self):
        """One chunk ends with `\\r\\n--b` and the next supplies `--\\r\\n`."""
        from clean_code.streaming import _stream_multipart_instances

        b = "bnd42"
        payload = _part(b, _dicom(b"one")) + f"\r\n--{b}--\r\n".encode("ascii")
        # Feed one byte at a time: every delimiter is split somewhere.
        got = list(_stream_multipart_instances(_FakeResponse(payload, b, 1)))
        assert len(got) == 1

    def test_a_separator_inclusive_content_length_is_normalised(self):
        """A MIME implementation may count the trailing CRLF into the length."""
        from clean_code.streaming import _stream_multipart_instances

        b = "bnd42"
        body = _dicom(b"one")
        head = (
            f"--{b}\r\nContent-Type: application/dicom\r\n"
            f"Content-Length: {len(body) + 2}\r\n\r\n"
        ).encode("ascii")
        payload = head + body + f"\r\n--{b}--\r\n".encode("ascii")
        got = list(_stream_multipart_instances(_FakeResponse(payload, b, 65536)))
        assert len(got) == 1
        part = got[0][0]
        assert part == body, "the separator CRLF must not be part of the body"


# ---------------------------------------------------------------------------
# protocol :574 / :611 / :693 / :1244
# ---------------------------------------------------------------------------
class TestMwlFilters:
    def test_a_physician_wildcard_filters_the_row(self):
        """PS3.4 Table K.6-1: Single Value or Wildcard matching."""
        from clean_code.protocol import _mwl_physician_matchers, _mwl_physician_matches

        ds = Dataset()
        sps = Dataset()
        sps.ScheduledPerformingPhysicianName = "Smith*"
        ds.ScheduledProcedureStepSequence = [sps]

        matchers = _mwl_physician_matchers(ds)
        assert matchers, "the physician key must produce a matcher"
        assert _mwl_physician_matches("Smith^John", matchers) is True
        assert _mwl_physician_matches("Jones^Ann", matchers) is False

    def test_the_row_filter_drops_a_non_matching_physician(self, monkeypatch):
        """The matcher is only useful if the SEARCH applies it: a Smith* query
        must not return a Jones row that the origin handed back."""
        from clean_code.handlers import cfind

        rows = [
            {"00400006": {"Value": ["Smith^John"]}, "00080018": {"Value": ["1.2.3.A"]}},
            {"00400006": {"Value": ["Jones^Ann"]}, "00080018": {"Value": ["1.2.3.B"]}},
        ]
        monkeypatch.setattr(
            cfind, "_qido_request", lambda *a, **k: list(rows), raising=False
        )
        monkeypatch.setattr(
            cfind, "_json_to_dataset_mwl", lambda row, kw: row, raising=False
        )
        monkeypatch.setattr(cfind, "service_base_url", lambda *a, **k: "http://x")

        ds = Dataset()
        sps = Dataset()
        sps.ScheduledPerformingPhysicianName = "Smith*"
        ds.ScheduledProcedureStepSequence = [sps]

        class _Event:
            identifier = ds
            is_cancelled = False

        matchers = cfind._mwl_physician_matchers(ds)
        out = list(
            cfind._mwl_search(
                _Event(),
                "http://127.0.0.1:1",
                0,
                None,
                5,
                {},
                None,
                physician_matchers=matchers,
            )
        )
        delivered = [
            emitted[1]["00080018"]["Value"][0]
            for emitted in out
            if emitted[0] == 0xFF00
        ]
        assert delivered == ["1.2.3.A"], out

    def test_an_absent_physician_is_kept(self):
        """Control: Type 2 permits absence, which is not a proven mismatch."""
        from clean_code.protocol import _mwl_physician_matchers, _mwl_physician_matches

        ds = Dataset()
        sps = Dataset()
        sps.ScheduledPerformingPhysicianName = "Smith*"
        ds.ScheduledProcedureStepSequence = [sps]
        matchers = _mwl_physician_matchers(ds)
        assert _mwl_physician_matches("", matchers) is True

    def test_fractional_seconds_in_a_time_range_are_understood(self):
        """`093000.25-093000.75` is a legal TM range; rejecting both bounds
        ran the query with no time filter at all."""
        from clean_code.protocol import _mwl_time_bounds

        bounds = _mwl_time_bounds("093000.25-093000.75")
        assert bounds is not None, "a fractional TM range must be understood"
        assert bounds == ("093000", "093000")
        # Non-vacuity: the plain forms still parse.
        assert _mwl_time_bounds("0900-1000") == ("090000", "100000")

    def test_a_whole_second_upper_bound_covers_the_fraction(self):
        """`{day}235959` is the INSTANT 23:59:59, so .500 fell outside."""
        from clean_code.protocol import _mwl_datetime_in_bounds

        bounds = ("D", "20261121", "20261121", "", "")
        assert _mwl_datetime_in_bounds("20261121235959.500", bounds) is True


class TestScReferences:
    def test_an_item_without_its_instance_uid_is_refused(self):
        """PS3.4 J.3.2 makes both UIDs Type 1. Skipping the item made a
        mixed request lose it silently."""
        from clean_code.protocol import _parse_n_action_rq

        ds = Dataset()
        ds.TransactionUID = "1.2.3"
        item = Dataset()
        item.ReferencedSOPClassUID = "1.2.840.10008.5.1.4.1.1.2"
        ds.ReferencedSOPSequence = [item]

        class _Event:
            dataset = ds

        with pytest.raises(ValueError):
            _parse_n_action_rq(_Event())

    def test_a_complete_reference_list_still_parses(self):
        """Control."""
        from clean_code.protocol import _parse_n_action_rq

        ds = Dataset()
        ds.TransactionUID = "1.2.3"
        item = Dataset()
        item.ReferencedSOPClassUID = "1.2.840.10008.5.1.4.1.1.2"
        item.ReferencedSOPInstanceUID = "1.2.3.4"
        ds.ReferencedSOPSequence = [item]

        class _Event:
            dataset = ds

        tx, sops = _parse_n_action_rq(_Event())
        assert tx == "1.2.3"
        assert sops == [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.4")]


class _ForwardOutbox:
    """Minimal stand-in for the forward OutboxQueue.

    Only the transitions the worker calls are modelled; each records the call
    so a test can assert the UID was NOT counted as settled when the
    transition itself raised.
    """

    def __init__(self, fail_on=()):
        self.delivered = []
        self.failed = []
        self.fail_on = set(fail_on)

    def mark_delivered(self, uid):
        if "delivered" in self.fail_on:
            raise RuntimeError("mark_delivered refused")
        self.delivered.append(uid)

    def mark_failed(self, uid, reason):
        if "failed" in self.fail_on:
            raise RuntimeError("mark_failed refused")
        self.failed.append((uid, reason))


class _ForwardAssoc:
    """A fake association holding one accepted presentation context.

    `_forward_process_batch` hands `send_c_store` the path of the stored blob,
    so the UID is recovered from the file name (`<uid>.dcm`).
    """

    def __init__(self, statuses, established_after=()):
        self.accepted_contexts = [_ForwardContext()]
        self._statuses = dict(statuses)
        self._established_after = set(established_after)
        self.is_established = True
        self.sent = []

    def send_c_store(self, path, context=None, msg_id=1, priority=2):
        name = os.path.basename(str(path))
        uid = name[:-4] if name.endswith(".dcm") else name
        self.sent.append(uid)
        if uid in self._established_after:
            self.is_established = False
        return self._statuses.get(uid)


class _ForwardContext:
    abstract_syntax = "1.2.840.10008.5.1.4.1.1.7"
    transfer_syntax = ["1.2.840.10008.1.2.1"]


class TestForwardSettlement:
    """The forward drain must settle a UID only on a completed transition."""

    def _run(self, monkeypatch, statuses, fail_on=(), established_after=()):
        from clean_code import main as bridge_main

        outbox = _ForwardOutbox(fail_on=fail_on)
        assoc = _ForwardAssoc(statuses, established_after=established_after)

        monkeypatch.setattr(
            bridge_main, "_forward_open_assoc", lambda *a, **k: assoc, raising=False
        )
        monkeypatch.setattr(
            bridge_main, "_forward_blob_size", lambda _p: 0, raising=False
        )
        blob = os.path.join(os.path.dirname(__file__), "conftest.py")
        items = [
            (uid, "1.2.840.10008.5.1.4.1.1.7", "1.2.840.10008.1.2.1", blob)
            for uid in statuses
        ]
        settled = set()
        ok = bridge_main._forward_process_batch(
            outbox,
            items,
            "DEST",
            "BRIDGE",
            "127.0.0.1",
            104,
            10,
            10,
            10,
            settled=settled,
        )
        return ok, settled, outbox, assoc

    def test_a_raising_mark_failed_leaves_the_uid_unsettled(
        self, monkeypatch, tmp_path
    ):
        """A transition that raised did not happen, so the caller must still
        requeue the item - settling it left it in in_progress/ with no retry."""
        from clean_code import main as bridge_main

        outbox = _ForwardOutbox(fail_on=("failed",))
        items = [("1.2.3.A", "", "", str(_blob(tmp_path, "1.2.3.A")))]

        settled = set()
        monkeypatch.setattr(
            bridge_main, "_forward_open_assoc", lambda *a, **k: None, raising=False
        )
        bridge_main._forward_process_batch(
            outbox,
            items,
            "DEST",
            "BRIDGE",
            "127.0.0.1",
            104,
            10,
            10,
            10,
            settled=settled,
        )
        assert "1.2.3.A" not in settled

    def test_a_raising_mark_delivered_leaves_the_uid_unsettled(
        self, monkeypatch, tmp_path
    ):
        """Same rule on the success path: mark_delivered raising must not count."""
        from clean_code import main as bridge_main

        outbox = _ForwardOutbox(fail_on=("delivered",))
        assoc = _ForwardAssoc({"1.2.3.A": _rsp(0x0000)})
        monkeypatch.setattr(
            bridge_main, "_forward_open_assoc", lambda *a, **k: assoc, raising=False
        )
        monkeypatch.setattr(
            bridge_main, "_forward_blob_size", lambda _p: 0, raising=False
        )
        items = [
            (
                "1.2.3.A",
                "1.2.840.10008.5.1.4.1.1.7",
                "1.2.840.10008.1.2.1",
                str(_blob(tmp_path, "1.2.3.A")),
            )
        ]
        settled = set()
        bridge_main._forward_process_batch(
            outbox,
            items,
            "DEST",
            "BRIDGE",
            "127.0.0.1",
            104,
            10,
            10,
            10,
            settled=settled,
        )
        assert "1.2.3.A" not in settled

    def test_a_send_exception_on_a_closed_association_returns_false(
        self, monkeypatch, tmp_path
    ):
        """A send that RAISED because the peer closed must abandon the batch.

        The error path used to mark the exception "handled", requeue nothing
        and return True, so the worker retried at once without the
        association-failure pause and hammered a peer that had already gone.
        """
        from clean_code import main as bridge_main

        class _Assoc(_ForwardAssoc):
            def send_c_store(self, path, context=None, msg_id=1, priority=2):
                self.is_established = False
                raise OSError("connection reset by peer")

        outbox = _ForwardOutbox()
        assoc = _Assoc({})
        monkeypatch.setattr(
            bridge_main, "_forward_open_assoc", lambda *a, **k: assoc, raising=False
        )
        monkeypatch.setattr(
            bridge_main, "_forward_blob_size", lambda _p: 0, raising=False
        )
        items = [
            (
                uid,
                "1.2.840.10008.5.1.4.1.1.7",
                "1.2.840.10008.1.2.1",
                str(_blob(tmp_path, uid)),
            )
            for uid in ("1.2.3.A", "1.2.3.B")
        ]
        settled = set()
        ok = bridge_main._forward_process_batch(
            outbox,
            items,
            "DEST",
            "BRIDGE",
            "127.0.0.1",
            104,
            10,
            10,
            10,
            settled=settled,
        )

        assert ok is False, "a closed association must be retried after a pause"
        assert settled == set(), "nothing was delivered, so nothing is settled"

    def test_a_lost_response_hands_back_the_rest(self, monkeypatch, tmp_path):
        """A None C-STORE-RSP with the association gone is a transport
        failure: the rest of the batch goes back uncharged."""
        from clean_code import main as bridge_main

        class _Assoc(_ForwardAssoc):
            def send_c_store(self, path, context=None, msg_id=1, priority=2):
                self.is_established = False
                return None

        outbox = _ForwardOutbox()
        assoc = _Assoc({})
        monkeypatch.setattr(
            bridge_main, "_forward_open_assoc", lambda *a, **k: assoc, raising=False
        )
        monkeypatch.setattr(
            bridge_main, "_forward_blob_size", lambda _p: 0, raising=False
        )
        items = [
            (
                uid,
                "1.2.840.10008.5.1.4.1.1.7",
                "1.2.840.10008.1.2.1",
                str(_blob(tmp_path, uid)),
            )
            for uid in ("1.2.3.A", "1.2.3.B")
        ]
        settled = set()
        ok = bridge_main._forward_process_batch(
            outbox,
            items,
            "DEST",
            "BRIDGE",
            "127.0.0.1",
            104,
            10,
            10,
            10,
            settled=settled,
        )
        assert ok is False
        assert "1.2.3.A" not in settled
        assert "1.2.3.B" not in settled
        assert outbox.failed == []

    def test_an_unknown_warning_status_is_not_delivered(self, monkeypatch, tmp_path):
        """PS3.4 Table B.2-1 enumerates the C-STORE warnings; 0xB007 is one but
        means "not stored as sent", and an undefined code is not a delivery.
        Both take the FAILURE path, so they are failed rather than delivered."""
        from clean_code import main as bridge_main

        outbox = _ForwardOutbox()
        assoc = _ForwardAssoc({"1.2.3.A": _rsp(0xB007), "1.2.3.B": _rsp(0xB0FF)})
        monkeypatch.setattr(
            bridge_main, "_forward_open_assoc", lambda *a, **k: assoc, raising=False
        )
        monkeypatch.setattr(
            bridge_main, "_forward_blob_size", lambda _p: 0, raising=False
        )
        items = [
            (
                uid,
                "1.2.840.10008.5.1.4.1.1.7",
                "1.2.840.10008.1.2.1",
                str(_blob(tmp_path, uid)),
            )
            for uid in ("1.2.3.A", "1.2.3.B")
        ]
        settled = set()
        bridge_main._forward_process_batch(
            outbox,
            items,
            "DEST",
            "BRIDGE",
            "127.0.0.1",
            104,
            10,
            10,
            10,
            settled=settled,
        )
        assert outbox.delivered == []
        assert [uid for uid, _reason in outbox.failed] == ["1.2.3.A", "1.2.3.B"]

    def test_a_defined_warning_counts_as_delivered(self, monkeypatch, tmp_path):
        """0xB000 (coercion) and 0xB006 (element discarded) are SUCCESS: the
        instance WAS stored, so retrying it parks a delivered blob."""
        from clean_code import main as bridge_main

        outbox = _ForwardOutbox()
        assoc = _ForwardAssoc({"1.2.3.A": _rsp(0xB000), "1.2.3.B": _rsp(0xB006)})
        monkeypatch.setattr(
            bridge_main, "_forward_open_assoc", lambda *a, **k: assoc, raising=False
        )
        monkeypatch.setattr(
            bridge_main, "_forward_blob_size", lambda _p: 0, raising=False
        )
        items = [
            (
                uid,
                "1.2.840.10008.5.1.4.1.1.7",
                "1.2.840.10008.1.2.1",
                str(_blob(tmp_path, uid)),
            )
            for uid in ("1.2.3.A", "1.2.3.B")
        ]
        settled = set()
        bridge_main._forward_process_batch(
            outbox,
            items,
            "DEST",
            "BRIDGE",
            "127.0.0.1",
            104,
            10,
            10,
            10,
            settled=settled,
        )
        assert outbox.delivered == ["1.2.3.A", "1.2.3.B"]
        assert settled == {"1.2.3.A", "1.2.3.B"}


def _blob(tmp_path, uid):
    """A stand-in stored blob; the worker only ever reads its size and name."""
    path = tmp_path / (uid + ".dcm")
    path.write_bytes(b"\x00" * 132)
    return path


def _rsp(status):
    from pydicom.dataset import Dataset as _Ds

    ds = _Ds()
    ds.Status = status
    return ds
