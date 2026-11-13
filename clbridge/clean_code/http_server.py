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
import socket
import time
from http.server import BaseHTTPRequestHandler

from prometheus_client import generate_latest
from pydicom import Dataset
from pydicom.datadict import dictionary_VR, keyword_dict

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

# Per-socket-operation deadline for a request. A print job is a JSON body with
# base64 images on a LAN, so 30 s is far beyond any legitimate transfer while
# still bounding a stalled peer: without it one silent body holds the
# connection (and the worker) open indefinitely. A peer that keeps sending a
# byte just inside every window is not a stall, so the TOTAL body read is
# bounded separately by BODY_TOTAL_TIMEOUT_SEC.
HTTP_SOCKET_TIMEOUT_SEC = 30

# Wall-clock budget for reading ONE request body. The largest legitimate body
# is a 100 MB base64 print job (the size ceiling in do_POST); on the LAN and
# even on a 10 Mbit link that is well under a minute, so 120 s leaves room for
# a slow printer driver while still bounding a peer that would otherwise hold
# the worker for as long as it cares to trickle.
BODY_TOTAL_TIMEOUT_SEC = 120

# Bytes per read in the body loop. The deadline bounds the total, so the chunk
# only has to be large enough that a normal body costs a handful of reads.
_BODY_CHUNK = 64 * 1024

# Wall-clock budget for the request line PLUS headers. `self.timeout` bounds
# one socket operation, so a peer sending a header byte every 29 s is never
# a stall and keeps the single-threaded listener busy for as long as it
# likes - while /health, the liveness probe, waits behind it. 30 s is far
# beyond any legitimate request head (nginx's default client_header_timeout
# is 60 s for a full request; a head is a fraction of that).
HEAD_TOTAL_TIMEOUT_SEC = 30


class _BodyDeadlineExceeded(Exception):
    """The peer is too slow: the wall-clock body budget ran out."""


class _BodyTruncated(Exception):
    """The peer sent fewer bytes than its Content-Length announced."""


class _HeadDeadlineExceeded(Exception):
    """The peer took too long over the request line and headers."""


class _HeadBudgetReader:
    """Re-arms the raw socket's timeout to the remaining head budget per BYTE.

    `parse_request` reads the headers through `self.rfile`, and `readline()`
    on a buffered reader loops internally: one call issues as many `recv`s as
    it needs, each of which would get its own fresh socket window. Re-arming
    once per `readline` call therefore still let a peer trickle a byte inside
    every window and hold the head read forever.

    `readline` is implemented HERE as a byte-at-a-time loop over `read(1)`, so
    the deadline is checked between every byte. The head is a few hundred
    bytes, so the extra Python-level looping costs nothing measurable, and the
    boundedness is what the head read exists for.
    """

    def __init__(self, inner, deadline, sock):
        self._inner = inner
        self._deadline = deadline
        self._sock = sock

    def _arm(self):
        remaining = self._deadline - time.monotonic()
        if remaining <= 0:
            raise _HeadDeadlineExceeded()
        if self._sock is not None:
            try:
                self._sock.settimeout(max(0.1, remaining))
            except OSError:
                pass

    def readline(self, limit=-1):
        """One line, assembled byte by byte under the remaining budget."""
        line = bytearray()
        while True:
            self._arm()
            char = self._inner.read(1)
            if not char:
                # EOF: the caller sees a short/empty line and answers
                # accordingly (empty request line = peer closed).
                return bytes(line)
            line += char
            if char == b"\n":
                return bytes(line)
            if 0 < limit <= len(line):
                # Same truncation contract as the buffered reader: a line
                # longer than `limit` is returned without its terminator and
                # the caller reports 414.
                return bytes(line)

    def read(self, n=-1):
        self._arm()
        return self._inner.read(n)

    def __getattr__(self, name):
        return getattr(self._inner, name)


