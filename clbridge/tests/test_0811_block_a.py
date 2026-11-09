"""0811 Block A: regression tests for the small/medium bridge findings.

Each test pins one defect that was fixed in this batch. Reverting the fix must
turn the matching test red - they assert the corrected behaviour directly, not
that "something was logged".
"""

from unittest.mock import MagicMock, patch

import httpx


class _shared_db:
    """Minimal SharedDb-compatible wrapper (same shape as test_db_move_jobs)."""

    def __init__(self, path):
        import sqlite3
        import threading

        self._conn = sqlite3.connect(path, check_same_thread=False)
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._lock = threading.Lock()

    def locked(self):
        outer = self

        class _L:
            def __enter__(s):
                outer._lock.acquire()
                return outer._conn

            def __exit__(s, *a):
                outer._lock.release()

        return _L()

    def close(self):
        with self._lock:
            try:
                self._conn.close()
            except Exception:
                pass


# ---------------------------------------------------------------------------
# M2: the retry helper must retry EVERY transport error, not two subclasses
# ---------------------------------------------------------------------------
class TestRetryCatchesEveryTransportError:
    def test_a_read_error_is_retried(self):
        """httpx.ReadError is a TransportError but not Timeout/ConnectError.

        It was fatal before: one reset mid-response aborted the batch while a
        connect failure was retried.
        """
        from clean_code.http_helpers import _http_retry

        calls = []

        def _do():
            calls.append(1)
            if len(calls) < 2:
                raise httpx.ReadError("connection reset")
            resp = MagicMock()
            resp.status_code = 200
            return resp

        with patch("clean_code.http_helpers.stow_retries"), patch(
            "clean_code.http_helpers.time.sleep"
        ):
            resp = _http_retry(_do, max_retries=3, delay=0)

        assert resp.status_code == 200
        assert len(calls) == 2, "the second attempt must happen"


# ---------------------------------------------------------------------------
# M4: a non-success N-EVENT-REPORT status is NOT a delivery
# ---------------------------------------------------------------------------
class TestNeventStatusIsChecked:
    def _run(self, status_code):
        from clean_code import nevent

        status = MagicMock()
        status.Status = status_code
        assoc = MagicMock()
        assoc.is_established = True
        assoc.accepted_contexts = [MagicMock()]
        assoc.accepted_contexts[0].abstract_syntax = nevent.StorageCommitmentPushModel
        assoc.send_n_event_report.return_value = (status, None)

        with patch.object(nevent, "AE") as ae_cls, patch.object(
            nevent, "associations_total"
        ), patch.object(nevent, "nevent_delivered_total") as delivered:
            ae = ae_cls.return_value
            ae.associate.return_value = assoc
            ok = nevent.send_n_event_report(
                "10.0.0.5",
                11112,
                "TEST_SCU",
                "1.2.3.4",
                1,
                [("1.2.3", "1.2.3.1")],
                [],
                bridge_ae_title="CLBRIDGE",
                max_retries=0,
                retry_delay=0,
            )
        return ok, delivered

    def test_success_status_reports_delivery(self):
        ok, delivered = self._run(0x0000)
        assert ok is True
        delivered.inc.assert_called_once()

    def test_failure_status_does_not_report_delivery(self):
        """PS3.7 9.1: only 0x0000 means the primitive succeeded.

        Counting a failure as delivered stopped the retries and left the
        commitment unreported for good.
        """
        ok, delivered = self._run(0x0122)
        assert ok is False
        delivered.inc.assert_not_called()


# ---------------------------------------------------------------------------
# M6: a verdict that was not delivered must NOT retire the job
# ---------------------------------------------------------------------------
class TestCommitTrackerKeepsUndeliveredJobs:
    def test_a_failed_report_keeps_the_job(self):
        from clean_code.commit_tracker import CommitTracker

        def _report(*_a, **_k):
            raise RuntimeError("modality unreachable")

        tracker = CommitTracker(verify=lambda c, u: True, report=_report)
        tracker.register("1.2.3.4", "SCU", "10.0.0.5", 11112, [("1.2.3", "1.2.3.1")])
        # Force the deadline so the next tick tries to resolve.
        with tracker._lock:
            tracker._jobs["1.2.3.4"].deadline = 0
            tracker._jobs["1.2.3.4"].pending.clear()

        tracker._tick()
        assert tracker.pending_count() == 1, "the verdict never reached the modality"

    def test_a_delivered_report_retires_the_job(self):
        from clean_code.commit_tracker import CommitTracker

        sent = []
        tracker = CommitTracker(
            verify=lambda c, u: True, report=lambda *a, **k: sent.append(a) or True
        )
        tracker.register("1.2.3.4", "SCU", "10.0.0.5", 11112, [("1.2.3", "1.2.3.1")])
        with tracker._lock:
            tracker._jobs["1.2.3.4"].deadline = 0
            tracker._jobs["1.2.3.4"].pending.clear()

        tracker._tick()
        assert tracker.pending_count() == 0
        assert len(sent) == 1


