# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Tests for _http_retry and stow_single_instance — HTTP retry behavior.

P31: Tests retry on 5xx, no retry on 4xx, retry on timeout,
retry on connection error, and success without retry.
"""

from unittest.mock import MagicMock, patch

import httpx
import pytest
from clean_code.http_helpers import (
    _http_retry,
    dicomweb_base,
    retry_after_seconds,
    stow_single_instance,
)

URL = "http://127.0.0.1:8024/dicomweb"


# ---------------------------------------------------------------------------
# _http_retry tests
# ---------------------------------------------------------------------------


class TestHttpRetry:

    def test_success_returns_immediately(self):
        """2xx response → return immediately, no retry."""
        mock_resp = MagicMock()
        mock_resp.status_code = 200

        def request_func():
            return mock_resp

        with patch("time.sleep") as mock_sleep:
            result = _http_retry(request_func, max_retries=3)
        assert result is mock_resp
        mock_sleep.assert_not_called()

    def test_4xx_returns_immediately_no_retry(self):
        """4xx client error → return immediately, no retry (permanent error)."""
        mock_resp = MagicMock()
        mock_resp.status_code = 404

        call_count = [0]

        def request_func():
            call_count[0] += 1
            return mock_resp

        with patch("time.sleep") as mock_sleep:
            result = _http_retry(request_func, max_retries=3)
        assert result is mock_resp
        assert call_count[0] == 1  # Only called once
        mock_sleep.assert_not_called()

    def test_5xx_retries_then_returns_response(self):
        """5xx server error -> retry max_retries times, then return the final
        response so the caller maps it (5xx -> 0xA700 transient)."""
        mock_resp = MagicMock()
        mock_resp.status_code = 500
        call_count = [0]

        def request_func():
            call_count[0] += 1
            return mock_resp

        with patch("time.sleep") as mock_sleep:
            result = _http_retry(request_func, max_retries=2, delay=0.01)
        assert result is mock_resp
        assert result.status_code == 500
        assert call_count[0] == 3  # initial + 2 retries
        assert mock_sleep.call_count == 2

    def test_timeout_retries_then_raises(self):
        """Timeout → retry, then raise last exception."""
        call_count = [0]

        def request_func():
            call_count[0] += 1
            raise httpx.TimeoutException("timed out")

        with patch("time.sleep") as mock_sleep:
            with pytest.raises(httpx.TimeoutException):
                _http_retry(request_func, max_retries=2, delay=0.01)
        assert call_count[0] == 3  # initial + 2 retries
        assert mock_sleep.call_count == 2

    def test_connection_error_retries_then_raises(self):
        """ConnectionError → retry, then raise last exception."""
        call_count = [0]

        def request_func():
            call_count[0] += 1
            raise httpx.ConnectError("refused")

        with patch("time.sleep") as mock_sleep:
            with pytest.raises(httpx.ConnectError):
                _http_retry(request_func, max_retries=2, delay=0.01)
        assert call_count[0] == 3
        assert mock_sleep.call_count == 2

    def test_5xx_then_success_on_retry(self):
        """5xx on first attempt, 2xx on retry → returns success."""
        mock_500 = MagicMock()
        mock_500.status_code = 503
        mock_200 = MagicMock()
        mock_200.status_code = 200

        call_count = [0]

        def request_func():
            call_count[0] += 1
            if call_count[0] == 1:
                return mock_500
            return mock_200

        with patch("time.sleep") as mock_sleep:
            result = _http_retry(request_func, max_retries=3, delay=0.01)
        assert result is mock_200
        assert call_count[0] == 2  # Called twice (initial + 1 retry)
        assert mock_sleep.call_count == 1

    def test_max_retries_zero_means_one_attempt(self):
        """max_retries=0 -> exactly 1 attempt, no sleep, response returned."""
        mock_resp = MagicMock()
        mock_resp.status_code = 500

        def request_func():
            return mock_resp

        with patch("time.sleep") as mock_sleep:
            result = _http_retry(request_func, max_retries=0, delay=0.01)
        assert result is mock_resp
        mock_sleep.assert_not_called()

    def test_5xx_does_not_mask_transport_exception(self):
        """5xx on the final attempt must not swallow a transport exception
        class: a timeout still propagates (callers catch it explicitly)."""

        def request_func():
            raise httpx.TimeoutException("timed out")

        with patch("time.sleep"):
            with pytest.raises(httpx.TimeoutException):
                _http_retry(request_func, max_retries=1, delay=0.01)


# ---------------------------------------------------------------------------
# stow_single_instance tests (status code mapping)
# ---------------------------------------------------------------------------


class TestStowStatusCodeMapping:
    """Test STOW-RS HTTP status → DIMSE status mapping.

    These tests validate the mapping logic without actual HTTP calls.
    """

    @staticmethod
    def _make_stow_dataset():
        """Create a minimal Dataset with file_meta for dcmwrite."""
        from pydicom import Dataset
        from pydicom.uid import ImplicitVRLittleEndian

        ds = Dataset()
        ds.PatientName = "Test"
        ds.SOPClassUID = "1.2.840.10008.5.1.4.1.1.2"
        ds.SOPInstanceUID = "1.2.3.4.5.6"
        ds.file_meta = Dataset()
        ds.file_meta.MediaStorageSOPClassUID = "1.2.840.10008.5.1.4.1.1.2"
        ds.file_meta.MediaStorageSOPInstanceUID = "1.2.3.4.5.6"
        ds.file_meta.TransferSyntaxUID = ImplicitVRLittleEndian
        return ds

    def test_stow_200_returns_success(self):
        """HTTP 200 → DIMSE 0x0000."""
        ds = self._make_stow_dataset()
        mock_resp = MagicMock()
        mock_resp.status_code = 200
        mock_resp.ok = True

        with patch("clean_code.http_helpers._http_retry", return_value=mock_resp):
            status, http_status, reason = stow_single_instance(
                ds,
                URL,
                False,
            )
        assert status == 0x0000
        assert http_status == 200

    def test_stow_507_insufficient_storage(self):
        """HTTP 507 → DIMSE 0xA700 (Out of Resources)."""
        ds = self._make_stow_dataset()
        mock_resp = MagicMock()
        mock_resp.status_code = 507
        mock_resp.ok = False

        with patch("clean_code.http_helpers._http_retry", return_value=mock_resp):
            status, http_status, reason = stow_single_instance(
                ds,
                URL,
                False,
            )
        assert status == 0xA700
        assert http_status == 507

    def test_stow_401_returns_c000(self):
        """HTTP 401 → DIMSE 0xC000 (auth error)."""
        ds = self._make_stow_dataset()
        mock_resp = MagicMock()
        mock_resp.status_code = 401

        with patch("clean_code.http_helpers._http_retry", return_value=mock_resp):
            status, http_status, reason = stow_single_instance(
                ds,
                URL,
                False,
            )
        assert status == 0xC000
        assert http_status == 401


# ---------------------------------------------------------------------------
# stow_batch_instances tests — one multipart POST per study-batch
# ---------------------------------------------------------------------------


def _make_payload(uid, raw_bytes):
    """Build a (sop_uid, ds, raw_bytes) batch payload tuple."""
    ds = TestStowStatusCodeMapping._make_stow_dataset()
    ds.SOPInstanceUID = uid
    return (uid, ds, raw_bytes)


class TestStowBatchInstances:
    """STOW-RS N-part multipart batching (fieldtest: 1063 -> ~20 POSTs)."""

    @staticmethod
    def _capture_body(uid_list):
        """Return (mock_client, results) with _http_retry driving a mock client."""
        from clean_code.http_helpers import stow_batch_instances

        payloads = [_make_payload(uid, uid.encode()) for uid in uid_list]
        client = MagicMock()
        resp = MagicMock()
        resp.status_code = 200
        resp.text = "{}"
        client.post.return_value = resp

        with patch(
            "clean_code.http_helpers._http_retry", side_effect=lambda req: req()
        ):
            results, _ = stow_batch_instances(
                payloads,
                URL,
                http_timeout=30,
                client=client,
            )
        return client, results

    def test_builds_multipart_with_n_parts(self):
        """Body has one part per SOP: boundary + Content-Type + raw bytes each."""
        uids = ["1.1.1.1", "1.1.1.2", "1.1.1.3"]
        client_mock, results = self._capture_body(uids)

        body = client_mock.post.call_args.kwargs["content"]
        headers = client_mock.post.call_args.kwargs["headers"]
        ct = headers["Content-Type"]

        assert ct.startswith('multipart/related; type="application/dicom"; boundary=')
        boundary = ct.split("boundary=")[1]

        # Exactly 3 part-bodies + 1 closing boundary.
        assert body.count(f"--{boundary}\r\n".encode()) == len(uids)
        assert body.count(b"Content-Type: application/dicom") == len(uids)
        # Each raw part is present byte-identical (P8 pass-through).
        for uid in uids:
            assert uid.encode() in body, f"raw bytes for {uid} missing from body"
        # Multipart closes with a final boundary.
        assert body.endswith(f"--{boundary}--\r\n".encode())

        # Per-SOP success outcomes in payload order.
        assert results == [
            (uids[0], 0x0000, 200, "OK"),
            (uids[1], 0x0000, 200, "OK"),
            (uids[2], 0x0000, 200, "OK"),
        ]

    def test_partial_failure_from_failed_sop_sequence(self):
        """FailedSOPSequence (0008,1198): only the failing SOP is marked 0xA700."""
        uid_ok = "1.1.1.9"
        uid_bad = "1.1.1.8"
        payloads = [
            _make_payload(uid_ok, uid_ok.encode()),
            _make_payload(uid_bad, uid_bad.encode()),
        ]

        resp = MagicMock()
        resp.status_code = 202
        resp.text = (
            '{"00081198":{"Value":[{"00081197":{"Value":[272]}'
            ',"00081155":{"Value":["' + uid_bad + '"]}}]}}'
        )

        with patch("clean_code.http_helpers._http_retry", return_value=resp):
            from clean_code.http_helpers import stow_batch_instances

            results, _ = stow_batch_instances(payloads, URL)

        by_uid = {uid: (st, hs, msg) for uid, st, hs, msg in results}
        assert by_uid[uid_ok][0] == 0x0000  # accepted SOP stays OK
        assert by_uid[uid_bad][0] == 0xA700  # rejected SOP retries
        assert "code=272" in by_uid[uid_bad][2]

    def test_total_5xx_marks_all_retryable(self):
        """HTTP 500 on the whole batch -> every SOP is 0xA700 (retryable)."""
        uids = ["1.1.1.5", "1.1.1.6"]
        payloads = [_make_payload(uid, uid.encode()) for uid in uids]
        resp = MagicMock()
        resp.status_code = 500

        with patch("clean_code.http_helpers._http_retry", return_value=resp):
            from clean_code.http_helpers import stow_batch_instances

            results, _ = stow_batch_instances(payloads, URL)
        assert all(st == 0xA700 for _, st, _, _ in results)

    def test_4xx_marks_all_fatal(self):
        """HTTP 400 on the whole batch -> every SOP is 0xC000 (fatal)."""
        uids = ["1.1.1.7"]
        payloads = [_make_payload(uid, uid.encode()) for uid in uids]
        resp = MagicMock()
        resp.status_code = 400

        with patch("clean_code.http_helpers._http_retry", return_value=resp):
            from clean_code.http_helpers import stow_batch_instances

            results, _ = stow_batch_instances(payloads, URL)
        assert results[0][1] == 0xC000

    def test_empty_payloads_returns_empty(self):
        """Empty batch -> no HTTP call, empty result list."""
        from clean_code.http_helpers import stow_batch_instances

        with patch("clean_code.http_helpers._http_retry") as mock_r:
            assert stow_batch_instances([], URL) == ([], None)
        mock_r.assert_not_called()

    def test_dcmwrite_used_when_raw_is_none(self):
        """raw_bytes=None -> dataset re-serialized (modified instance)."""
        from clean_code.http_helpers import stow_batch_instances

        ds = TestStowStatusCodeMapping._make_stow_dataset()
        ds.SOPInstanceUID = "1.1.1.m"
        payloads = [("1.1.1.m", ds, None)]

        client = MagicMock()
        resp = MagicMock()
        resp.status_code = 200
        resp.text = "{}"
        client.post.return_value = resp

        with patch(
            "clean_code.http_helpers._http_retry", side_effect=lambda req: req()
        ):
            stow_batch_instances(payloads, URL, client=client)

        body = client.post.call_args.kwargs["content"]
        # Re-serialized DICOM has our File Meta (not the 3-byte raw stub).
        assert b"DICM" in body


# ---------------------------------------------------------------------------
# 2908: clarus_url refactor - 507 no-retry, Retry-After, dicomweb_base
# ---------------------------------------------------------------------------


class TestHttpRetry507:
    """507 Insufficient Storage is returned immediately, never retried."""

    def test_507_returns_immediately_no_retry(self):
        """507 -> one attempt, response returned (backpressure handled upstream)."""
        mock_resp = MagicMock()
        mock_resp.status_code = 507

        call_count = [0]

        def request_func():
            call_count[0] += 1
            return mock_resp

        with patch("time.sleep") as mock_sleep:
            result = _http_retry(request_func, max_retries=3, delay=0.01)
        assert result is mock_resp
        assert call_count[0] == 1
        mock_sleep.assert_not_called()


class TestRetryAfterSeconds:
    """Retry-After parsing: clamp 5..300, default 30 on garbage/absence."""

    def test_numeric_seconds(self):
        assert retry_after_seconds({"Retry-After": "60"}) == 60

    def test_clamped_low(self):
        assert retry_after_seconds({"Retry-After": "1"}) == 5

    def test_clamped_high(self):
        assert retry_after_seconds({"Retry-After": "9000"}) == 300

    def test_missing_header_defaults_30(self):
        assert retry_after_seconds({}) == 30

    def test_garbage_defaults_30(self):
        assert retry_after_seconds({"Retry-After": "soon"}) == 30

    def test_http_date_defaults_30(self):
        assert (
            retry_after_seconds({"Retry-After": "Wed, 21 Oct 2026 07:28:00 GMT"}) == 30
        )


class TestDicomwebBase:
    """dicomweb_base is the single source of the base URL (no trailing slash)."""

    def test_strips_trailing_slash(self):
        assert (
            dicomweb_base("http://127.0.0.1:8024/dicomweb/")
            == "http://127.0.0.1:8024/dicomweb"
        )

    def test_https_passthrough(self):
        assert (
            dicomweb_base("https://tsami.example.com/dicomweb")
            == "https://tsami.example.com/dicomweb"
        )

    def test_root_url_strips_all_slashes(self):
        assert (
            dicomweb_base("https://tsami.example.com/") == "https://tsami.example.com"
        )


class TestStowBatch507:
    """507 on the whole batch -> per-SOP 0xA700 + retry_after seconds."""

    def test_507_returns_backoff_from_header(self):
        uids = ["1.1.1.7"]
        payloads = [_make_payload(uid, uid.encode()) for uid in uids]
        resp = MagicMock()
        resp.status_code = 507
        resp.headers = {"Retry-After": "120"}

        with patch("clean_code.http_helpers._http_retry", return_value=resp):
            from clean_code.http_helpers import stow_batch_instances

            results, backoff = stow_batch_instances(payloads, URL)
        assert backoff == 120
        assert all(st == 0xA700 and hs == 507 for _, st, hs, _ in results)

    def test_507_without_header_defaults_backoff_30(self):
        uids = ["1.1.1.7"]
        payloads = [_make_payload(uid, uid.encode()) for uid in uids]
        resp = MagicMock()
        resp.status_code = 507
        resp.headers = {}

        with patch("clean_code.http_helpers._http_retry", return_value=resp):
            from clean_code.http_helpers import stow_batch_instances

            results, backoff = stow_batch_instances(payloads, URL)
        assert backoff == 30
        assert all(st == 0xA700 for _, st, _, _ in results)


# ---------------------------------------------------------------------------
# QIDO-RS: an empty or unparseable answer is "no matches", not an exception
# ---------------------------------------------------------------------------
class TestQidoEmptyAnswers:
    """PS3.18 10.6: a search with no matches may answer 204 with no body.

    The C-FIND handlers only catch httpx errors, so a body that is absent or
    not JSON must come back as an empty list - otherwise a no-match search
    surfaces as a tooling failure.
    """

    def _response(self, status, content):
        resp = MagicMock()
        resp.status_code = status
        resp.content = content
        resp.raise_for_status = MagicMock()
        if content:
            import json

            resp.json = MagicMock(side_effect=lambda: json.loads(content))
        else:
            resp.json = MagicMock(side_effect=ValueError("no body"))
        return resp

    def test_204_no_content_is_an_empty_result(self):
        from clean_code.http_helpers import _qido_request

        resp = self._response(204, b"")
        resp.json = MagicMock(side_effect=ValueError("204 has no body"))
        with patch("clean_code.http_helpers._http_retry", return_value=resp):
            assert _qido_request(URL, {}) == []
        resp.json.assert_not_called()

    def test_a_200_with_an_empty_body_is_an_empty_result(self):
        from clean_code.http_helpers import _qido_request

        resp = self._response(200, b"")
        resp.json = MagicMock(side_effect=ValueError("200 with no body"))
        with patch("clean_code.http_helpers._http_retry", return_value=resp):
            assert _qido_request(URL, {}) == []
        resp.json.assert_not_called()

    def test_a_non_json_body_warns_and_returns_empty(self, caplog):
        from clean_code.http_helpers import _qido_request

        resp = MagicMock()
        resp.status_code = 200
        resp.content = b"<html>proxy error</html>"
        resp.raise_for_status = MagicMock()
        resp.json = MagicMock(side_effect=ValueError("not json"))
        with patch("clean_code.http_helpers._http_retry", return_value=resp):
            with caplog.at_level("WARNING"):
                assert _qido_request(URL, {}) == []
        assert any("not JSON" in r.message for r in caplog.records)

    def test_the_body_is_parsed_once(self):
        """Two calls to json() would read the body twice and, on a stream,
        parse the second time from an exhausted buffer."""
        from clean_code.http_helpers import _qido_request

        resp = MagicMock()
        resp.status_code = 200
        resp.content = b'[{"00080018": {"Value": ["1.2.3"]}}]'
        resp.raise_for_status = MagicMock()
        resp.json = MagicMock(return_value=[{"00080018": {"Value": ["1.2.3"]}}])
        with patch("clean_code.http_helpers._http_retry", return_value=resp):
            rows = _qido_request(URL, {})
        assert rows
        assert resp.json.call_count == 1