# ---------------------------------------------------------------------------
# Bridge: JSON -> DICOM Dataset converter for C-PRINT
# ---------------------------------------------------------------------------
class _UnknownPrintAttribute(ValueError):
    """A print job names an attribute the DICOM dictionary does not know.

    Raised instead of skipping the key: a typo in a film-session keyword used
    to drop the attribute silently, and the print job then differed from what
    the caller asked for with no error anywhere.
    """


# PS3.5 Table 6.2-1: the VRs whose value is a number on the wire. A JSON
# number for one of these is passed through as a number; a JSON STRING is
# still accepted (a caller may format IS/DS itself) and reaches pydicom as it
# was written.
_NUMERIC_VRS = frozenset({"US", "SS", "UL", "SL", "UV", "SV", "IS", "DS", "FL", "FD"})


def _print_attr_value(vr, value):
    """One attribute value, typed for the VR the dictionary gave the tag.

    JSON can only carry strings, numbers, booleans and nulls, so a caller can
    legitimately send `{"ImageBoxPosition": 1}` or `"1"` for a US attribute.
    `str()`ing every scalar made the VALUE a string regardless - pydicom then
    warns ("a value of type 'str' cannot be assigned to a tag with VR US") and
    a strict SCP reads the element with the wrong value type. Numbers pass
    through untouched and pydicom's own converter applies the VR's rules;
    everything else keeps the historical string form.
    """
    if (
        vr in _NUMERIC_VRS
        and isinstance(value, (int, float))
        and not isinstance(value, bool)
    ):
        return value
    return str(value)


