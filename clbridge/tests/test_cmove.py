# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Unit tests for clarus_bridge.handlers.cmove — C-MOVE request parsing
and transfer syntax negotiation."""

import pytest
from clean_code.handlers.cmove import (
    _associate_for_move,
    _parse_c_move_rq,
)
from pydicom import Dataset


class FakeEvent:
    """Fake pynetdicom event for testing C-MOVE handlers."""

    def __init__(self, identifier):
        self.identifier = identifier
        # pynetdicom provides move_destination as a direct event attribute
        self.move_destination = getattr(identifier, "MoveDestination", None)


class TestParseCMoveRq:
    """Tests for _parse_c_move_rq()."""

    def test_missing_move_destination_raises_value_error(self):
        ds = Dataset()
        ds.StudyInstanceUID = "1.2.3.4"
        # MoveDestination missing
        event = FakeEvent(ds)
        with pytest.raises(ValueError, match="MoveDestination"):
            _parse_c_move_rq(event)

    def test_missing_study_uid_raises_value_error(self):
        ds = Dataset()
        ds.MoveDestination = "DEST_AET"
        # StudyInstanceUID missing
        event = FakeEvent(ds)
        with pytest.raises(ValueError, match="StudyInstanceUID"):
            _parse_c_move_rq(event)

    def test_valid_rq_parsed_correctly(self):
        ds = Dataset()
        ds.StudyInstanceUID = "1.2.3.4.5.6"
        ds.MoveDestination = "DEST_AET"
        event = FakeEvent(ds)
        study_uids, dest, level, series_uid, sop_uid = _parse_c_move_rq(event)
        assert study_uids == ["1.2.3.4.5.6"]
        assert dest == "DEST_AET"
        # Absent QueryRetrieveLevel means STUDY scope (the historical meaning).
        assert level == ""
        assert series_uid == ""
        assert sop_uid == ""

    def test_series_level_carries_the_series_scope(self):
        """PS3.4 C.4.2.1.2: a SERIES-level C-MOVE names SeriesInstanceUID."""
        ds = Dataset()
        ds.QueryRetrieveLevel = "SERIES"
        ds.StudyInstanceUID = "1.2.3"
        ds.SeriesInstanceUID = "1.2.3.4"
        ds.MoveDestination = "DEST_AET"
        _study, _dest, level, series_uid, sop_uid = _parse_c_move_rq(FakeEvent(ds))
        assert (level, series_uid, sop_uid) == ("SERIES", "1.2.3.4", "")

    def test_image_level_carries_series_and_instance(self):
        ds = Dataset()
        ds.QueryRetrieveLevel = "IMAGE"
        ds.StudyInstanceUID = "1.2.3"
        ds.SeriesInstanceUID = "1.2.3.4"
        ds.SOPInstanceUID = "1.2.3.4.5"
        ds.MoveDestination = "DEST_AET"
        _study, _dest, level, series_uid, sop_uid = _parse_c_move_rq(FakeEvent(ds))
        assert (level, series_uid, sop_uid) == ("IMAGE", "1.2.3.4", "1.2.3.4.5")

    def test_series_level_without_a_series_uid_is_refused(self):
        """Refuse rather than fall back to whole-study retrieval: the SCU
        would receive data it never asked for."""
        ds = Dataset()
        ds.QueryRetrieveLevel = "SERIES"
        ds.StudyInstanceUID = "1.2.3"
        ds.MoveDestination = "DEST_AET"
        with pytest.raises(ValueError, match="SeriesInstanceUID"):
            _parse_c_move_rq(FakeEvent(ds))

    def test_an_unknown_level_is_refused(self):
        ds = Dataset()
        ds.QueryRetrieveLevel = "PATIENT"
        ds.StudyInstanceUID = "1.2.3"
        ds.MoveDestination = "DEST_AET"
        with pytest.raises(ValueError, match="QueryRetrieveLevel"):
            _parse_c_move_rq(FakeEvent(ds))


# ---------------------------------------------------------------------------
# P31: Tests for _associate_for_move retry on ACSE timeout
# ---------------------------------------------------------------------------


class FakeAssoc:
    """Fake pynetdicom Association for testing."""

    def __init__(self, is_established=True):
        self.is_established = is_established
        self.accepted_contexts = []


class TestAssociateForMoveRetry:
    """0609: _associate_for_move - outbound_assoc_attempts knob (default 3),
    immediate retries (no sleep) on the not-established return shape;
    exceptions still propagate without retry."""

    def test_retries_until_established_immediate(self, monkeypatch):
        """First attempt not established -> immediate retries -> success on
        the 2nd of the default attempts (no sleep between them)."""
        calls = []
        sleeps = []

        def fake_associate(self_ae, host, port, ae_title):
            calls.append(1)
            if len(calls) == 1:
                return FakeAssoc(is_established=False)
            return FakeAssoc(is_established=True)

        monkeypatch.setattr("pynetdicom.ae.ApplicationEntity.associate", fake_associate)
        monkeypatch.setattr("time.sleep", lambda s: sleeps.append(s))

        assoc, _ = _associate_for_move(
            "TEST_AE",
            "BRIDGE_AE",
            None,
            "127.0.0.1",
            11112,
            acse_timeout=5,
        )
        assert assoc.is_established
        assert len(calls) == 2, "one immediate retry then success"
        assert sleeps == [], "immediate retries: no sleep"

    def test_raises_cannot_associate_after_attempts_exhausted(self, monkeypatch):
        """All default attempts (3) not established -> RuntimeError."""
        calls = []

        def fake_associate(self_ae, host, port, ae_title):
            calls.append(1)
            return FakeAssoc(is_established=False)

        monkeypatch.setattr("pynetdicom.ae.ApplicationEntity.associate", fake_associate)
        monkeypatch.setattr("time.sleep", lambda _s: None)

        with pytest.raises(RuntimeError, match="Cannot associate"):
            _associate_for_move(
                "TEST_AE",
                "BRIDGE_AE",
                None,
                "127.0.0.1",
                11112,
                acse_timeout=5,
            )
        assert len(calls) == 3, "3 attempts (outbound_assoc_attempts default)"

    def test_attempts_knob_one_attempt(self, monkeypatch):
        """outbound_assoc_attempts=1 -> exactly one A-ASSOCIATE, no retry."""
        calls = []

        def fake_associate(self_ae, host, port, ae_title):
            calls.append(1)
            return FakeAssoc(is_established=False)

        monkeypatch.setattr("pynetdicom.ae.ApplicationEntity.associate", fake_associate)
        with pytest.raises(RuntimeError, match="Cannot associate"):
            _associate_for_move(
                "TEST_AE",
                "BRIDGE_AE",
                None,
                "127.0.0.1",
                11112,
                acse_timeout=5,
                outbound_assoc_attempts=1,
            )
        assert len(calls) == 1, "attempts=1 => single attempt"

    def test_raises_immediately_on_associate_exception(self, monkeypatch):
        """An associate() EXCEPTION propagates without retry."""
        calls = []

        def fake_associate(self_ae, host, port, ae_title):
            calls.append(1)
            raise RuntimeError("ACSE timeout")

        monkeypatch.setattr("pynetdicom.ae.ApplicationEntity.associate", fake_associate)
        monkeypatch.setattr("time.sleep", lambda _s: None)

        with pytest.raises(RuntimeError, match="ACSE timeout"):
            _associate_for_move(
                "TEST_AE",
                "BRIDGE_AE",
                None,
                "127.0.0.1",
                11112,
                acse_timeout=5,
            )
        assert len(calls) == 1, "no retry on exception"

    def test_succeeds_on_first_attempt(self, monkeypatch):
        """Normal case: first attempt succeeds."""
        call_count = [0]

        def fake_associate(self_ae, host, port, ae_title):
            call_count[0] += 1
            return FakeAssoc(is_established=True)

        monkeypatch.setattr("pynetdicom.ae.ApplicationEntity.associate", fake_associate)

        assoc, ctx_map = _associate_for_move(
            "TEST_AE",
            "BRIDGE_AE",
            None,
            "127.0.0.1",
            11112,
            acse_timeout=5,
        )
        assert assoc.is_established
        assert call_count[0] == 1  # no retries needed

    def test_returns_accepted_contexts_map_set_valued(self, monkeypatch):
        """Accepted contexts become a SET-valued map (R3)."""

        def fake_associate(self_ae, host, port, ae_title):
            assoc = FakeAssoc(is_established=True)
            from pynetdicom.presentation import PresentationContext

            ctx = PresentationContext()
            ctx.abstract_syntax = "1.2.840.10008.5.1.4.1.1.2"
            ctx.transfer_syntax = ["1.2.840.10008.1.2.1"]
            ctx.result = 0  # acceptance
            assoc.accepted_contexts = [ctx]
            return assoc

        monkeypatch.setattr("pynetdicom.ae.ApplicationEntity.associate", fake_associate)

        assoc, ctx_map = _associate_for_move(
            "TEST_AE",
            "BRIDGE_AE",
            None,
            "127.0.0.1",
            11112,
            acse_timeout=5,
        )
        assert "1.2.840.10008.5.1.4.1.1.2" in ctx_map
        assert ctx_map["1.2.840.10008.5.1.4.1.1.2"] == {"1.2.840.10008.1.2.1"}

    def test_terminal_transport_oserror_no_retry(self, monkeypatch):
        """C2: associate() raising an OSError-family (e.g. socket.gaierror on
        DNS) is a TERMINAL transport - exactly ONE ae.associate call, an
        immediate RuntimeError, and ZERO retries."""
        import socket

        calls = []

        def fake_associate(self_ae, host, port, ae_title):
            calls.append(1)
            raise socket.gaierror("[Errno 11001] getaddrinfo failed")

        monkeypatch.setattr("pynetdicom.ae.ApplicationEntity.associate", fake_associate)
        monkeypatch.setattr("time.sleep", lambda _s: None)

        with pytest.raises(RuntimeError, match="terminal transport"):
            _associate_for_move(
                "TEST_AE",
                "BRIDGE_AE",
                None,
                "127.0.0.1",
                11112,
                acse_timeout=5,
                outbound_assoc_attempts=3,
            )
        assert len(calls) == 1, "terminal transport must not retry"

    def test_terminal_connection_refused_no_retry(self, monkeypatch):
        """C2: a ConnectionRefusedError raised by associate() is terminal -
        immediate RuntimeError with ZERO retries."""
        calls = []

        def fake_associate(self_ae, host, port, ae_title):
            calls.append(1)
            raise ConnectionRefusedError("Connection refused")

        monkeypatch.setattr("pynetdicom.ae.ApplicationEntity.associate", fake_associate)
        with pytest.raises(RuntimeError, match="terminal transport"):
            _associate_for_move(
                "TEST_AE",
                "BRIDGE_AE",
                None,
                "127.0.0.1",
                11112,
                acse_timeout=5,
                outbound_assoc_attempts=5,
            )
        assert len(calls) == 1, "terminal transport must not retry regardless of knob"


# ---------------------------------------------------------------------------
