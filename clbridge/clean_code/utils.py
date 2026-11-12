# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge  -  core utilities: shutdown signal, logging, dataset measurement.

Contains graceful shutdown event, signal handler, logging setup with rotation,
and C-STORE dataset size measurement.
"""

import hashlib
import hmac
import io
import ipaddress
import logging
import os
import signal
import sys
import tempfile
import threading
import time
from logging.handlers import RotatingFileHandler

from pydicom import dcmwrite
from pydicom.uid import ImplicitVRLittleEndian

# ---------------------------------------------------------------------------
# Bridge: graceful shutdown event  -  checked by all handlers
# ---------------------------------------------------------------------------
_shutdown_requested = threading.Event()

# : PID file path for duplicate instance detection cleanup
_pid_path = None  # Set by main.py after successful startup checks


def set_pid_path(path: str) -> None:
    """Set PID file path (called by main.py after startup checks)."""
    global _pid_path
    _pid_path = path


def _signal_handler(signum, frame):
    """Handle SIGTERM/SIGINT/SIGBREAK  -  initiate graceful shutdown (BR-007).

    nginx-way: signal handler does ONE thing  -  sets the shutdown flag.
    The ShutdownMonitor thread (started in main.py) bridges this flag
    to pynetdicom's event loop by calling ae.shutdown() from a regular
    thread context, which is safe (not async-signal-restricted).
    """
    sig_name = signal.Signals(signum).name
    logging.info("Received %s, initiating graceful shutdown...", sig_name)
    _shutdown_requested.set()
    # That's it. Nothing else. No timers, no os._exit, no file I/O.
    # ShutdownMonitor thread handles the rest.


# ---------------------------------------------------------------------------
# Bridge: configure logging with optional rotation
# ---------------------------------------------------------------------------
def _setup_logging(log_level="INFO", log_file=None):
    """Configure root logger with console handler and optional file handler.

    Log rotation: 10 MB x 3 backups (always on when log_file is set).
    """
    level = getattr(logging, log_level.upper(), logging.INFO)
    fmt = logging.Formatter(
        "%(asctime)s [%(levelname)s] %(message)s",
        datefmt="%Y-%m-%dT%H:%M:%S",
    )

    root_logger = logging.getLogger()
    root_logger.setLevel(level)
    root_logger.handlers.clear()

    # -- Suppress library noise  -  : libraries keep quiet --
    # urllib3/requests log every HTTP request at INFO (432 lines of "201 Created"
    # for a 432-slice upload). pynetdicom/pydicom dump tracebacks.
    # Below WARNING, library output is noise  -  admin doesn't need it.
    for lib in ("urllib3", "requests", "httpx", "httpcore", "pydicom", "pynetdicom"):
        logging.getLogger(lib).setLevel(logging.WARNING)

    # -- Console: UTF-8 with error replacement  -  never crash on non-cp1251 chars --
    # Chinese patient names, Norwegian AE titles, DICOM UIDs with special chars  -
    # Windows console may be cp1251, but log messages come in UTF-8.
    # reconfigure() makes stderr replace unencodable chars with '?' instead of crashing.
    console_stream = sys.stderr
    try:
        console_stream.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass  # Python < 3.7 or non-TTY  -  fall through to raw stderr
    console = logging.StreamHandler(console_stream)
    console.setFormatter(fmt)
    root_logger.addHandler(console)

    if log_file:
        # Auto-create the parent directory (e.g. logs/) so a fresh host does not
        # fail on a missing path.
        log_dir = os.path.dirname(os.path.abspath(log_file))
        os.makedirs(log_dir, exist_ok=True)
        max_bytes = 10 * 1024 * 1024  # 10 MB
        file_handler = RotatingFileHandler(
            log_file,
            maxBytes=max_bytes,
            backupCount=3,
            encoding="utf-8",
        )
        file_handler.setFormatter(fmt)
        root_logger.addHandler(file_handler)
        logging.info("Logging to file: %s (10 MB x 3 rotations)", log_file)


# ---------------------------------------------------------------------------
# Bridge: C-STORE size limit measurement
# ---------------------------------------------------------------------------
def _measure_dataset_bytes(dataset, event=None):
    """Return dataset size in bytes.

    Prefers pynetdicom's raw DataSet if available (no extra serialization).
    Falls back to dcmwrite() into BytesIO buffer.

    pynetdicom hands large payloads over as a stream (BytesIO) rather than
    bytes, and a stream has no len(); the size of such a payload is its
    remaining length, so seek to the end and restore the position. Without
    this the size of every large C-STORE would be reported as unmeasurable.
    """
    # Bridge: try raw request data first (fast path, BR-001)
    if event is not None:
        raw = getattr(event.request, "DataSet", None)
        if raw is not None:
            try:
                return len(raw)
            except TypeError:
                # A stream: measure by seeking, then restore the read
                # position so the caller can still consume the payload.
                pos = raw.tell()
                size = raw.seek(0, io.SEEK_END)
                raw.seek(pos)
                return size
    # Bridge: fallback  -  serialize to measure (slow path, BR-001)
    buf = io.BytesIO()
    # Ensure bare Datasets (e.g. from tests) have encoding context
    if not hasattr(dataset, "is_implicit_VR") or dataset.is_implicit_VR is None:
        dataset.is_implicit_VR = True
        dataset.is_little_endian = True
    # Populate minimal file_meta to satisfy pydicom validator
    if not hasattr(dataset, "file_meta") or not dataset.file_meta:
        from pydicom import Dataset as DcmDs

        dataset.file_meta = DcmDs()
    fm = dataset.file_meta
    if not getattr(fm, "MediaStorageSOPClassUID", None):
        fm.MediaStorageSOPClassUID = (
            getattr(dataset, "SOPClassUID", "1.2.840.10008.5.1.4.1.1.7")
            or "1.2.840.10008.5.1.4.1.1.7"
        )
    if not getattr(fm, "MediaStorageSOPInstanceUID", None):
        fm.MediaStorageSOPInstanceUID = (
            getattr(dataset, "SOPInstanceUID", "1.2.826.0.1.3680043.8.498.1")
            or "1.2.826.0.1.3680043.8.498.1"
        )
    if not getattr(fm, "TransferSyntaxUID", None):
        fm.TransferSyntaxUID = ImplicitVRLittleEndian
    dcmwrite(buf, dataset, enforce_file_format=True)
    return len(buf.getvalue())


# ---------------------------------------------------------------------------
# Bridge: PatientName masking - no PHI in logs.
# ---------------------------------------------------------------------------
def replace_with_retry(src, dst, attempts=50):
    """Publish `src` over `dst` atomically, retrying a transient refusal.

    `os.replace` is atomic but NOT free of contention on Windows: it is built
    on ReplaceFile, which fails with a sharing violation (WinError 5/32) when
    the destination is momentarily held open by another handle - a concurrent
    writer, a reader mid-parse, or the OS itself. Measured on this code base:
    8 threads publishing the same destination lost ~10% of the renames, and a
    single thread in isolation never does.

    Retrying is the whole fix: the window is a single syscall wide, so a short
    escalating sleep converges almost immediately (measured: 265 failures
    without a retry, zero with one, ~0.9 extra attempts per publish). The
    attempt budget bounds the cost when the destination is genuinely locked
    forever, and the last error is re-raised unchanged so callers keep their
    existing error handling.
    """
    for attempt in range(attempts):
        try:
            os.replace(src, dst)
            return
        except PermissionError:
            if attempt == attempts - 1:
                raise
            time.sleep(0.001 * (attempt + 1))


# Fixed-size lock striping. A dict keyed by SOP Instance UID (or by index-file
# path) grows one entry per distinct key and nothing ever removes them, so a
# 24/7 service leaks a lock object per instance received - monotonic RSS growth
# with no upper bound. Stripes bound the registry at construction time.
#
# 256 stripes: two writers for the SAME key must still serialise, and they do
# because equal keys hash to the same stripe. Distinct keys sharing a stripe
# merely wait for each other, which costs concurrency but never correctness,
# and 256 is far more than the handful of concurrent publishers this bridge
# runs - the collision rate only matters if that count approaches the stripe
# count.
_STRIPE_COUNT = 256


class LockStripes:
    """A bounded replacement for a per-key dict of threading.Lock."""

    __slots__ = ("_stripes",)

    def __init__(self, count=_STRIPE_COUNT):
        self._stripes = [threading.Lock() for _ in range(count)]

    def for_key(self, key):
        """Return the lock guarding `key`.

        `hash()` is used rather than a digest because a single CPython process
        keeps string hashes stable for its lifetime, and these keys never leave
        the process. `zlib.crc32(key.encode())` would be the portable choice if
        this ever had to agree across interpreters; it does not.
        """
        stripes = self._stripes
        return stripes[hash(key) % len(stripes)]


# ---------------------------------------------------------------------------
# Peer address normalisation
# ---------------------------------------------------------------------------
def normalize_host(host):
    """Canonical form of a peer address for EQUALITY comparison.

    A `peers.conf` host may be `localhost`, an upper-case alias, a bracketed
    IPv6 literal or this machine's FQDN, while a request's calling address is
    the raw A-ASSOCIATE socket address (`127.0.0.1`, `::1`). String inequality
    is not address inequality, so both sides go through this once before being
    compared.

    What it actually does, in order:
      1. strips brackets and whitespace and lower-cases (a NAME is returned
         lower-cased and otherwise untouched - no DNS lookup, no expansion);
      2. the wildcard binds (`0.0.0.0`, `::`) and the literal `localhost`
         become `127.0.0.1`;
      3. anything the IPv4/IPv6 parser accepts is canonicalised by
         `str(ip_address(...))`, and every address in the loopback BLOCK
         (`127.0.0.0/8`, `::1`, and IPv4-mapped forms such as
         `::ffff:127.0.0.1`) is collapsed to `127.0.0.1`.

    It does NOT resolve names, does NOT expand a non-loopback alias to the
    address the kernel would pick, and does NOT treat two different
    non-loopback addresses as equal.
    """
    h = str(host or "").strip().lower().strip("[]")
    if not h:
        return ""
    # Zero-compression-vs-explicit and IPv4-mapped forms are the same endpoint.
    if h in ("0.0.0.0", "::"):
        return "127.0.0.1"
    if h == "localhost":
        return "127.0.0.1"
    try:
        addr = ipaddress.ip_address(h)
    except ValueError:
        return h
    # An IPv4-mapped IPv6 address IS the IPv4 address: a dual-stack listener
    # reports an IPv4 peer as `::ffff:10.0.0.5` while peers.conf says
    # `10.0.0.5`, and comparing the two as written never matches. Unwrap
    # first so the rest of the canonicalisation sees one address family.
    mapped = getattr(addr, "ipv4_mapped", None)
    if mapped is not None:
        addr = mapped
    # The whole loopback block is one machine: `::1`, `[::1]` and `127.0.0.1`
    # must compare equal or a peers.conf entry saying `localhost` disagrees
    # with an A-ASSOCIATE from `::1`.
    if addr.is_loopback:
        return "127.0.0.1"
    return str(addr)


def mask_patient_name(name: str) -> str:
    """Mask PatientName for 152-FZ: Ivanov -> I....v, Smith -> S...h."""
    if not name:
        return "***"
    n = len(name)
    if n <= 1:
        return "***"
    if n == 2:
        return name[0] + "."
    return name[0] + "." * min(n - 2, 4) + name[-1]


def mask_patient_id(patient_id: str) -> str:
    """Mask PatientID for the logs with a keyed HMAC token (152-FZ).

    A national/MRN identifier has no safe first/last mask (its prefix is
    site-coded), so the log carries a stable 12-hex token instead of the raw
    value. The study UID logged next to it remains the correlation key.

    The token is HMAC-SHA256 under a per-install secret, not a bare digest.
    Patient IDs are low entropy by design - a few digits, often with a
    site prefix - so an unkeyed SHA-256 of one is reversible by enumeration:
    anyone holding a log line hashes candidates until one matches, in
    seconds, and the log stops being anonymised. The secret is what makes
    that search impossible without the key file.

    Correlating two lines needs the same secret, so the key persists next to
    the outbox and is created once on first use. Deleting it yields a NEW
    key, and every token written afterwards differs from the ones already in
    the log: old lines keep their own tokens and no longer join to new ones.
    That affects log correlation ONLY - forwarding is unaffected, because the
    outbox carries the raw PatientID, not this token.
    """
    if not patient_id:
        return "***"
    key = _mask_key()
    return hmac.new(key, patient_id.encode("utf-8"), hashlib.sha256).hexdigest()[:12]


#: Per-install masking key, read once. `None` = not yet loaded.
_mask_key_cache = None


def _outbox_root() -> str:
    """The configured outbox directory, resolved the same way main.py resolves
    it (relative paths are relative to the config file). Falls back to the
    default so a unit-test import does not depend on startup order."""
    configured = os.environ.get("CLBRIDGE_OUTBOX_DIR")
    if configured:
        return configured
    return os.path.join(os.getcwd(), "clbridge", "outbox")


def set_mask_key_dir(path: str) -> None:
    """Point the masking key at `path` (called by main.py once the outbox
    directory is resolved). Clears the cache so the next call reloads."""
    global _mask_key_cache, _mask_key_dir_override
    _mask_key_dir_override = path
    _mask_key_cache = None


#: Explicit override set by main.py; empty means "derive from the outbox".
_mask_key_dir_override = ""

#: Serialises the load-or-create sequence between THREADS of this process.
#: The `O_EXCL` create already arbitrates between processes; two threads in
#: one process would otherwise both miss the cache and both run the repair
#: branch over the same file. `RLock` because the recursion is not expected:
#: a plain `Lock` would be enough today, and a nested call in the future must
#: not deadlock the whole process over a log line.
_mask_key_lock = threading.RLock()

#: A published key is 32 random bytes (256 bits); anything shorter is a
#: truncated file or a half-written one, never a key. The length is the
#: contract the winner writes and the loser accepts, so both sides agree.
_MASK_KEY_BYTES = 32

#: How many times a loser re-reads a file it found too short before giving
#: up. The winner's window is one `os.replace` - microseconds - so three
#: attempts with a 10 ms backoff cover an arbitrarily slow scheduler without
#: turning a broken file into an infinite wait.
_MASK_KEY_READ_ATTEMPTS = 3
_MASK_KEY_READ_PAUSE_S = 0.01


def _publish_key(path: str, key: bytes) -> None:
    """Write `key` to `path` so no reader can ever observe a partial file.

    A create-then-write publishes the NAME before the BYTES: a concurrent
    reader sees an empty file, and an empty HMAC key is as good as no key at
    all - the token becomes an unkeyed digest of a low-entropy patient id,
    reversible by enumeration. Writing to a temporary sibling and `os.replace`
    onto the final name makes the publication atomic on POSIX and on Windows,
    so a reader opens either the old file or the complete new one.
    """
    fd, tmp_path = tempfile.mkstemp(
        prefix=".patient_id_mask.", dir=os.path.dirname(path)
    )
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(key)
        os.replace(tmp_path, path)
    except OSError:
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise


def _read_key(path: str) -> bytes:
    """Read a published key, retrying while it is still too short."""
    for attempt in range(_MASK_KEY_READ_ATTEMPTS):
        try:
            with open(path, "rb") as handle:
                key = handle.read()
        except FileNotFoundError:
            key = b""
        if len(key) >= _MASK_KEY_BYTES:
            return key
        if attempt + 1 < _MASK_KEY_READ_ATTEMPTS:
            time.sleep(_MASK_KEY_READ_PAUSE_S)
    return key


def _mask_key() -> bytes:
    """Load the per-install masking key, creating or repairing it as needed.

    Three cases, and they are genuinely different:

    * ABSENT - create it exclusively (`O_EXCL`). Two processes starting at the
      same moment must not both write a key with one silently winning: the
      loser re-reads what the winner wrote, so both end up with the same key.
    * PRESENT and long enough - use it. This is the normal path.
    * PRESENT but too short - the file is truncated or was never a key. It has
      to be REPLACED, which the exclusive-create path cannot do: `O_EXCL`
      fails on an existing file, and re-reading it would hand back the same
      unusable bytes while the log claimed otherwise. The replacement is
      therefore an in-place truncating write.

    A missing key is created rather than treated as an error: the first log
    line of a fresh install must already be masked, and refusing to log it
    would trade a privacy leak for an outage. An UNREADABLE key (permissions,
    a directory in its place) is logged and not fatal - tokens then stop being
    stable across restarts, which is visible and recoverable, whereas a crash
    at startup is not.
    """
    global _mask_key_cache
    if _mask_key_cache is not None:
        return _mask_key_cache
    with _mask_key_lock:
        if _mask_key_cache is not None:
            return _mask_key_cache
        key = _load_or_create_key()
        _mask_key_cache = key
        return key


def _load_or_create_key() -> bytes:
    """The load-or-repair sequence behind `_mask_key`, run under the lock."""
    root = _mask_key_dir_override or _outbox_root()
    path = os.path.join(root, ".patient_id_mask.key")

    try:
        with open(path, "rb") as handle:
            existing = handle.read()
    except FileNotFoundError:
        existing = None
    except OSError as exc:
        logging.error(
            "[MASK] cannot read the patient-id key at %s (%s); "
            "log tokens are not stable across restarts",
            path,
            exc,
        )
        return os.urandom(_MASK_KEY_BYTES)

    if existing is not None and len(existing) >= _MASK_KEY_BYTES:
        return existing

    key = os.urandom(_MASK_KEY_BYTES)
    try:
        os.makedirs(root, exist_ok=True)
        if existing is None:
            # Absent. The exclusive create decides a race between processes,
            # and its only role is to name one winner: the bytes are then
            # published by rename, so no reader ever sees the empty file the
            # create leaves behind.
            try:
                fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            except FileExistsError:
                # Another process won. Its file is authoritative; it is read
                # back with a length check, because the winner's rename makes a
                # short read impossible and a half-written file must never be
                # adopted as a key.
                adopted = _read_key(path)
                if len(adopted) >= _MASK_KEY_BYTES:
                    return adopted
                # The winner is mid-publication or gone; publishing ours is
                # still the best repair, and it is atomic for every reader.
                _publish_key(path, key)
                return key
            os.close(fd)
        else:
            # Present but unusable: it cannot be trusted and `O_EXCL` would
            # refuse to fix it, so it is replaced.
            #
            # R11: re-read first. A SHORT file is also what a process sees
            # while ANOTHER process is mid-publish, and replacing it there
            # made two live processes hold DIFFERENT keys - the same PatientID
            # then masked to different tokens, which breaks log correlation.
            # `_read_key` retries with its own bounded pause, so a publication
            # in flight gets to finish; ours is published only if the file is
            # still unusable afterwards.
            adopted = _read_key(path)
            if len(adopted) >= _MASK_KEY_BYTES:
                return adopted
            # The length is reported because a short key is a symptom worth
            # seeing; after the retries it really is truncated, not in flight.
            logging.warning(
                "[MASK] key file %s is short (%d bytes); replacing it",
                path,
                len(adopted),
            )
        _publish_key(path, key)
    except OSError as exc:
        logging.error(
            "[MASK] cannot persist the patient-id key at %s (%s); "
            "log tokens are not stable across restarts",
            path,
            exc,
        )
    return key