def _json_print_to_dataset(json_dict):
    """Convert JSON dict of DICOM tags to pydicom Dataset.

    Keys can be:
        - DICOM tag hex string (e.g. "00100010" -> PatientName)
        - DICOM keyword (e.g. "PatientName")

    The VR comes from the DICOM data dictionary (PS3.6; pydicom mirrors it in
    `pydicom.datadict.dictionary_VR`), never from the Python type of the JSON
    value. A hardcoded "SH" for every scalar wrote IS/CS/LO/DS/US attributes
    under the wrong VR, and "OB" for a list turned a multi-valued CS (such as
    a Film Box attribute) into a byte string the print SCP cannot interpret.

    An unknown keyword raises `_UnknownPrintAttribute` rather than being
    skipped: the caller answers 400 and the operator sees the typo.
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
                raise _UnknownPrintAttribute(f"unknown print attribute: {key}")
            # keyword_dict values are ints like 0x00100010
            group = tag_int >> 16
            elem = tag_int & 0xFFFF

        tag = (group, elem)
        # PS3.6: the dictionary is the authority for the VR. It RAISES
        # KeyError for a tag it does not know (private or simply absent), so
        # the lookup is guarded rather than post-checked - `dictionary_VR`
        # never returns an empty string on an unknown tag.
        try:
            vr = dictionary_VR(tag)
        except KeyError:
            raise _UnknownPrintAttribute(
                f"unknown print attribute: {key} ({group:04X},{elem:04X})"
            ) from None
        if not vr:
            raise _UnknownPrintAttribute(
                f"unknown print attribute: {key} ({group:04X},{elem:04X})"
            )

        if vr == "SQ":
            # A sequence carries items, not scalars: a dict is one item.
            if isinstance(value, dict):
                items = [_json_print_to_dataset(value)]
            elif isinstance(value, list):
                items = [
                    _json_print_to_dataset(v) if isinstance(v, dict) else v
                    for v in value
                ]
            else:
                raise _UnknownPrintAttribute(
                    f"{key} is SQ but the value is not a sequence"
                )
            ds.add_new(tag, vr, items)
        elif isinstance(value, list):
            # Multi-valued attribute keeps the TAG's VR (PS3.5 6.4 VM>1).
            ds.add_new(tag, vr, [_print_attr_value(vr, v) for v in value])
        else:
            ds.add_new(tag, vr, _print_attr_value(vr, value))
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

    # `socketserver` copies this onto the connection in BaseHTTPRequestHandler
    # .setup(), so it bounds EVERY socket operation on the handler - including
    # the body read below. It stops a peer that STALLS; a peer that trickles
    # keeps every recv inside the window, and that is what the total deadline
    # in `_read_body_with_deadline` bounds.
    timeout = HTTP_SOCKET_TIMEOUT_SEC

    # Dependencies injected via make_bridge_handler() factory  -  NO lazy imports in do_GET!
    driver_registry = {}
    printer_registry = {}
    bridge_ae_title = "CLBRIDGE"  # BR-P31: cached references to avoid per-request imports (P2.10 bottleneck fix)
    _outbox_ref = None  # OutboxQueue  -  reliable persistence queue
    _health_checker_ref = None  # HealthChecker  -  real server health status
    _commit_tracker_ref = None  # CommitTracker  -  Storage Commitment batches
    _move_jobs_ref = None  # MoveJobManager  -  C-MOVE job table

    def _read_body_with_deadline(self, content_length):
        """Read exactly `content_length` bytes, bounded by a wall-clock budget.

        `self.timeout` bounds one socket operation, so a peer that sends a
        byte just inside every timeout window holds this thread for as long as
        it likes while never tripping the socket timeout. The deadline makes
        the TOTAL bounded, which is the property the class comment claims.

        Checking the deadline BETWEEN reads is not enough on its own: a read
        can block for the whole socket timeout (30 s), so a peer trickling one
        byte per 29 s overran a 120 s budget by up to two windows. Each read is
        therefore bounded by what is LEFT of the budget, and a read that
        returns nothing before its own budget means the deadline passed inside
        it - reported as the same `_BodyDeadlineExceeded` a between-read check
        would raise.

        `rfile.read(n)` cannot be used here: it is a BUFFERED reader whose
        internal loop keeps issuing `recv`s until it has `n` bytes, so one call
        can outlive the budget however the socket timeout is set (the timeout
        applies per `recv`, not per call). The bytes are read from the RAW
        socket instead - one `recv` per iteration, each preceded by a
        budget-derived timeout - and any bytes the reader already buffered are
        consumed from it first so the two sources cannot interleave.
        """
        deadline = time.monotonic() + BODY_TOTAL_TIMEOUT_SEC
        sock = getattr(self, "connection", None)
        prev_timeout = None
        if sock is not None:
            try:
                prev_timeout = sock.gettimeout()
            except OSError:
                sock = None
        chunks = []
        remaining = content_length
        try:
            # Anything the buffered reader already pulled off the socket must
            # be consumed first, or the raw reads below would start past it.
            #
            # `peek(0)` on an EMPTY body still enters the buffered reader and
            # can block until the socket timeout (30 s) for a client that sent
            # `Content-Length: 0` and is waiting for the response - the request
            # then times out with nothing to read.
            if sock is not None and remaining > 0:
                try:
                    buffered = self.rfile.peek(0)
                except Exception:
                    buffered = b""
                if buffered:
                    take = min(len(buffered), remaining)
                    chunks.append(self.rfile.read(take))
                    remaining -= take
            while remaining > 0:
                budget = deadline - time.monotonic()
                if budget <= 0:
                    raise _BodyDeadlineExceeded()
                want = min(_BODY_CHUNK, remaining)
                if sock is None:
                    # No raw socket (an in-process caller): fall back to the
                    # reader, still bounded between iterations.
                    chunk = self.rfile.read(want)
                else:
                    # Never wait longer than the remaining budget: the socket
                    # timeout is the only thing that bounds a blocking recv.
                    try:
                        sock.settimeout(min(budget, self.timeout or budget))
                    except OSError:
                        sock = None
                        continue
                    try:
                        chunk = sock.recv(want)
                    except socket.timeout:
                        # Whose timeout fired decides the answer: ours means the
                        # wall-clock budget ran out (408, retryable), the peer's
                        # own stall is equally a 408 for the caller.
                        if time.monotonic() >= deadline:
                            raise _BodyDeadlineExceeded() from None
                        raise
                if not chunk:
                    raise _BodyTruncated()
                chunks.append(chunk)
                remaining -= len(chunk)
            return b"".join(chunks)
        finally:
            if sock is not None and prev_timeout is not None:
                try:
                    sock.settimeout(prev_timeout)
                except OSError:
                    pass

    def _read_head_with_deadline(self):
        """Parse the request line and headers under one wall-clock budget.

        `self.timeout` bounds ONE socket operation, and every received byte
        resets it, so a peer that sends a header line every 29 s is never a
        "stall" and holds this thread for as long as it likes. The listener
        is single-threaded, so that also blocks /health.

        The deadline is read ONCE here and travels with the reader: every
        read of the request line and of each header re-arms the raw socket to
        whatever is LEFT of the total, so a trickling peer exhausts the budget
        even though every individual recv succeeds. The body has its own
        budget (`_read_body_with_deadline`), which this one deliberately does
        not touch.
        """
        deadline = time.monotonic() + HEAD_TOTAL_TIMEOUT_SEC
        sock = getattr(self, "connection", None)
        prev_timeout = None
        if sock is not None:
            try:
                prev_timeout = sock.gettimeout()
            except OSError:
                sock = None
        rfile = self.rfile
        try:
            self.rfile = _HeadBudgetReader(rfile, deadline, sock)
            # Reset the parsed-request fields BEFORE reading: a head timeout
            # escapes before `parse_request` ever runs, and `send_error` logs
            # and quotes `self.requestline` - which `setup()` creates only
            # when the previous request completed. A peer that stalled on the
            # very first byte raised AttributeError out of the 408 path
            # instead of answering it.
            self.command = None
            self.request_version = self.default_request_version
            self.requestline = ""
            self.raw_requestline = self.rfile.readline(65537)
            if len(self.raw_requestline) > 65536:
                self.requestline = ""
                self.request_version = ""
                self.command = ""
                self.send_error(414)  # HTTPStatus.REQUEST_URI_TOO_LONG
                return False
            if not self.raw_requestline:
                # The peer closed before sending anything: not an error.
                self.close_connection = True
                return False
            if not self.parse_request():
                # parse_request already sent the error (400/505) or answered
                # an Expect: 100-continue rejection.
                return False
            return True
        finally:
            self.rfile = rfile
            if sock is not None and prev_timeout is not None:
                try:
                    sock.settimeout(prev_timeout)
                except OSError:
                    pass

    def handle_one_request(self):
        """Read one request under the head budget, then dispatch it.

        Reimplemented only to route the request line and headers through
        `_read_head_with_deadline`: the stdlib loop calls `readline` and
        `parse_request` directly, and each of those resets the per-operation
        socket timeout on every byte received. The body budget is applied
        later, inside the handler, and is unaffected.
        """
        try:
            self.raw_requestline = b""
            self.command = None
            self.request_version = self.default_request_version
            self.requestline = ""
            self.headers = None
            if not self._read_head_with_deadline():
                return
            method = getattr(self, "do_" + str(self.command), None)
            if method is None:
                self.send_error(
                    501, "Unsupported method (%r)" % self.command
                )  # HTTPStatus.NOT_IMPLEMENTED
                return
            method()
            self.wfile.flush()
        except (socket.timeout, TimeoutError, _HeadDeadlineExceeded):
            # The head budget ran out. 408 says what happened; dropping the
            # connection silently leaves the peer with no answer at all.
            self.close_connection = True
            try:
                self.send_error(408, "Request header timed out")
            except OSError:
                pass
        except ConnectionError:
            self.close_connection = True

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
                # A checker that raises must not keep reporting the last good
                # value: a stale 1 tells the dashboards the server is alive
                # while the very check that reports it is broken.
                server_health.set(0)
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

        # Validate Content-Type. Compare the media-type ESSENCE, never a
        # substring: `"application/json" in "text/plain; x=application/json"`
        # is True, and text/plain is CORS-safelisted, so a cross-origin page
        # could post a print job with no preflight at all (PS3.18 aside, this
        # is the request boundary). Parameters (charset) are allowed.
        content_type = self.headers.get("Content-Type", "")
        media_type = content_type.split(";", 1)[0].strip().lower()
        if media_type != "application/json":
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
        # Two failure modes, two answers: too slow is a 408 (the peer can
        # retry with less data), an early EOF is a 400 (the peer lied about
        # the length). Neither may escape do_POST - an uncaught exception here
        # drops the connection with no status at all.
        try:
            body = self._read_body_with_deadline(content_length)
        except _BodyDeadlineExceeded:
            self.send_error(408, "Request body timed out")
            return
        except _BodyTruncated:
            self.send_error(400, "Request body truncated")
            return
        except socket.timeout:
            # The peer STALLED mid-body: the socket read hit the per-connection
            # timeout instead of the wall-clock budget. Same meaning as the
            # deadline case - the request was never answered, so 408, not the
            # dropped connection an uncaught OSError leaves behind.
            self.send_error(408, "Request body read timed out")
            return

        try:
            request_data = json.loads(body)
        except ValueError as e:
            # json.loads(bytes) decodes to str first: non-UTF-8 bytes raise
            # UnicodeDecodeError (a ValueError), not JSONDecodeError. Both are
            # malformed input and must yield a 400, never a dropped connection.
            self._json_error(400, f"Invalid JSON: {e}")
            return
        # A syntactically valid array or scalar is still not a print request,
        # and `request_data.get` would raise AttributeError out of the handler.
        if not isinstance(request_data, dict):
            self._json_error(400, "Expected a JSON object")
            return

        printer_name = request_data.get("printer_name")
        if printer_name is None or printer_name == "":
            self._json_error(400, "Missing printer_name")
            return
        # A list or object here reaches `printer_registry.get(...)` and raises
        # TypeError (unhashable) out of the handler, dropping the connection
        # without the JSON error the caller can act on.
        if not isinstance(printer_name, str):
            self._json_error(400, "printer_name must be a string")
            return
        # `images: null` iterates over None (TypeError), and a string or an
        # object iterates character-by-character / key-by-key: both must be a
        # JSON 400, not a dropped connection or a garbage print job. Checked
        # with the rest of the request shape, before the registry lookup.
        raw_images = request_data.get("images", [])
        if not isinstance(raw_images, list):
            self._json_error(400, "images must be a list")
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

        # Decode images from base64 (validated as a list above).
        images = []
        for img_b64 in raw_images:
            try:
                # validate=True: without it b64decode DISCARDS every character
                # outside the alphabet, so a truncated or corrupted image body
                # decoded to some shorter byte string and failed deep inside
                # the print driver instead of here, where the request shape is
                # still known and a 400 is the honest answer.
                if isinstance(img_b64, str):
                    img_b64 = img_b64.encode("ascii")
                images.append(base64.b64decode(img_b64, validate=True))
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
                # PS3.8 9.3.2.1: the Called AE Title is the PRINT SCP's, not
                # ours. `printer_cfg["aet"]` is what the registry documents as
                # the printer's identity; addressing it with the bridge's own
                # title only works against a peer that accepts anything.
                called_aet=printer_cfg.get("aet") or None,
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
        """Redirect HTTP server logs to Python logging.

        The arguments are passed through, NOT pre-formatted: `format % args`
        raises TypeError as soon as a caller's message does not match the
        format string, and `send_error` passes through text that reached it
        from the request. A logging call must never raise out of the handler.
        """
        logging.debug("[HTTP-BRIDGE] " + format, *args)


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
