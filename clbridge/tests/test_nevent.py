# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Tests for Stage 2: CommitTracker resolution + N-EVENT-REPORT SCU sender.

Covers:
- CommitTracker: resolve-on-all-confirmed (EventTypeID=1), deadline failure
  (EventTypeID=2, unconfirmed -> FailureReason 0110H), verify errors do not
  kill the loop, close() stops the worker.
- nevent.send_n_event_report: builds the correct N-EVENT-REPORT Dataset and
  sends it over a separate SCU association (mocked AE/assoc).
"""

import threading
import time
from unittest.mock import MagicMock, patch

from clean_code.commit_tracker import CommitTracker

# ---------------------------------------------------------------------------
# CommitTracker
# ---------------------------------------------------------------------------


class TestCommitTracker:
    def _fake_report(self):
        calls = []

        def report(host, port, ae, tx_uid, event_type, ok, failed):
            calls.append((host, port, ae, tx_uid, event_type, ok, failed))

        return calls, report

    def test_resolves_immediately_when_all_confirmed(self):
        """All SOPs confirmed on first tick -> ONE report, event_type=1."""
        calls, report = self._fake_report()
        verify = MagicMock(return_value=True)
        tracker = CommitTracker(
            verify=verify,
            report=report,
            interval=1.0,
            deadline_sec=300.0,
            now=time.time,
        )
        tracker.register(
            "TX1",
            "MODALITY",
            "10.0.0.5",
            11112,
            [("1.2.3", "1.2.3.1"), ("1.2.3", "1.2.3.2")],
        )
        tracker._tick()

        assert len(calls) == 1
        host, port, ae, tx, ev, ok, failed = calls[0]
        assert (host, port, ae, tx, ev) == ("10.0.0.5", 11112, "MODALITY", "TX1", 1)
        assert len(ok) == 2
        assert failed == []
        # Job removed after resolution.
        assert tracker.pending_count() == 0

    def test_deadline_passes_with_unconfirmed_reports_failure(self):
        """Unconfirmed SOP at 300s deadline -> event_type=2, reason 0110H.

        Covers the spec: a hard 300s timeout -> N-EVENT-REPORT failure for
        SOPs never confirmed, so the modality keeps its local copy.
        """
        t0 = [1000.0]
        calls, report = self._fake_report()

        def verify(sop_class, sop_uid):
            return sop_uid == "1.2.3.delivered"

        tracker = CommitTracker(
            verify=verify,
            report=report,
            interval=1.0,
            deadline_sec=300.0,
            now=lambda: t0[0],
        )
        tracker.register(
            "TX2",
            "MODALITY",
            "10.0.0.5",
            11112,
            [
                ("1.2.3", "1.2.3.delivered"),
                ("1.2.3", "1.2.3.lost"),
            ],
        )

        # Before deadline: reported SOP confirmed, lost one still pending ->
        # not resolved yet.
        tracker._tick()
        assert tracker.pending_count() == 1

        # Advance past 300s deadline -> resolve with failure for the lost SOP.
        t0[0] += 301.0
        tracker._tick()
        assert tracker.pending_count() == 0
        assert len(calls) == 1
        ev = calls[0][4]
        ok = calls[0][5]
        failed = calls[0][6]
        assert ev == 2  # Storage Commitment Complete - Failures Exist
        assert [u for _, u in ok] == ["1.2.3.delivered"]
        assert [u for _, u, r in failed] == ["1.2.3.lost"]
        assert failed[0][2] == 0x0110

    def test_verify_exception_does_not_kill_loop(self):
        """A raising verify callback is caught; remaining SOPs retry next tick."""
        calls, report = self._fake_report()
        blows = [True]

        def verify(sop_class, sop_uid):
            if blows[0]:
                blows[0] = False
                raise RuntimeError("boom")
            return True

        tracker = CommitTracker(
            verify=verify,
            report=report,
            interval=1.0,
            deadline_sec=300.0,
            now=time.time,
        )
        tracker.register("TX3", "MODALITY", "10.0.0.5", 11112, [("1.2.3", "1.2.3.1")])
        tracker._tick()  # verify raises -> not resolved, still pending
        assert tracker.pending_count() == 1
        tracker._tick()  # verify works -> resolved
        assert tracker.pending_count() == 0
        assert len(calls) == 1

    def test_close_stops_worker(self):
        """close() sets the stop event; run() returns."""
        tracker = CommitTracker(verify=lambda c, u: True, report=lambda *a, **k: None)
        t = threading.Thread(target=tracker.run, daemon=True)
        t.start()
        tracker.close()
        t.join(timeout=5)
        assert not t.is_alive()


# ---------------------------------------------------------------------------
# nevent.send_n_event_report
# ---------------------------------------------------------------------------


class TestSendNEventReport:
    @staticmethod
    def _make_assoc(established=True, sc_accepted=True):
        assoc = MagicMock()
        assoc.is_established = established
        if sc_accepted:
            ctx = MagicMock()
            type(ctx).abstract_syntax = "1.2.840.10008.1.20.1"
            assoc.accepted_contexts = [ctx]
        else:
            assoc.accepted_contexts = []
        # pynetdicom 3.x: send_n_event_report returns (status, response_dataset)
        assoc.send_n_event_report.return_value = (MagicMock(Status=0x0000), None)
        return assoc

    def test_sends_report_with_success_and_failed_sops(self):
        """Builds/report: EventTypeID=2, TransactionUID, Referenced + Failed seq."""
        from clean_code.nevent import send_n_event_report

        assoc = self._make_assoc()
        with patch("clean_code.nevent.AE") as mock_ae_cls:
            mock_ae = MagicMock()
            mock_ae.associate.return_value = assoc
            mock_ae_cls.return_value = mock_ae

            ok = send_n_event_report(
                "10.0.0.5",
                11112,
                "MODALITY",
                "TX9",
                2,
                success_sops=[("1.2.3", "1.2.3.ok")],
                failed_sops=[("1.2.3", "1.2.3.bad", 0x0110)],
                bridge_ae_title="TEST_BRIDGE",
            )

        assert ok is True
        # The commitment report travels under the configured AE title: a
        # modality rejects a report from an AE it did not receive the request
        # from, so the caller must supply it (no default).
        mock_ae_cls.assert_called_with(ae_title="TEST_BRIDGE")
        sent = assoc.send_n_event_report.call_args[0][0]
        # EventTypeID (0000,1002) is a command-set element: it travels as the
        # separate `event_type` argument, NOT inside the Event Information
        # data set.
        assert assoc.send_n_event_report.call_args[0][1] == 2
        assert "EventTypeID" not in sent, "EventTypeID is a command-set element"
        assert sent.TransactionUID == "TX9"
        ref = list(sent.ReferencedSOPSequence)
        assert len(ref) == 1
        assert ref[0].ReferencedSOPInstanceUID == "1.2.3.ok"
        failing = list(sent.FailedSOPSequence)
        assert len(failing) == 1
        assert failing[0].ReferencedSOPInstanceUID == "1.2.3.bad"
        assert failing[0].FailureReason == 0x0110

    def test_success_event_only_referenced_sequence(self):
        """ALL confirmed -> EventTypeID=1, no FailedSOPSequence."""
        from clean_code.nevent import send_n_event_report

        assoc = self._make_assoc()
        with patch("clean_code.nevent.AE") as mock_ae_cls:
            mock_ae = MagicMock()
            mock_ae.associate.return_value = assoc
            mock_ae_cls.return_value = mock_ae

            send_n_event_report(
                "10.0.0.5",
                11112,
                "MODALITY",
                "TX10",
                1,
                success_sops=[("1.2.3", "1.2.3.ok")],
                failed_sops=[],
                bridge_ae_title="TEST_BRIDGE",
            )
        sent = assoc.send_n_event_report.call_args[0][0]
        # EventTypeID travels in the command set, not the data set.
        assert assoc.send_n_event_report.call_args[0][1] == 1
        assert "EventTypeID" not in sent, "EventTypeID is a command-set element"
        assert not hasattr(sent, "FailedSOPSequence")

    def test_bridge_ae_title_is_required(self):
        """No default: the report must travel under the configured AE title.

        A default here silently pushes every commitment report as
        CLARUS_BRIDGE whatever [server] ae_title says, and the modality
        rejects a report from an AE it never sent the request to.
        """
        import inspect

        from clean_code.nevent import send_n_event_report

        param = inspect.signature(send_n_event_report).parameters["bridge_ae_title"]
        assert param.default is inspect.Parameter.empty

    def test_returns_false_when_scp_unavailable(self, caplog):
        """Association never establishes after retries -> False, logs warn."""
        from clean_code.nevent import send_n_event_report

        with patch("clean_code.nevent.AE") as mock_ae_cls, patch(
            "clean_code.nevent.time.sleep"
        ) as mock_sleep:
            mock_ae = MagicMock()
            mock_ae.associate.return_value = MagicMock(is_established=False)
            mock_ae_cls.return_value = mock_ae
            ok = send_n_event_report(
                "10.0.0.5",
                11112,
                "MODALITY",
                "TX11",
                1,
                success_sops=[("1.2.3", "1.2.3.ok")],
                failed_sops=[],
                bridge_ae_title="TEST_BRIDGE",
                max_retries=2,
            )
        assert ok is False
        assert mock_ae.associate.call_count == 3  # initial + 2 retries
        assert mock_sleep.call_count == 2
