# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""0109 wire test: main-association retry against a MUTE SCP.

A raw TCP listener accepts the connection but sends NOTHING (no A-ASSOCIATE
response) - the exact field shape of the MicroDicom wedge (pynetdicom logs
"ACSE timeout reached", associate() returns is_established=False). The
bridge must make one retry and then fail with "Cannot associate".
The retries are IMMEDIATE (no pacing delay; the 0709
`outbound_assoc_retry_delay` knob is removed) - `time.sleep` is stubbed so
the test never waits on real ACSE timeouts.
"""

import socket
import threading
import time

import pytest
from clean_code.handlers.cmove import _associate_for_move


def test_mute_scp_gets_one_retry_then_cannot_associate(monkeypatch):
    accepted = []
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", 0))
    port = srv.getsockname()[1]
    srv.listen(5)
    stop = threading.Event()

    def mute_loop():
        srv.settimeout(0.2)
        while not stop.is_set():
            try:
                conn, _ = srv.accept()
                accepted.append(1)
                # Keep the socket open and silent: no A-ASSOCIATE response.
            except TimeoutError:
                continue
        srv.close()

    t = threading.Thread(target=mute_loop, daemon=True)
    t.start()

    # A pacing delay would make the test slow; retries are immediate (no
    # pacing delay after the 0709 knob removal), and `time.sleep` is stubbed
    # so real ACSE timeouts are never waited on.
    monkeypatch.setattr("time.sleep", lambda _s: None)

    with pytest.raises(RuntimeError, match="Cannot associate"):
        _associate_for_move(
            "MUTE_SCP",
            "BRIDGE_AE",
            None,
            "127.0.0.1",
            port,
            acse_timeout=1,
        )

    time.sleep(0.5)
    stop.set()
    t.join(timeout=2)
    assert (
        len(accepted) >= 2
    ), f"mute SCP must see 1 attempt + 1 retry connection, got {len(accepted)}"
