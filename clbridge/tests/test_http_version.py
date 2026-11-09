# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Bridge HTTP surface: GET /version, GET /health, GET /metrics.

The three routes share one `do_GET`, so the tests drive all three against a
real HTTP server on an ephemeral port - the same way an operator's curl does.
The point of `/version` is that two field bundles can be told apart, so the
tests pin the shape AND that the stamp is never empty.
"""

import http.client
import threading
from http.server import HTTPServer

import pytest
from clean_code.config import VERSION
from clean_code.http_server import BUNDLE_STAMP, BridgeHTTPHandler


@pytest.fixture()
def bridge_http():
    """A real HTTP server with the bridge handler on an ephemeral port."""
    server = HTTPServer(("127.0.0.1", 0), BridgeHTTPHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server.server_address[1]
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def _get(port, path):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        conn.request("GET", path)
        resp = conn.getresponse()
        return resp.status, resp.getheader("Content-Type"), resp.read()
    finally:
        conn.close()


def test_version_reports_the_bundle_stamp(bridge_http):
    """`GET /version` names the bundle and the revision it was cut from."""
    status, content_type, body = _get(bridge_http, "/version")

    assert status == 200
    assert content_type == "text/plain; charset=utf-8"

    text = body.decode("utf-8")
    assert text.startswith("clbridge "), text
    assert VERSION in text, text
    # The stamp is the whole point: an empty one cannot distinguish builds.
    assert BUNDLE_STAMP, "BUNDLE_STAMP must never be an empty string"
    assert text == f"clbridge {VERSION} ({BUNDLE_STAMP})", text


def test_version_body_is_exactly_one_line(bridge_http):
    """A version banner is one line - a trailing newline is a wire change."""
    _, _, body = _get(bridge_http, "/version")
    assert b"\n" not in body, body


def test_health_is_still_a_pure_liveness_probe(bridge_http):
    """`/health` is untouched: 200, text/plain, exactly `OK`."""
    status, content_type, body = _get(bridge_http, "/health")

    assert status == 200
    assert content_type == "text/plain"
    assert body == b"OK"


def test_metrics_route_is_untouched(bridge_http):
    """`/metrics` keeps its Prometheus content type and body."""
    status, content_type, body = _get(bridge_http, "/metrics")

    assert status == 200
    assert content_type == "text/plain; version=0.0.3; charset=utf-8"
    assert b"clarus_bridge_" in body, body[:200]


def test_unknown_get_path_is_404(bridge_http):
    """Only the documented routes answer; everything else is a 404."""
    status, _, _ = _get(bridge_http, "/nope")
    assert status == 404
