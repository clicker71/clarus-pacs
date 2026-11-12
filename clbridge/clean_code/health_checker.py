# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge - DICOMweb Server Health Checker.

Background poller + pre-flight gate. Polls GET /health every N seconds
and updates a threading.Event. C-STORE handler reads the Event -
zero HTTP overhead on the hot path, proactive degradation.

Modes (from clbridge.conf [dicomweb] health_check_mode):
  "probe"  - background poll. On HTTP 404: server has no /health endpoint
             - disable checks until bridge restart (max compatibility).
  "always" - background poll every `interval` seconds. If server unhealthy,
             C-STORE immediately returns 0xA700 - no buffering, no waste.
             For Clarus: always use this mode.
  "never"  - skip health check entirely (max DICOMweb compatibility).

/health is NOT in PS3.18 - it's a Kubernetes liveness/readiness de-facto
standard. Non-K8s DICOMweb servers (Orthanc, dcm4che) don't have it.
"""

import logging
import threading
import time

import requests

from clean_code.http_helpers import dicomweb_origin


class HealthChecker:
    """Background health-check of DICOMweb server + pre-flight gate.

    Background thread polls GET /health every `interval` seconds.
    Updates `_server_healthy` threading.Event proactively.
    C-STORE handler reads the Event via check_before_stow() -
    no HTTP on the hot path, bridge degrades before buffering.

    Thread-safe: check_before_stow() is lock-free (reads Event).
    Only the background poller writes state.
    """

    def __init__(
        self,
        clarus_url: str,
        mode: str = "probe",
        interval: float = 5.0,  # background poll interval (probe + always)
        http_timeout: float = 5.0,
        tls=None,
    ):
        if mode not in ("probe", "always", "never"):
            raise ValueError(
                f"health_check_mode must be 'probe'|'always'|'never', got '{mode}'"
            )
        self._clarus_url = clarus_url
        # /health lives at the server ROOT, not under /dicomweb (2908).
        self._health_url = f"{dicomweb_origin(clarus_url)}/health"
        self._tls = tls
        self._mode = mode
        self._interval = interval
        self._http_timeout = http_timeout

        # Proactive state (updated by background poller)
        self._server_healthy = threading.Event()
        self._server_healthy.set()  # Start optimistic - assume alive
        self._last_ok_time = time.monotonic()
        self._consecutive_failures = 0
        self._disabled_until_restart = False  # set on 404 in "probe" mode
        self._last_reason = ""
        # R11: set when /health answers 4xx. That is the server being reachable
        # but refusing the probe (bad auth, wrong verb, throttling), which says
        # nothing about whether it can store. The gate is restored while this
        # is set, so a transient probe rejection cannot pin every C-STORE to
        # 0xA700 for the life of the process.
        self._probe_rejected = False

        # Background thread
        self._thread = None

    # --- Public API ---

    def is_healthy(self) -> bool:
        """Return True if server is currently considered healthy.

        Non-blocking snapshot - reads Event, no HTTP.
        """
        if self._mode == "never":
            return True
        if self._disabled_until_restart:
            return True  # Server has no /health - assume OK
        return self._server_healthy.is_set()

    def check_before_stow(self) -> tuple:
        """Pre-flight health gate. Call before each STOW-RS.

        Both "probe" and "always" use the background poller's Event.
        Lock-free: zero HTTP overhead on the C-STORE hot path.

        "never" mode: always OK.
        "probe" mode:  if 404 was detected, checks disabled (always OK).
        "always" mode: Event reflects server state; 0xA700 if unhealthy.

        Returns:
            (True, 0x0000, "")         - server healthy, proceed
            (False, 0xA700, reason)    - server unhealthy, reject with 0xA700
            (True, 0x0000, "disabled") - health check disabled (probe got 404)
        """
        if self._mode == "never":
            return (True, 0x0000, "")

        if self._disabled_until_restart:
            return (True, 0x0000, "disabled")

        # Lock-free Event read - updated by background poller.
        # Zero HTTP on the hot path, proactive degradation.
        if self._server_healthy.is_set():
            return (True, 0x0000, "")

        return (False, 0xA700, self._last_reason or "server unhealthy")

    def dead_seconds(self) -> float:
        """Return seconds since last successful health check (0 if healthy)."""
        if self._server_healthy.is_set():
            return 0.0
        return time.monotonic() - self._last_ok_time

    # --- Lifecycle ---

    def start(self):
        """Start background health-check thread.

        "always" mode: poller runs forever, never disables.
        "probe" mode:  poller runs until first 404, then disables.
        "never" mode:  no poller, no checks.
        """
        if self._mode == "never":
            logging.info("[HEALTH] Health check disabled (mode=never).")
            return

        # Both "probe" and "always" use background polling.
        # Probe auto-disables on 404 (non-Clarus servers without /health).
        # Always mode: stays enabled permanently (Clarus, Kubernetes -
        # /health endpoint is guaranteed to exist).
        self._thread = threading.Thread(
            target=self._run,
            name="health-checker",
            daemon=True,
        )
        self._thread.start()
        logging.info(
            "[HEALTH] Background poller started: GET %s "
            "every %.0fs, timeout=%.0fs, mode=%s",
            self._health_url,
            self._interval,
            self._http_timeout,
            self._mode,
        )

    def stop(self):
        """Stop background thread (no-op - daemon thread exits with process)."""
        # Daemon thread, no explicit stop needed

    # - Background Poller -

    def _run(self):
        """Background loop: poll /health, update Event."""
        from clean_code.utils import _shutdown_requested

        was_dead = False
        while not _shutdown_requested.is_set():
            was_dead = self._poll_once(was_dead)
            if self._disabled_until_restart:
                return  # Probe mode found no /health - nothing to poll
            time.sleep(self._interval)

    def _poll_once(self, was_dead):
        """One /health fetch; update the healthy Event. Returns the new
        `was_dead` flag.

        Split out of the loop so each verdict is reachable without a thread:
        the 4xx/5xx distinction is the whole point of the branch and could not
        be exercised while it lived inside an infinite loop.
        """
        url = self._health_url
        try:
            resp = requests.get(url, timeout=self._http_timeout, cert=self._tls)
            if resp.status_code == 200:
                self._on_healthy()
                return False
            if resp.status_code == 404 and self._mode == "probe":
                # Server has no /health - disable checks permanently
                self._disabled_until_restart = True
                logging.warning(
                    "[HEALTH] Server returned 404 on /health - "
                    "health check disabled until bridge restart "
                    "(mode=probe). Server: %s",
                    self._clarus_url,
                )
                return was_dead
            if 400 <= resp.status_code < 500:
                # A 4xx is the SERVER ANSWERING: it is alive and refusing this
                # particular request. 401/403 mean the probe is unauthenticated
                # or the config is wrong, 405 that the verb is wrong, 429 that
                # the probe is being throttled. Reporting "down" for them
                # cleared the health Event and made every C-STORE answer
                # 0xA700 - a bridge that stores nothing because its own probe
                # is unauthorised is worse than one that stores and lets the
                # real request fail on its own merits.
                #
                # The probe's own "not found" is the 404 case above. Any other
                # 4xx therefore means the endpoint exists but this request
                # cannot use it: keep the last verdict, and say so once so the
                # operator can fix the probe.
                reason = f"probe rejected HTTP {resp.status_code}"
                if not self._probe_rejected:
                    logging.warning(
                        "[HEALTH] /health answered HTTP %d - server is "
                        "reachable but the probe is not accepted; keeping "
                        "C-STORE enabled (check auth/config)",
                        resp.status_code,
                    )
                self._probe_rejected = True
                self._last_reason = reason
                # The server ANSWERED, so it is up for our purposes: restore
                # the gate. Leaving it cleared because the probe is unauthorised
                # made the bridge refuse every instance after a single 401.
                self._server_healthy.set()
                self._consecutive_failures = 0
                self._last_ok_time = time.monotonic()
                return was_dead
            self._on_unhealthy(f"Server returned HTTP {resp.status_code}", was_dead)
            return True
        except requests.exceptions.Timeout:
            self._on_unhealthy(f"/health timeout after {self._http_timeout}s", was_dead)
            return True
        except requests.exceptions.ConnectionError as e:
            self._on_unhealthy(f"Connection refused: {e}", was_dead)
            return True
        except Exception as e:
            self._on_unhealthy(f"/health error: {e}", was_dead)
            return True

    def _on_healthy(self):
        """Called when /health returns 200."""
        if not self._server_healthy.is_set():
            downtime = time.monotonic() - self._last_ok_time
            logging.warning(
                "[HEALTH] Server recovered after %.0fs downtime",
                downtime,
            )
        self._server_healthy.set()
        self._consecutive_failures = 0
        self._last_ok_time = time.monotonic()
        self._last_reason = ""
        self._probe_rejected = False

    def _on_unhealthy(self, reason: str, was_dead: bool):
        """Called when /health fails."""
        self._server_healthy.clear()
        self._consecutive_failures += 1
        self._last_reason = reason
        if not was_dead:
            logging.warning(
                "[HEALTH] Check #%d failed: %s - bridge degraded, "
                "C-STORE will return 0xA700",
                self._consecutive_failures,
                reason,
            )
        elif self._consecutive_failures % 12 == 0:
            # Periodic reminder every ~60s (12 * 5s interval)
            dead_for = time.monotonic() - self._last_ok_time
            logging.error(
                "[HEALTH] Server still dead after %.0fs " "(check #%d): %s",
                dead_for,
                self._consecutive_failures,
                reason,
            )