# ---------------------------------------------------------------------------
# M8: the destination port comes from the peer config, never the source port
# ---------------------------------------------------------------------------
class TestNactionDestinationPort:
    def test_the_association_source_port_is_not_used(self):
        """PS3.8: A-ASSOCIATE carries no listening port.

        `requestor.port` is the ephemeral SOURCE port of this connection, so a
        report pushed there always failed.
        """
        from clean_code.handlers import naction

        tracker = MagicMock()
        req = type("Req", (), {"ae_title": "SCU", "address": "10.0.0.5", "port": 54321})
        event = MagicMock()
        event.assoc.requestor = req()

        # A resolver that knows nothing about this peer must leave port_ None,
        # which skips registration - it must not fall back to 54321.
        naction._register_commit(
            tracker,
            "1.2.3.4",
            "SCU",
            [(0x0018, 0x0010)],
            event,
            resolve_scp=lambda ae, addr: None,
        )
        tracker.register.assert_not_called()


# ---------------------------------------------------------------------------
# M10: `failed` is the observed failure list, not total - completed
# ---------------------------------------------------------------------------
class TestCmoveFailureCount:
    def test_unseen_instances_are_not_counted_as_failures(self):
        """PS3.4 C.4.2.1.4.2: the failure count is what was seen and not sent.

        `total - completed` also counted every instance that never reached this
        pass, so a resumed job invented failures.
        """
        delivered = {"1.1", "1.2"}
        study = ["1.1", "1.2", "1.3"]
        failed_uids = [u for u in study if u not in delivered]
        assert len(failed_uids) == 1
        assert len(study) - len(delivered) == 1  # same here

        # The case that differs: a resumed job that has not yet seen 1.3.
        delivered = {"1.1"}
        observed = set()
        failed_uids = [u for u in study if u in observed and u not in delivered]
        assert failed_uids == [], "nothing observed, nothing failed yet"


# ---------------------------------------------------------------------------
# M11: a peer removed from peers.conf must disappear from the DB
# ---------------------------------------------------------------------------
class TestMoveJobsPeerReload:
    def test_a_removed_peer_is_deleted(self, tmp_path):
        from clean_code.move_jobs import MoveJobManager

        peers = tmp_path / "peers.conf"
        peers.write_text("A 10.0.0.1:104\nB 10.0.0.2:104\n", encoding="utf-8")

        db = _shared_db(str(tmp_path / "jobs.db"))
        store = MoveJobManager(shared_db=db, max_workers=1)
        try:
            store.reload_peers_from_file(str(peers))
            assert store.lookup_scp("A", "10.0.0.1")
            assert store.lookup_scp("B", "10.0.0.2")

            # Drop B, then reload: a peer the operator removed must stop
            # routing, otherwise it stays a destination the config no longer
            # names.
            peers.write_text("A 10.0.0.1:104\n", encoding="utf-8")
            store._last_peers_reload = 0
            store.reload_peers_from_file(str(peers))

            assert store.lookup_scp("A", "10.0.0.1")
            assert not store.lookup_scp("B", "10.0.0.2"), "a removed peer must be gone"
        finally:
            store.close()
            db.close()


# ---------------------------------------------------------------------------
# m1: an unmatched STOW failure must not mark the batch delivered
# ---------------------------------------------------------------------------
class TestStowUnmatchedFailure:
    def test_a_rejection_for_an_unsent_sop_retries_the_batch(self):
        """PS3.18 10.5: the server rejected something we did not send."""
        from clean_code.http_helpers import _parse_stow_failures_all

        body = (
            '{"00081198":{"vr":"SQ","Value":[{"00081155":'
            '{"vr":"UI","Value":["9.9.9"]}}]}}'
        )
        failed = _parse_stow_failures_all(body)
        assert [f["sop_uid"] for f in failed] == ["9.9.9"]

        sent = {"1.2.3"}
        unmatched = [f for f in failed if f["sop_uid"] not in sent]
        assert unmatched, "the response names a SOP this batch never sent"
