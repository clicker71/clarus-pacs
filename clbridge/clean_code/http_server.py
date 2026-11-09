# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge  -  HTTP handler for /print, /metrics, /health, /version.

BridgeHTTPHandler: POST /print (C-PRINT), GET /metrics (Prometheus), GET /health,
GET /version (build identification).
Includes JSON->DICOM Dataset converter and factory for dependency injection (MAJ-003).
"""

import base64
import json
import logging
from http.server import BaseHTTPRequestHandler

from prometheus_client import generate_latest
from pydicom import Dataset
from pydicom.datadict import keyword_dict

from clean_code.config import VERSION
from clean_code.metrics import (
    cmove_jobs_pending,
    commit_batches_pending,
    memory_usage,
    naction_batches_pending,
    outbox_pending,
    server_health,
)
from clean_code.process_rss import get_process_rss_bytes

# Short git revision of the commit the bundle was generated from. bundle.py
# rewrites this line when it assembles clbridge.py; in the source tree the
# value is the placeholder, which is also the honest answer for a bundle cut
# outside a repository.
BUNDLE_STAMP = "unknown"


# ---------------------------------------------------------------------------
# Bridge: JSON -> DICOM Dataset converter for C-PRINT
# ---------------------------------------------------------------------------
def _json_print_to_dataset(json_dict):
    """Convert JSON dict of DICOM tags to pydicom Dataset.

    Keys can be:
        - DICOM tag hex string (e.g. "00100010" -> PatientName)
        - DICOM keyword (e.g. "PatientName")
    """
    ds = Dataset()
    for key, value in json_dict.items():
        # Try hex tag
        if len(key) == 8 and all(c in "0123456789ABCDEFabcdef" for c in key):
            group = int(key[0:4], 16)
            elem = int(key[4:8], 16)
        else:
            # Try keyword lookup
            tag_int = keyword_dict.get(key)
            if tag_int is None:
                logging.debug("[C-PRINT] Unknown DICOM keyword: %s", key)
                continue
            # keyword_dict values are ints like 0x00100010
            group = tag_int >> 16
            elem = tag_int & 0xFFFF

        # Handle nested dicts as sequences
        if isinstance(value, dict):
            item = _json_print_to_dataset(value)
            ds.add_new((group, elem), "SQ", [item])
        elif isinstance(value, list):
            # Could be VR mismatch; set raw
            ds.add_new((group, elem), "OB", value)
        else:
            ds.add_new((group, elem), "SH", str(value))  # default VR; may be overridden
    return ds


# ---------------------------------------------------------------------------
# Bridge: HTTP Bridge Handler  -  /print + /metrics, /health
# ---------------------------------------------------------------------------
class BridgeHTTPHandler(BaseHTTPRequestHandler):
    """HTTP handler for POST /print, GET /metrics, GET /health, GET /version.

    POST /print  -  receives print jobs from ClarityX.
        Request body: JSON with keys:
            printer_name (str): key in printers.yaml
            film_session (dict): DICOM Film Session attributes as JSON
            images (list[str]): base64-encoded DICOM Part 10 files
            calling_aet (str, optional): override AE Title
        Response: JSON PrintResult

    GET /metrics  -  Prometheus text format metrics.
    GET /health  -  liveness probe (200 OK), pure and fast.
    GET /version  -  build identification (version + generated-from revision).
    """

    # Dependencies injected via make_bridge_handler() factory  -  NO lazy imports in do_GET!
    driver_registry = {}
    printer_registry = {}
    bridge_ae_title = "CLARUS_BRIDGE"
    # BR-P31: cached references to avoid per-request imports (P2.10 bottleneck fix)
    _outbox_ref = None  # OutboxQueue  -  reliable persistence queue
    _health_checker_ref = None  # HealthChecker  -  real server health status
    _commit_tracker_ref = None  # CommitTracker  -  Storage Commitment batches
    _move_jobs_ref = None  # MoveJobManager  -  C-MOVE job table

    def do_GET(self):
        """Handle GET /metrics, GET /health and GET /version (P22d, P2.10).

        P2.10: /health is a PURE liveness probe  -  no imports, no queue checks,
        no health checker calls. Those are enforced at C-STORE level (handle_c_store).
        /health MUST return in <1ms to avoid serialization bottleneck in
        the single-threaded HTTP server (or thread pool starvation in
        ThreadingHTTPServer).
        """
        if self.path == "/metrics":
            # RSS comes straight from the OS and involves no handle to hold
            # open, so there is nothing to cache between scrapes. A failure
            # leaves the gauge at 0 rather than breaking the scrape.
            memory_usage.set(get_process_rss_bytes())

            # update Outbox queue metrics  -  use cached refs, NO imports
            try:
                _outbox = type(self)._outbox_ref
                if _outbox is not None:
                    outbox_pending.set(_outbox.pending_count())
            except Exception:
                pass

            # update queue-depth gauges  -  cached refs, no imports.
            # Every access is guarded: a broken tracker must not break a scrape.
            try:
                _ct = type(self)._commit_tracker_ref
                if _ct is not None:
                    _n = _ct.pending_count()
                    commit_batches_pending.set(_n)
                    naction_batches_pending.set(_n)
            except Exception:
                pass

            try:
                _mj = type(self)._move_jobs_ref
                if _mj is not None:
                    cmove_jobs_pending.set(_mj.count_pending())
            except Exception:
                pass

            # server_health from real HealthChecker, NOT outbox pending
            try:
                _hc = type(self)._health_checker_ref
                if _hc is not None:
                    server_health.set(1 if _hc.is_healthy() else 0)
            except Exception as e:
                logging.error("[METRICS] server_health update failed: %s", e)

            metrics_data = generate_latest()  # pyright: ignore[reportOptionalCall]
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; version=0.0.3; charset=utf-8")
            self.send_header("Content-Length", str(len(metrics_data)))
            self.end_headers()
            self.wfile.write(metrics_data)
        elif self.path == "/health":
            # P2.10: /health is a PURE liveness probe. No imports, no queue checks,
            # no health checker  -  just "is the HTTP server alive?".
            # Backpressure (queue%, server health) is enforced at C-STORE level.
            # This MUST be the fastest handler in the system.
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(b"OK")
        elif self.path == "/version":
            # Which bundle is running? Same weight as /health: a constant join,
            # no imports beyond the module constant.
            body = f"clbridge {VERSION} ({BUNDLE_STAMP})".encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            self.send_response(404)
            self.end_headers()

    def do_POST(self):
        if self.path != "/print":
            self.send_error(404, "Not Found")
            return

        # Validate Content-Type
        content_type = self.headers.get("Content-Type", "")
        if "application/json" not in content_type:
            self.send_error(400, "Expected application/json")
            return

        # Read body. A non-numeric or negative length is a malformed request,
        # not a reason to drop the connection without an answer: `int()` would
        # raise out of the handler, and a negative length passes the size
        # ceiling below while `read(-1)` blocks the worker on a body that never
        # ends.
        try:
            content_length = int(self.headers.get("Content-Length", 0))
        except (TypeError, ValueError):
            self._json_error(400, "Invalid Content-Length")
            return
        if content_length < 0:
            self._json_error(400, "Invalid Content-Length")
            return
        if content_length > 100 * 1024 * 1024:  # 100 MB limit
            self.send_error(413, "Payload too large")
            return
        body = self.rfile.read(content_length)

        try:
            request_data = json.loads(body)
        except json.JSONDecodeError as e:
            self._json_error(400, f"Invalid JSON: {e}")
            return
        # A syntactically valid array or scalar is still not a print request,
        # and `request_data.get` would raise AttributeError out of the handler.
        if not isinstance(request_data, dict):
            self._json_error(400, "Expected a JSON object")
            return

        printer_name = request_data.get("printer_name")
        if not printer_name:
            self._json_error(400, "Missing printer_name")
            return

        printer_cfg = self.printer_registry.get(printer_name)
        if not printer_cfg:
            self._json_error(404, f"Printer '{printer_name}' not found in registry")
            return

        # Decode film_session from JSON -> pydicom Dataset
        try:
            film_session = _json_print_to_dataset(request_data.get("film_session", {}))
        except Exception as e:
            self._json_error(400, f"Invalid film_session: {e}")
            return

        # Decode images from base64
        images = []
        for img_b64 in request_data.get("images", []):
            try:
                images.append(base64.b64decode(img_b64))
            except Exception as e:
                self._json_error(400, f"Invalid image base64: {e}")
                return

        # Get calling AE title
        calling_aet = request_data.get("calling_aet", self.bridge_ae_title)

        # Select driver
        driver_name = printer_cfg.get("driver", "generic_dicom")
        driver = self.driver_registry.get(driver_name)
        if not driver:
            self._json_error(500, f"Driver '{driver_name}' not registered")
            return

        # Execute print
        try:
            result = driver.print(
                film_session=film_session,
                images=images,
                printer_host=printer_cfg["host"],
                printer_port=printer_cfg["port"],
                calling_aet=calling_aet,
            )
            self._json_response(200 if result.is_success else 502, result)
        except Exception as e:
            logging.error("[C-PRINT] Print job failed: %s", e)
            logging.debug("[C-PRINT] Full traceback follows:", exc_info=True)
            self._json_error(500, f"Print failed: {e}")

    def _json_response(self, status_code, result):
        """Send JSON PrintResult."""
        body = json.dumps(
            {
                "job_id": result.job_id,
                "status": f"0x{result.status:04X}",
                "film_boxes_printed": result.film_boxes_printed,
                "failed_images": result.failed_images,
                "printer_message": result.printer_message,
            }
        ).encode("utf-8")
        self.send_response(status_code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _json_error(self, status_code, message):
        """Send JSON error."""
        body = json.dumps({"error": message}).encode("utf-8")
        self.send_response(status_code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        """Redirect HTTP server logs to Python logging."""
        logging.debug("[HTTP-BRIDGE] " + format % args)


# ---------------------------------------------------------------------------
# Bridge: factory for BridgeHTTPHandler with injected dependencies
# ---------------------------------------------------------------------------
def make_bridge_handler(
    driver_registry,
    printer_registry,
    bridge_ae_title,
    health_checker=None,
    outbox=None,
    commit_tracker=None,
    move_jobs=None,
):
    """Create BridgeHTTPHandler subclass with injected dependencies.

    P2.10: Injects bridge runtime dependencies (outbox, etc.)
    as class-level attributes so do_GET() never does lazy imports. This eliminates
    the per-request import lock contention that was serializing /health behind
    /metrics in the single-threaded HTTPServer.

    PH-007: health_checker is stored for real server_health gauge.
    """

    class _Handler(BridgeHTTPHandler):
        pass

    _Handler.driver_registry = driver_registry
    _Handler.printer_registry = printer_registry
    _Handler.bridge_ae_title = bridge_ae_title
    _Handler._outbox_ref = outbox
    _Handler._health_checker_ref = health_checker
    _Handler._commit_tracker_ref = commit_tracker
    _Handler._move_jobs_ref = move_jobs
    return _Handler
