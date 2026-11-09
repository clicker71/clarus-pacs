# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false, reportArgumentType=false, reportOptionalCall=false

"""Clarus Bridge - DIMSE to DICOMweb adapter.

Configuration: clbridge.conf (nginx-style)
Sections: [server], [dicomweb], [dimse], [logging], [metrics], [advanced]

CLI:
  --config <path>   Path to config file (default: ./clbridge.conf)
  --version          Print version and exit
  --help             Print this help
"""

import ctypes
import functools
import logging
import os
import signal
import sys
import threading
import time

try:
    import yaml
except ImportError:
    yaml = None  # type: ignore

from http.server import ThreadingHTTPServer

from pydicom.uid import ImplicitVRLittleEndian
from pynetdicom import AE, _config, evt
from pynetdicom.sop_class import (
    ModalityWorklistInformationFind,
    PatientRootQueryRetrieveInformationModelFind,
    PatientRootQueryRetrieveInformationModelMove,
    StorageCommitmentPushModel,
    StudyRootQueryRetrieveInformationModelFind,
    StudyRootQueryRetrieveInformationModelMove,
)

# pass-through C-STORE - raw bytes from temp file.
# pynetdicom reads file_meta only, sends dataset bytes directly from disk.
# No dcmread(), no encode(), no re-serialization. One line.
_config.STORE_SEND_CHUNKED_DATASET = True

# TS-order constants (IMAGE_TS EVRLE-first etc.) live in clean_code.protocol
# (single definition - the bundle warns on duplicate module-level names).

from clean_code.assoc_tracker import (
    register_assoc_accepted,
    register_assoc_released,
)
from clean_code.commit_tracker import CommitTracker
from clean_code.config import (
    VERSION,
    ConfParser,
    validate_clarus_url,
)
from clean_code.handlers.cfind import handle_c_find
from clean_code.handlers.cmove import (
    _cmove_cleanup_worker,
    _cmove_worker,
    _extract_dataset_ui,
    _extract_meta_ui,
    handle_c_move,
)
from clean_code.handlers.cstore import (
    handle_c_echo,
    handle_c_store,
    set_max_concurrent_cstore,
)
from clean_code.handlers.naction import _sop_confirm, handle_n_action
from clean_code.health_checker import HealthChecker
from clean_code.http_helpers import STC_NO_RESOURCE, _stc_commit_request
from clean_code.http_server import make_bridge_handler
from clean_code.metrics import (
    ae_label,
    associations_total,
    bridge_up,
    bytes_total,
    cstore_outbound_duration,
    forward_delivered_total,
    forward_failed_total,
    forward_last_success_timestamp,
    forward_pending,
    forward_pending_files,
    forward_undelivered_total,
    outbox_disk_bytes,
    outbox_last_success_timestamp,
    outbox_pending,
    outbox_pending_files,
    outbox_recovery_total,
)
from clean_code.move_jobs import MoveJobManager
from clean_code.nevent import send_n_event_report
from clean_code.outbox import OutboxQueue
from clean_code.protocol import (
    IMAGE_TS,
    IMAGE_TS_INCOMING,
    JPEGLS_LOSSLESS_TS,  # noqa: F401  # owner decision: main exposes it (test_bridge_ts_order)
    NON_IMAGE_TS,
)  # 31.08 TS order
from clean_code.registry import (
    _get_sop_registry_uids,
    _load_printer_registry,
    _load_sop_registry,
)
from clean_code.sop_index import SopIndexManager
from clean_code.utils import (
    _setup_logging,
    _shutdown_requested,
    _signal_handler,
    mask_patient_id,
    set_mask_key_dir,
    set_pid_path,
)

# Bridge: C-PRINT driver framework - optional, graceful fallback
try:
    from drivers.base import AbstractPrintDriver, PrintResult
    from drivers.generic_dicom import GenericDicomPrintDriver

    _CPRINT_AVAILABLE = True
except ImportError:
    _CPRINT_AVAILABLE = False
    AbstractPrintDriver = None  # type: ignore
    PrintResult = None  # type: ignore
    GenericDicomPrintDriver = None  # type: ignore


import argparse

# SCP peer auto-remove threshold: an auto-registered move destination is
# dropped from the peer list after this many CONSECUTIVE association
# failures.
_SCP_FAILURES_BEFORE_REMOVE = 3


# ---------------------------------------------------------------------------
# ---------------------------------------------------------------------------
# _parse_args - CLI: only --version and --config
# ---------------------------------------------------------------------------
def _parse_args():
    """Parse CLI arguments. Returns argparse.Namespace with --config path only."""
    parser = argparse.ArgumentParser(
        description="Clarus Bridge - DICOM Bridge (C-STORE/C-FIND/C-MOVE/N-ACTION/C-PRINT)"
    )
    parser.add_argument("--version", action="version", version=f"clbridge v{VERSION}")
    # Default config path: next to the .exe (PyInstaller) or .py script.
    # PyInstaller one-file: sys.executable - actual .exe location.
    # Regular Python: __file__ - script location.
    if getattr(sys, "frozen", False):
        _script_dir = os.path.dirname(os.path.abspath(sys.executable))
    else:
        _script_dir = os.path.dirname(os.path.abspath(__file__))
    _default_config = os.path.join(_script_dir, "clbridge.conf")
    parser.add_argument(
        "--config",
        default=_default_config,
        help="Path to configuration file (default: clbridge.conf "
        "in script directory)",
    )
    # --regenerate-uids for tests/anonymization (NOT in config - too dangerous for prod)
    parser.add_argument(
        "--regenerate-uids",
        action="store_true",
        default=False,
        help="Regenerate Study/Series/SOP UIDs (tests/anonymization only)",
    )
    return parser.parse_args()


# ---------------------------------------------------------------------------
# CONFIG-DISCIPLINE: _populate_args - read ALL settings from clbridge.conf
# ---------------------------------------------------------------------------
def _populate_args(cfg):
    """Read all configuration values into a namespace. No CLI defaults - config is authoritative.

    Returns argparse.Namespace with all settings populated.
    """
    args = argparse.Namespace()

    # [server]
    args.bridge_host = cfg.get("server", "host", default="127.0.0.1")
    args.bridge_port = cfg.get_int("server", "port", default=8104)
    args.ae_title = cfg.get("server", "ae_title", default="CLARUS_BRIDGE")
    # QA / Expose Rejected Instances AE (AIRA 57.2.1). A DIMSE association has
    # no per-query "show rejected" flag, so the standard mechanism is a second
    # AE Title whose queries go to a separately configured QA webservice
    # endpoint. Empty port = off, which leaves the bridge with exactly one AE.
    args.expose_port = cfg.get_int("server", "expose_port", default=None)
    args.expose_ae_title = cfg.get("server", "expose_ae_title", default="CLBRIDGE_QA")

    # [dicomweb]
    # 2908: single clarus_url knob (replaces host/clarus_port, NO fallback).
    clarus_url, tls_cert, tls_key = validate_clarus_url(cfg)
    args.clarus_url = clarus_url
    # The QA webservice endpoint AIRA 57.2.1 calls "specifically configured to
    # provide access to hidden instances". It is a SEPARATE BASE URL, not a
    # query parameter: the bridge must work against any DICOMweb server, and a
    # Clarus-only parameter would make it useless elsewhere.
    args.qa_url = cfg.get("dicomweb", "qa_url", default="").strip().rstrip("/")
    args.tls_client_cert = tls_cert
    args.tls_client_key = tls_key
    args.tls_pair = (tls_cert, tls_key) if tls_cert and tls_key else None
    args.health_check_mode = cfg.get(
        "dicomweb", "health_check_mode", default="probe"
    ).lower()

    # [dicomweb] storage_commitment_presence: does the ORIGIN have a
    # Storage Commitment resource (PS3.18 13.4/13.5)? true => commitment
    # verdicts come from the origin's own honest check; false (default) =>
    # today's local heuristic. Unrelated to [dimse] no_storage_commit.
    args.storage_commitment_presence = cfg.get_bool(
        "dicomweb", "storage_commitment_presence", default=False
    )

    # [dimse]
    args.c_find_max_responses = cfg.get_int(
        "dimse", "c_find_max_responses", default=200
    )
    # c_move_workers: auto-scale - I/O-bound workers (TCP to viewer + HTTP from Clarus).
    # max(2, cpu_count()//2). 2-2, 4-2, 8-4, 16-8. Admin can override in config.
    _default_cmove = max(2, (os.cpu_count() or 2) // 2)
    args.c_move_workers = cfg.get_int("dimse", "c_move_workers", default=_default_cmove)
    args.no_storage_commit = cfg.get_bool("dimse", "no_storage_commit", default=False)
    # peers_file: path to peers.conf (chmod 644, public peer list)
    args.peers_file = cfg.get("peers", "peers_file", default="./peers.conf")
    # [peers] - peer auto-registration
    args.lazyload = cfg.get_bool("peers", "lazyload", default=True)
    args.lazyload_interval = cfg.get_int("peers", "lazyload_interval", default=30)
    # - DIMSE timeouts (inbound - outbound) -
    # Inbound: bridge as SCP (C-STORE from modality, C-FIND from viewer)
    args.inbound_dimse_timeout = cfg.get_int(
        "dimse", "inbound_dimse_timeout", default=60
    )
    # inbound_network_timeout: 0 = disabled (SCP waits for client - ).
    # Viewers keep associations open during C-MOVE (PS3.4 -C.4.2.2).
    # Non-zero: kill idle associations after N seconds.
    args.inbound_network_timeout = cfg.get_int(
        "dimse", "inbound_network_timeout", default=0
    )
    args.inbound_acse_timeout = cfg.get_int("dimse", "inbound_acse_timeout", default=15)
    # Outbound: bridge as SCU (C-MOVE - C-STORE to viewer)
    args.outbound_dimse_timeout = cfg.get_int(
        "dimse", "outbound_dimse_timeout", default=60
    )
    args.outbound_network_timeout = cfg.get_int(
        "dimse", "outbound_network_timeout", default=60
    )
    args.outbound_acse_timeout = cfg.get_int(
        "dimse", "outbound_acse_timeout", default=15
    )
    # Total outbound A-ASSOCIATE attempts (main AND fallback) with
    # immediate retries, default 3, range 1..5 (spec sec.12.2 single knob).
    args.outbound_assoc_attempts = cfg.get_int(
        "dimse", "outbound_assoc_attempts", default=3
    )
    # C-MOVE progress poll timeout (2608): the bridge holds the viewer's
    # association and polls job progress in the DB. If a worker dies mid-job
    # the poll must not run forever - on expiry unfinished jobs are failed
    # and 0xB000 is returned. Range: 30..3600.
    args.move_poll_timeout = cfg.get_int("dimse", "move_poll_timeout", default=600)
    # [dimse] inbound association cap (0509 capacity): pynetdicom default
    # is 8 simultaneous inbound associations per AE. One C-MOVE holds its
    # association for the whole study transfer, and TS-fallback opens a
    # SECOND association, so 8 caps concurrent C-MOVE viewers at ~5
    # (measured 05.09: "Local limit exceeded" at step 6). Each held
    # association costs one pynetdicom thread, idle while the study
    # transfers. Default is DELIBERATELY 8 (pynetdicom parity): on weak
    # boxes the bridge then fails SOFT at the cap instead of going deep
    # into memory pressure (05.09 zram RCA: small hosts die of memory,
    # not of thread count). The bench measured 6-7 viewers clean at 32
    # with c_move_workers=2 - raise deliberately. Admin rule:
    # max_associations >= 2 x concurrent C-MOVE viewers + 4 headroom.
    # Range: 4..256.
    args.max_associations = cfg.get_int("dimse", "max_associations", default=16)

    # [bridge] - DICOM field coercion
    args.fix_empty_patient_id = cfg.get_bool(
        "bridge", "fix_empty_patient_id", default=True
    )
    args.fix_empty_patient_name = cfg.get_bool(
        "bridge", "fix_empty_patient_name", default=True
    )
    args.bridge_id_prefix = cfg.get("bridge", "id_prefix", default="AUTOPAT_")
    args.bridge_name_prefix = cfg.get("bridge", "name_prefix", default="Anonymous^")

    # [logging]
    args.log_level = cfg.get("logging", "log_level", default="INFO").upper()
    args.log_file = cfg.get("logging", "log_file", default=None)

    # [metrics]
    args.http_port = cfg.get_int("metrics", "health_port", default=8105)
    args.http_print_bind = cfg.get("metrics", "http_bind", default="127.0.0.1")

    # [advanced]
    args.sop_registry = cfg.get(
        "advanced", "sop_registry", default="clbridge/sop_classes.yaml"
    )
    args.printers_yaml = cfg.get(
        "advanced", "printers_yaml", default="clbridge/printers.yaml"
    )
    args.sop_index_dir = cfg.get(
        "advanced", "sop_index_dir", default="./clbridge/sop_index"
    )
    args.max_store_mb = cfg.get_int("advanced", "max_store_mb", default=0)
    args.http_timeout = cfg.get_int("advanced", "http_timeout", default=30)
    args.health_check_interval = cfg.get_float(
        "advanced", "health_check_interval_sec", default=5.0
    )
    args.stow_queue_max_mb = cfg.get_int("advanced", "stow_queue_max_mb", default=500)
    args.stow_queue_size = cfg.get_int("advanced", "stow_queue_size", default=20000)
    args.outbox_dir = cfg.get("outbox", "outbox_dir", default="clbridge/outbox")
    args.outbox_cleanup = cfg.get_bool("outbox", "cleanup", default=True)
    args.outbox_max_retries = cfg.get_int("outbox", "max_retries", default=10)
    args.outbox_retry_interval = cfg.get_int("outbox", "retry_interval", default=0)
    # Outbox STOW batching: flush a study-batch when any trigger fires.
    args.outbox_batch_max_sop = cfg.get_int("outbox", "batch_max_sop", default=100)
    args.outbox_batch_max_bytes = cfg.get_int(
        "outbox", "batch_max_bytes", default=96 * 1024 * 1024
    )
    args.outbox_batch_idle_sec = cfg.get_float("outbox", "batch_idle_sec", default=5.0)
    # STORE-FORWARD: when on, a C-STORE against a dead server (>=30s)
    # is PARKED (0x0000; blob already atomically on disk) instead of 0xC000.
    args.outbox_store_forward = cfg.get_bool("outbox", "store_forward", default=False)

    # [forward] - 0409: best-effort DIMSE->DIMSE mirror to a second archive.
    # Absent or enabled=false keeps behaviour byte-identical to pre-0409.
    args.forward_enabled = cfg.get_bool("forward", "enabled", default=False)
    args.forward_dest_ae = cfg.get("forward", "dest_ae", default=None)
    args.forward_dest_host = cfg.get("forward", "dest_host", default=None)
    args.forward_dest_port = cfg.get_int("forward", "dest_port", default=0)
    args.forward_max_queue = cfg.get_int("forward", "forward_max_queue", default=20000)

    # Backpressure: max concurrent C-STORE handlers
    max_cstore = cfg.get_int("advanced", "max_concurrent_cstore", default=None)
    if max_cstore is not None:
        max_cstore = max(1, min(50, max_cstore))
        set_max_concurrent_cstore(max_cstore)
        logging.info("[CONFIG] max_concurrent_cstore = %d", max_cstore)

    return args


# ---------------------------------------------------------------------------
# CONFIG-DISCIPLINE: _validate_config - validate all config values
# ---------------------------------------------------------------------------
def _validate_config(cfg):
    """Validate config values are within allowed ranges. Raises ValueError on violation."""
    errors = []

    def check(cond, msg):
        if not cond:
            errors.append(msg)

    # Server
    port = cfg.get_int("server", "port", default=8104)
    check(1 <= port <= 65535, f"server.port {port} out of range [1..65535]")

    # DICOMweb
    # 2908: clarus_url validated fail-fast (scheme, host, legacy-key removal,
    # mTLS pair symmetry). Raises ValueError with a clear message.
    validate_clarus_url(cfg)

    # Dimse
    cmove = cfg.get_int("dimse", "c_move_workers", default=2)
    check(0 <= cmove <= 16, f"dimse.c_move_workers {cmove} out of range [0..16]")
    cfind = cfg.get_int("dimse", "c_find_max_responses", default=200)
    check(
        0 <= cfind <= 10000,
        f"dimse.c_find_max_responses {cfind} out of range [0..10000]",
    )
    # Inbound DIMSE timeouts (bridge as SCP)
    idt = cfg.get_int("dimse", "inbound_dimse_timeout", default=60)
    check(10 <= idt <= 300, f"dimse.inbound_dimse_timeout {idt} out of range [10..300]")
    int_ = cfg.get_int("dimse", "inbound_network_timeout", default=0)
    check(
        int_ == 0 or (10 <= int_ <= 600),
        f"dimse.inbound_network_timeout {int_} out of range [0 or 10..600]",
    )
    iat = cfg.get_int("dimse", "inbound_acse_timeout", default=15)
    check(5 <= iat <= 60, f"dimse.inbound_acse_timeout {iat} out of range [5..60]")
    # Outbound DIMSE timeouts (bridge as SCU, C-MOVE)
    odt = cfg.get_int("dimse", "outbound_dimse_timeout", default=60)
    check(
        10 <= odt <= 600, f"dimse.outbound_dimse_timeout {odt} out of range [10..600]"
    )
    ont = cfg.get_int("dimse", "outbound_network_timeout", default=60)
    check(
        10 <= ont <= 600, f"dimse.outbound_network_timeout {ont} out of range [10..600]"
    )
    oat = cfg.get_int("dimse", "outbound_acse_timeout", default=15)
    check(5 <= oat <= 60, f"dimse.outbound_acse_timeout {oat} out of range [5..60]")
    # Outbound association attempts (main + fallback), range 1..5.
    oaa = cfg.get_int("dimse", "outbound_assoc_attempts", default=3)
    check(1 <= oaa <= 5, f"dimse.outbound_assoc_attempts {oaa} out of range [1..5]")
    # C-MOVE progress poll timeout (2608): bounds the held reactor's poll of
    # job progress. On expiry unfinished jobs are failed and 0xB000 returned.
    mpt = cfg.get_int("dimse", "move_poll_timeout", default=600)
    check(30 <= mpt <= 3600, f"dimse.move_poll_timeout {mpt} out of range [30..3600]")
    mas = cfg.get_int("dimse", "max_associations", default=16)
    check(4 <= mas <= 256, f"dimse.max_associations {mas} out of range [4..256]")

    # Logging
    level = cfg.get("logging", "log_level", default="INFO").upper()
    check(
        level in ("DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"),
        f"logging.log_level '{level}' must be DEBUG|INFO|WARNING|ERROR|CRITICAL",
    )

    # Metrics
    hp = cfg.get_int("metrics", "health_port", default=8105)
    check(1 <= hp <= 65535, f"metrics.health_port {hp} out of range [1..65535]")

    # Advanced
    ms = cfg.get_int("advanced", "max_store_mb", default=0)
    check(ms >= 0, f"advanced.max_store_mb {ms} must be >= 0")
    ht = cfg.get_int("advanced", "http_timeout", default=30)
    check(1 <= ht <= 300, f"advanced.http_timeout {ht} out of range [1..300]")
    sqm = cfg.get_int("advanced", "stow_queue_max_mb", default=500)
    check(
        10 <= sqm <= 2000, f"advanced.stow_queue_max_mb {sqm} out of range [10..2000]"
    )
    sqs = cfg.get_int("advanced", "stow_queue_size", default=20000)
    check(
        100 <= sqs <= 100000,
        f"advanced.stow_queue_size {sqs} out of range [100..100000]",
    )
    hi = cfg.get_float("advanced", "health_check_interval_sec", default=5.0)
    check(
        1.0 <= hi <= 60.0,
        f"advanced.health_check_interval_sec {hi} out of range [1.0..60.0]",
    )

    # DICOMweb health check mode
    hcm = cfg.get("dicomweb", "health_check_mode", default="probe").lower()
    check(
        hcm in ("probe", "always", "never"),
        f"dicomweb.health_check_mode '{hcm}' must be probe|always|never",
    )

    # Outbox STOW batching
    bs = cfg.get_int("outbox", "batch_max_sop", default=100)
    check(1 <= bs <= 500, f"outbox.batch_max_sop {bs} out of range [1..500]")
    bb = cfg.get_int("outbox", "batch_max_bytes", default=96 * 1024 * 1024)
    check(
        1 <= bb <= 2048 * 1024 * 1024,
        f"outbox.batch_max_bytes {bb} out of range [1..2048MB]",
    )
    bi = cfg.get_float("outbox", "batch_idle_sec", default=5.0)
    check(0.1 <= bi <= 60.0, f"outbox.batch_idle_sec {bi} out of range [0.1..60]")

    # [forward] - 0409: best-effort DIMSE->DIMSE second-archive mirror.
    # When enabled, the destination modality/host/port are REQUIRED and the
    # port/queue-size ranges are enforced. When disabled (or the section
    # absent), nothing is required - an upgrade changes no behaviour.
    if cfg.get_bool("forward", "enabled", default=False):
        if not cfg.get("forward", "dest_ae", default=None):
            errors.append("forward.dest_ae is required when forward.enabled=true")
        if not cfg.get("forward", "dest_host", default=None):
            errors.append("forward.dest_host is required when forward.enabled=true")
        fdp = cfg.get_int("forward", "dest_port", default=0)
        check(1 <= fdp <= 65535, f"forward.dest_port {fdp} out of range [1..65535]")
        fqm = cfg.get_int("forward", "forward_max_queue", default=20000)
        check(
            1 <= fqm <= 100000,
            f"forward.forward_max_queue {fqm} out of range [1..100000]",
        )

    if errors:
        raise ValueError("Configuration errors:\n  " + "\n  ".join(errors))

    logging.info("[CONFIG] All values validated - OK")


# _init_components - initialize managers and registries
# ---------------------------------------------------------------------------
class _LockedDb:
    """The `SharedDb.locked()` context: one connection, one lock."""

    def __init__(self, lock, conn):
        self._lock = lock
        self._conn = conn

    def __enter__(self):
        self._lock.acquire()
        return self._conn

    def __exit__(self, *args):
        self._lock.release()


class _MoveJobsDb:
    """Minimal SharedDb-compatible wrapper.

    One file, one connection, one lock. Two consumers share the shape - the
    C-MOVE state machine (`move_jobs.db`) and the Storage Commitment
    tracker's outstanding batches (`commit_jobs.db`) - and a single
    serialized writer is what keeps both free of SQLite busy-handler
    tuning. They get separate FILES, not separate classes: a tracker write
    that cannot proceed must not be able to block a C-MOVE claim.
    """

    def __init__(self, path):
        import sqlite3 as _sqlite3

        self._conn = _sqlite3.connect(path, check_same_thread=False)
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._lock = threading.Lock()

    def locked(self):
        return _LockedDb(self._lock, self._conn)

    def close(self):
        with self._lock:
            try:
                self._conn.close()
            except Exception:
                pass


def _init_components(args):
    """Initialize SopIndexManager, MoveJobManager, printer_registry, driver_registry.

    Returns (sop_index, move_jobs, printer_registry, driver_registry).
    """
    sop_index = SopIndexManager(index_dir=args.sop_index_dir)
    # move_jobs keeps its own SQLite - it's the right tool for state machine + multi-worker claim.
    move_jobs_db_path = os.path.join(
        os.path.dirname(args.sop_index_dir), "move_jobs.db"
    )
    # SCP peer auto-remove threshold is a fixed constant.
    move_jobs = MoveJobManager(
        shared_db=_MoveJobsDb(move_jobs_db_path),
        max_workers=args.c_move_workers,
        scp_remove_failures=_SCP_FAILURES_BEFORE_REMOVE,
    )
    move_jobs.lazyload_peers_interval = args.lazyload_interval if args.lazyload else 0

    # Bridge: recover in-progress jobs from previous crash
    recovered = move_jobs.recover_in_progress_jobs()
    if recovered > 0:
        logging.warning(
            "[C-MOVE] Recovered %d in-progress job(s) from previous session. "
            "They are now available for retry by the C-MOVE worker.",
            recovered,
        )

    # Bridge: peers are loaded on first C-MOVE, not at startup.
    # Weasis/OHIF/modality will NEVER be running before the bridge.
    # Lazy registration via lookup_scp miss - reload_peers_from_file.

    # Bridge: load printer registry
    printer_registry = _load_printer_registry(args.printers_yaml)

    # Bridge: register C-PRINT drivers - only if drivers/ folder is present
    driver_registry = {}
    if _CPRINT_AVAILABLE:
        # pyright: ignore[reportOptionalCall] - guarded by _CPRINT_AVAILABLE
        driver_registry["generic_dicom"] = GenericDicomPrintDriver()
        logging.info("[C-PRINT] Generic DICOM Print driver loaded")
    else:
        logging.info(
            "[C-PRINT] drivers/ folder not found - print support disabled. "
            "Download drivers/ from https://github.com/clarus-pacs/clarus to enable."
        )

    return sop_index, move_jobs, printer_registry, driver_registry


# ---------------------------------------------------------------------------
# BR-P31: SOP index periodic cleanup worker - prevents unbounded :memory: growth
# ---------------------------------------------------------------------------
def _sop_index_cleanup_worker(sop_index, max_age_hours=24):
    """Background thread: remove SOP index entries older than max_age_hours.

    Without periodic cleanup, the SopIndexManager's :memory: SQLite B-tree grows
    unbounded - 55K objects = ~15-25 MB, millions of objects after weeks of 24/7
    operation = hundreds of MB of RSS - Python GC gen2 pauses - GIL stalls.

    Runs once per hour. Stops when _shutdown_requested is set.
    """
    from clean_code.utils import _shutdown_requested

    max_age_seconds = max_age_hours * 3600
    logging.info(
        "[SOP-INDEX] Cleanup worker started (max age: %dh)",
        max_age_hours,
    )

    while not _shutdown_requested.is_set():
        _shutdown_requested.wait(timeout=3600.0)  # Every hour
        if _shutdown_requested.is_set():
            break
        try:
            before = sop_index.count()
            sop_index.cleanup(max_age_seconds)
            after = sop_index.count()
            if before != after:
                logging.info(
                    "[SOP-INDEX] Cleanup: %d entries removed, %d remain",
                    before - after,
                    after,
                )
        except Exception as e:
            logging.error("[SOP-INDEX] Cleanup error: %s", e)

    logging.info("[SOP-INDEX] Cleanup worker stopped")


# ---------------------------------------------------------------------------
# Duplicate instance detection - three layers
# ---------------------------------------------------------------------------


def _check_port_available(port: int, bind_addr: str, name: str) -> None:
    """Layer 1: TCP bind() with the listener's own reuse semantics.

    The check must match how the real listeners bind, or it reports a free
    port as taken: a listener that closes leaves the port in TIME_WAIT for
    about a minute on Linux, and a probe without SO_REUSEADDR fails with
    EADDRINUSE for that whole window - which turns an ordinary restart into a
    manual wait.

    Windows is the exception in the other direction: there SO_REUSEADDR lets a
    second socket bind a port that is ACTIVELY listening, so it would weaken
    exactly the duplicate-instance guard this function exists for.
    SO_EXCLUSIVEADDRUSE is its counterpart - it refuses a port another socket
    holds, including one in TIME_WAIT.
    """
    import socket

    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        if sys.platform == "win32":
            exclusive = getattr(socket, "SO_EXCLUSIVEADDRUSE", None)
            if exclusive is not None:
                sock.setsockopt(socket.SOL_SOCKET, exclusive, 1)
        else:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind((bind_addr, port))
    except OSError as e:
        logging.error(
            "[FATAL] bind() to %s:%d (%s) failed: %s - "
            "another process may be using this port. Refusing to start.",
            bind_addr,
            port,
            name,
            e,
        )
        sys.exit(1)
    finally:
        sock.close()


def _check_dimse_unique(host: str, port: int, my_ae_title: str) -> None:
    """Layer 2: C-ECHO probe against the DICOMweb server via pynetdicom.

    Probes the Clarus Bridge AE over DIMSE; a live association proves
    the configured server address and AE settings are reachable.
    - AE Title is echoed back by the server (Clarus Bridge).
    - AE Title mismatch is reported without aborting the DICOMweb path.
    """
    from pynetdicom import AE as ProbeAE
    from pynetdicom.sop_class import Verification

    # Resolve 0.0.0.0 / :: to loopback - can't connect to bind-any address.
    probe_host = host
    if host in ("0.0.0.0", "::"):
        probe_host = "127.0.0.1"

    probe_ae_title = (f"{my_ae_title}_PROBE")[:16]  # DICOM AE: max 16 chars
    probe_ae = ProbeAE(ae_title=probe_ae_title)
    probe_ae.add_requested_context(Verification)
    probe_ae.network_timeout = 2
    probe_ae.acse_timeout = 2

    # Suppress pynetdicom internal logging during probe - connection
    # refusal is expected (means we're the only process on this port).
    _probe_logger = logging.getLogger("pynetdicom")
    _probe_prev_level = _probe_logger.level
    _probe_logger.setLevel(logging.CRITICAL + 1)

    assoc = None
    try:
        # Call as the bridge's own AE title - with require_called_aet=True a
        # running bridge instance rejects the generic probe title.
        assoc = probe_ae.associate(probe_host, port, ae_title=my_ae_title)
        if assoc.is_established:
            remote_ae = (
                assoc.acceptor.ae_title
                if hasattr(assoc, "acceptor") and assoc.acceptor
                else "unknown"
            )
            if remote_ae == my_ae_title:
                logging.error(
                    "[FATAL] C-ECHO: another Clarus Bridge (AE=%s) already "
                    "listening on %s:%d. Refusing to start.",
                    remote_ae,
                    host,
                    port,
                )
            else:
                logging.error(
                    "[FATAL] C-ECHO: DIMSE port %s:%d already in use "
                    "by AE=%s. Refusing to start.",
                    host,
                    port,
                    remote_ae,
                )
            sys.exit(1)
    except Exception:
        # - - - - -. -.
        pass
    finally:
        _probe_logger.setLevel(_probe_prev_level)
        try:
            if assoc is not None and assoc.is_established:
                assoc.release()
        except Exception:
            pass


def _default_pid_path(port: int) -> str:
    """Platform default path for the bridge PID file.

    Linux: `/var/run/clbridge-{port}.pid`. A PID file is a system-level
    thing on Unix; the FHS location is what an operator's init scripts
    expect. It is NOT guaranteed writable (that needs root), which is
    exactly why `_write_pid_file` degrades instead of dying - see below.

    Elsewhere (Windows, macOS): next to the executable/script, which is
    the historical location and always writable by the user who can start
    the process in the first place.
    """
    if sys.platform.startswith("linux"):
        return os.path.join("/var/run", f"clbridge-{port}.pid")
    script_dir = os.path.dirname(
        os.path.abspath(sys.executable if getattr(sys, "frozen", False) else __file__)
    )
    return os.path.join(script_dir, f"clbridge-{port}.pid")


def _fallback_pid_path(port: int) -> str:
    """Fallback PID path when the default is not writable.

    Next to the script/executable. This is the pre-04.09 behaviour and it
    worked everywhere except when the install directory itself was read-only.
    """
    script_dir = os.path.dirname(
        os.path.abspath(sys.executable if getattr(sys, "frozen", False) else __file__)
    )
    return os.path.join(script_dir, f"clbridge-{port}.pid")


def _pid_is_alive(pid: int) -> bool:
    """True when `pid` names a running process.

    `os.kill(pid, 0)` is the POSIX idiom, but on Windows it does NOT probe:
    CPython implements it with TerminateProcess, so signal 0 KILLS the process
    it was asked to check. The bridge's own PID file then took down whatever
    held that pid - which on a recycled pid is an unrelated program.
    """
    if pid <= 0:
        return False
    if os.name == "nt":
        # Windows: ask for the process handle and its exit code instead.
        import ctypes
        from ctypes import wintypes

        PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
        STILL_ACTIVE = 259
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        handle = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
        if not handle:
            err = ctypes.get_last_error()
            # 5 = ACCESS_DENIED: the process exists but is not ours to inspect.
            return err == 5
        try:
            code = wintypes.DWORD()
            if not kernel32.GetExitCodeProcess(handle, ctypes.byref(code)):
                return True  # cannot tell: assume alive and refuse to steal
            return code.value == STILL_ACTIVE
        finally:
            kernel32.CloseHandle(handle)
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        # The process exists but belongs to another user.
        return True


def _write_pid_file(path: str) -> None:
    """Layer 3: atomic PID file via O_EXCL.

    Order: platform default (/var/run on Linux, next to the script
    elsewhere); if not writable, fall back next to the script; if
    neither is writable, WARN and run WITHOUT a PID file. FATAL only
    when a live foreign pid occupies the path.

    The FATAL path exits via os._exit(1) because _write_pid_file is
    called from _fatal_pid_error.
    """
    pid = str(os.getpid())
    try:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
    except FileExistsError:
        with open(path) as f:
            old_pid = f.read().strip()
        try:
            alive = _pid_is_alive(int(old_pid))
        except ValueError:
            alive = False
        if alive:
            logging.error(
                "[FATAL] PID file %s exists and pid=%s is ALIVE. "
                "Another instance is running. Remove %s manually if stale.",
                path,
                old_pid,
                path,
            )
            # os._exit: immediate process termination, works on Win+*nix.
            # sys.exit only raises SystemExit - non-daemon threads prevent exit.
            os._exit(1)
        else:
            logging.warning(
                "[WARN] Stale PID file %s (pid=%s is dead). Overwriting.",
                path,
                old_pid,
            )
            try:
                os.remove(path)
                fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
            except OSError as e:
                logging.warning(
                    "[WARN] Cannot rewrite stale PID file %s: %s. Continuing without it.",
                    path,
                    e,
                )
                return
    except OSError as e:
        # PermissionError is an OSError. NOT fatal: the service must start.
        logging.warning(
            "[WARN] Cannot create PID file %s: %s. Continuing without it.",
            path,
            e,
        )
        return
    with os.fdopen(fd, "w") as f:
        f.write(pid)
    logging.info("[PID] %s written (pid=%s)", path, pid)


def _acquire_pid_file(port: int) -> str:
    """Resolve, write and register the PID file. Returns the path in use.

    There is deliberately NO configuration for this: no config key, no CLI
    flag, no environment variable. The PID file is an internal artefact and
    the operator never needs to know about it.

    Order (owner directive, incident 04.09):
      1. platform default (`/var/run/...` on Linux, next to the script
         elsewhere);
      2. if that is not writable - fall back next to the script;
      3. if neither is writable - WARN and run WITHOUT a PID file.

    A live foreign pid is fatal, because two instances must not silently
    coexist.
    """
    candidates = []
    default = _default_pid_path(port)
    fallback = _fallback_pid_path(port)
    candidates.append(default)
    if fallback != default:
        candidates.append(fallback)

    for candidate in candidates:
        directory = os.path.dirname(candidate) or "."
        if not os.path.isdir(directory):
            logging.warning(
                "[WARN] PID directory %s does not exist; skipping %s.",
                directory,
                candidate,
            )
            continue
        if not os.access(directory, os.W_OK):
            logging.warning(
                "[WARN] PID directory %s is not writable; skipping %s.",
                directory,
                candidate,
            )
            continue
        # A live foreign pid inside _write_pid_file exits the process; a
        # stale one is overwritten and we return that same path.
        _write_pid_file(candidate)
        return candidate

    logging.warning(
        "[WARN] No writable PID file location found (tried: %s). "
        "Continuing without a PID file.",
        ", ".join(candidates),
    )
    return ""


# ---------------------------------------------------------------------------
# _start_background_services - HTTP server, C-MOVE workers, cleanup
# ---------------------------------------------------------------------------
def _start_background_services(
    args, driver_registry, printer_registry, move_jobs, sop_index, sop_registry
):
    """Start HTTP server, C-MOVE workers, cleanup thread.

    Returns (http_server, http_thread, cmove_thread, cleanup_thread, sop_cleanup_thread,
             outbox, outbox_workers, health_checker, commit_tracker, commit_thread,
             forward_outbox, forward_worker_thread).
    """
    # Outbox Queue - filesystem-backed reliable DICOM persistence + delivery
    max_queue = args.stow_queue_size
    outbox = OutboxQueue(
        outbox_dir=args.outbox_dir,
        max_queue=max_queue,
        max_retries=args.outbox_max_retries,
        cleanup=args.outbox_cleanup,
        retry_interval=args.outbox_retry_interval,
    )
    recovered, undelivered = outbox.recover_stuck()
    outbox_pending.set(outbox.pending_count())
    if recovered > 0:
        outbox_recovery_total.inc(recovered)
    # Update disk bytes metric
    try:
        total = sum(
            os.path.getsize(os.path.join(root, f))
            for root, _, files in os.walk(args.outbox_dir)
            for f in files
        )
        outbox_disk_bytes.set(total)
    except OSError:
        pass

    # Forwarder: SECOND OutboxQueue instance pointed at its own
    # directory (<outbox_dir>/forward). Only constructed when forward is
    # enabled - an upgrade without the config block creates NO new dirs and
    # changes NO metric behaviour. Feeding it forward_pending (not
    # outbox_pending) keeps the two queues' gauges isolated. The drain worker
    # that consumes this queue is the drain worker; reception-only wiring lives here.
    forward_outbox = None
    if getattr(args, "forward_enabled", False):
        forward_dir = os.path.join(args.outbox_dir, "forward")
        forward_outbox = OutboxQueue(
            outbox_dir=forward_dir,
            max_queue=args.forward_max_queue,
            max_retries=10,  # 0409: forward-queue retry budget (like outbox)
            cleanup=args.outbox_cleanup,
            pending_gauge=forward_pending,
            undelivered_total=forward_undelivered_total,
        )
        logging.info(
            "[FORWARD] secondary queue enabled -> %s (max_queue=%d, max_retries=10)",
            forward_dir,
            args.forward_max_queue,
        )

    # HealthChecker - background poller + pre-flight gate
    hc_mode = getattr(args, "health_check_mode", "probe")
    hc_interval = getattr(args, "health_check_interval", 5.0)
    health_checker = HealthChecker(
        clarus_url=args.clarus_url,
        mode=hc_mode,
        interval=hc_interval,
        http_timeout=args.http_timeout,
        tls=args.tls_pair,
    )
    health_checker.start()

    # Bridge: start HTTP server in daemon thread
    bridge_up.set(1)  # mark bridge as running

    # P2.10: ThreadingHTTPServer - each request handled in its own thread.
    # Single-threaded HTTPServer serialized /health behind heavy /metrics
    # (Prometheus rendering), causing >5s response times under load.
    HandlerCls = make_bridge_handler(
        driver_registry,
        printer_registry,
        args.ae_title,
        outbox=outbox,
        health_checker=health_checker,
        move_jobs=move_jobs,
    )
    http_server = ThreadingHTTPServer(
        (args.http_print_bind, args.http_port),
        HandlerCls,
    )
    http_thread = threading.Thread(
        target=http_server.serve_forever,
        name="http-bridge-server",
        daemon=False,
    )
    http_thread.start()
    logging.info(
        "[HTTP] Bridge server listening on %s:%d (/print, /metrics, /health)",
        args.http_print_bind,
        args.http_port,
    )

    # Start single outbox background worker (KISS: one worker, N SOPs per STOW-RS)
    outbox_workers = _start_outbox_workers(
        outbox,
        args.clarus_url,
        args.http_timeout,
        sop_index,
        move_jobs,
        batch_max_sop=args.outbox_batch_max_sop,
        batch_max_bytes=args.outbox_batch_max_bytes,
        batch_idle_sec=args.outbox_batch_idle_sec,
        health_checker=health_checker,
        tls=args.tls_pair,
        bridge_ae_title=args.ae_title,
    )
    logging.info(
        "[OUTBOX-WORKER] Started 1 worker, outbox: %s "
        "(batch: sop<=%d bytes<=%d idle<=%.0fs)",
        args.outbox_dir,
        args.outbox_batch_max_sop,
        args.outbox_batch_max_bytes,
        args.outbox_batch_idle_sec,
    )

    # Bridge: start C-MOVE background worker pool
    cmove_thread = threading.Thread(
        target=_cmove_worker,
        args=(
            move_jobs,
            args.clarus_url,
            args.http_timeout,
            args.ae_title,
        ),
        kwargs={
            "num_workers": args.c_move_workers,
            "sop_registry": sop_registry,
            "storage_commit_after_move": not args.no_storage_commit,
            "sop_index": sop_index,
            "peers_file": args.peers_file,
            "outbound_network_timeout": args.outbound_network_timeout,
            "outbound_dimse_timeout": args.outbound_dimse_timeout,
            "outbound_acse_timeout": args.outbound_acse_timeout,
            "outbound_assoc_attempts": args.outbound_assoc_attempts,
            "tls": args.tls_pair,
            "qa_url": args.qa_url,
        },
        name="cmove-worker-pool",
        daemon=False,
    )
    cmove_thread.start()
    logging.info(
        "[C-MOVE] Background worker pool started (%d workers)", args.c_move_workers
    )

    # Bridge: start C-MOVE cleanup worker
    cleanup_thread = threading.Thread(
        target=_cmove_cleanup_worker,
        args=(move_jobs, 24),
        name="cmove-cleanup",
        daemon=False,
    )
    cleanup_thread.start()

    # BR-P31: start SOP index cleanup worker - prevents unbounded :memory: growth
    sop_cleanup_thread = threading.Thread(
        target=_sop_index_cleanup_worker,
        args=(sop_index, 24),
        name="sop-index-cleanup",
        daemon=False,
    )
    sop_cleanup_thread.start()

    # Async Storage Commitment report loop (N-EVENT-REPORT). Fire-and-
    # forget: N-ACTION registers a batch; this worker resolves it and sends
    # ONE report back.
    #
    # Two modes (2409), selected once here by storage_commitment_presence:
    #   false (default) - the readiness gate is the local probe: all SOPs
    #     confirmed via WADO-RS, or the 300s deadline, then the local
    #     heuristic's verdict (unconfirmed -> 0x0110).
    #   true - the readiness gate is the bridge's OWN outbox delivery state
    #     (a C-STORE returns before its STOW completes, so an immediate
    #     origin-side ask would report false "not stored" verdicts), then
    #     ONE POST to the origin's Commit resource and its verdict verbatim.
    def _verify_cb(sop_class, sop_uid):
        return (
            _sop_confirm(
                sop_uid,
                sop_index,
                args.clarus_url,
                args.http_timeout,
                tls=args.tls_pair,
            )
            == 0
        )

    stc_presence = bool(getattr(args, "storage_commitment_presence", False))

    def _resolve_stc(job):
        """Ask the origin ONCE and report its verdict verbatim (true-mode).

        Only wired in true-mode; false-mode leaves `resolve` unset so the
        tracker runs its own heuristic, byte-for-byte as before.
        """
        committed, failed, sentinel = _stc_commit_request(
            args.clarus_url,
            job.tx_uid,
            job.sops,
            http_timeout=args.http_timeout,
            tls=args.tls_pair,
        )
        if sentinel == STC_NO_RESOURCE:
            # The WARN already named the misconfiguration; there is
            # deliberately no fallback to the local path.
            logging.warning(
                "[N-EVENT] tx=%s: origin has no Commit resource - reporting "
                "every SOP failed (0x0110)",
                job.tx_uid,
            )
        event_type = 1 if not failed else 2
        send_n_event_report(
            job.host,
            job.port,
            job.calling_ae,
            job.tx_uid,
            event_type,
            committed,
            failed,
            args.ae_title,
        )

    # Storage Commitment batches are a promise to the modality (PS3.4
    # J.3.3.1: the N-EVENT-REPORT IS the result), so an outstanding batch has
    # to survive a restart. It gets its OWN database file rather than a table
    # alongside move_jobs: a tracker write must not contend with a C-MOVE
    # claim, and the two have nothing to join on.
    commit_db_path = os.path.join(os.path.dirname(args.sop_index_dir), "commit_jobs.db")

    if stc_presence:
        # Origin-owned commitment: the readiness signal is the bridge's OWN
        # delivery state, not a probe (a C-STORE returns before its STOW
        # completes, so an immediate origin-side ask would report false "not
        # stored" verdicts). `delivered` is the outbox's read-side query.
        commit_tracker = CommitTracker(
            verify=lambda sop_class, sop_uid: outbox.delivered(sop_uid),
            report=functools.partial(
                send_n_event_report, bridge_ae_title=args.ae_title
            ),
            resolve=_resolve_stc,
            deadline_sec=300.0,
            db=_MoveJobsDb(commit_db_path),
        )
    else:
        # Unchanged local heuristic: the tracker's own _resolve owns it.
        commit_tracker = CommitTracker(
            verify=_verify_cb,
            report=functools.partial(
                send_n_event_report, bridge_ae_title=args.ae_title
            ),
            deadline_sec=300.0,
            db=_MoveJobsDb(commit_db_path),
        )
    commit_thread = threading.Thread(
        target=commit_tracker.run,
        name="commit-tracker",
        daemon=False,
    )
    commit_thread.start()
    # Expose the tracker to the /metrics scrape (pending-batch gauges). The
    # HTTP server is already serving, so the ref is attached to its handler
    # class once the tracker exists.
    try:
        HandlerCls._commit_tracker_ref = commit_tracker
    except Exception:
        pass
    logging.info(
        "Storage Commitment: %s",
        "origin STC" if stc_presence else "local heuristic",
    )

    # Forwarder: separate DIMSE drain thread for the second,
    # NON-authoritative archive. Started ONLY when the forward queue exists
    # (enabled). It shares NO state with the primary STOW drain thread and
    # runs on its own queue, so a dead second server can only pause its own
    # batch - never the primary reception/delivery (isolation).
    forward_worker_thread = None
    if forward_outbox is not None:
        forward_worker_thread = threading.Thread(
            target=_forward_worker,
            args=(
                forward_outbox,
                args.forward_dest_ae,
                args.ae_title,
                args.forward_dest_host,
                args.forward_dest_port,
            ),
            kwargs={
                "network_timeout": args.outbound_network_timeout,
                "dimse_timeout": args.outbound_dimse_timeout,
                "acse_timeout": args.outbound_acse_timeout,
            },
            name="forward-worker",
            daemon=False,
        )
        forward_worker_thread.start()
        logging.info(
            "[FORWARD] drain worker started: %s@%s:%d (best-effort DIMSE mirror)",
            args.forward_dest_ae,
            args.forward_dest_host,
            args.forward_dest_port,
        )

    return (
        http_server,
        http_thread,
        cmove_thread,
        cleanup_thread,
        sop_cleanup_thread,
        outbox,
        outbox_workers,
        health_checker,
        commit_tracker,
        commit_thread,
        forward_outbox,
        forward_worker_thread,
    )


# ---------------------------------------------------------------------------
# Outbox Queue: background STOW workers (replaces sync STOW-RS in handler)
# ---------------------------------------------------------------------------


def _outbox_worker(
    outbox,
    clarus_url,
    http_timeout,
    sop_index,
    move_jobs,
    batch_max_sop=100,
    batch_max_bytes=96 * 1024 * 1024,
    batch_idle_sec=5.0,
    health_checker=None,
    tls=None,
    bridge_ae_title="CLARUS_BRIDGE",
):
    """Background worker: drains outbox, batches same-study SOPs into one STOW-RS.

    Fieldtest: 1 SLICE = 1 POST -> N SLICES = 1 POST (1063 -> ~20).
    Flush triggers, whoever fires first:
      - count  >= batch_max_sop
      - bytes  >= batch_max_bytes (buffer cap; on-disk blob size as proxy)
      - idle   >= batch_idle_sec  (study tail, non-empty buffer)
      - study boundary: SWITCH to another study flushes the tail IMMEDIATELY.
    Studies never mix in one batch. Single worker drains sequentially; each
    flush is independent (no cross-study waiting). P8 raw pass-through kept
    per part (byte-identical content for CAS dedup).

    Stage 5 (store_forward): while the server is down (health_checker reports
    unhealthy), the worker PAUSES instead of burning STOW retries
    (MAX_RETRIES=10 -> undelivered -> manual intervention). No HTTP on this
    hot path - it only reads the health checker's cached event.

    nginx-way: one worker drains STOW. Outbox decouples C-STORE from
    delivery: C-STORE ACKs once the blob is on disk, worker forwards later.
    """
    import os

    import httpx

    from clean_code.http_helpers import stow_batch_instances
    from clean_code.outbox import STUCK_ITEM_CHECK_INTERVAL

    # P30: persistent HTTP client -- Keep-Alive. One handshake for the whole
    # queue drain instead of one per instance (x2 throughput).
    # 2908: cert=tls enables mTLS when configured; verify is ALWAYS on
    # (httpx default) - there is no knob to disable it.
    _http = httpx.Client(
        timeout=httpx.Timeout(http_timeout + 10),
        limits=httpx.Limits(max_keepalive_connections=1, max_connections=1),
        cert=tls,
    )

    _last_stuck_check = time.time()  # periodic reaper timer
    # P8: watchdog -- last successful delivery for liveness monitoring
    _last_progress = time.time()
    _progress_lock = threading.Lock()
    # Log "paused"/"resumed" once per transition, not every 5s poll.
    _was_paused = False
    # 2908: 507 backpressure state (Retry-After pause, files stay pending).
    _pause_until = 0.0
    _was_507_paused = False

    # Delivery summary ("432/432 delivered", not 432 "201 Created").
    _delivered_total = 0
    _delivered_failed = 0
    _last_summary_log = time.time()  # throttle progress logs to every 30s
    _queue_was_empty = True  # detect queue-empty -> summary transition
    # Per-study tracking: {study_uid -> {delivered, failed, patient, modalities}}
    _study_stats: dict[str, dict] = {}
    _current_study: str | None = None

    # Batching state: current same-study buffer + its byte total.
    _batch: list = []  # (sop_uid, ds, blob_path, raw_bytes, dataset_modified)
    _batch_study: str | None = None
    _batch_bytes = 0

    def _log_study_start(sop_uid, ds, study_uid):
        """First SOP of a study: log and seed per-study stats."""
        if study_uid not in _study_stats:
            try:
                _patient_id = mask_patient_id(str(getattr(ds, "PatientID", "?")))
            except Exception:
                _patient_id = "?"
            try:
                _modality = str(getattr(ds, "Modality", "??"))
            except Exception:
                _modality = "??"
            _study_stats[study_uid] = {
                "delivered": 0,
                "failed": 0,
                "patient": _patient_id,
                "modalities": {_modality} if _modality and _modality != "??" else set(),
            }
        else:
            # Track all modalities seen (studies often mix DX + SR etc.)
            try:
                _modality = str(getattr(ds, "Modality", "??"))
                if _modality and _modality != "??":
                    _study_stats[study_uid].setdefault("modalities", set()).add(
                        _modality
                    )
            except Exception:
                pass

    def _deliver_batch():
        """Send the accumulated same-study batch, mark per-SOP outcomes."""
        nonlocal _batch, _batch_study, _batch_bytes
        nonlocal _delivered_total, _delivered_failed, _last_summary_log
        nonlocal _queue_was_empty, _current_study
        nonlocal _pause_until
        if not _batch:
            return
        batch_study = _batch_study
        mods = (
            ", ".join(
                sorted(_study_stats.get(batch_study, {}).get("modalities", set()))
            )
            or "??"
        )
        logging.info(
            "[OUTBOX] Flushing batch: study=%s %d SOP(s) %d bytes %s",
            (batch_study or "")[:50],
            len(_batch),
            _batch_bytes,
            mods,
        )
        payloads = [
            (uid, ds, (None if modified else raw))
            for uid, ds, _bp, raw, modified in _batch  # P8: raw stays as-is unless modified
        ]
        try:
            results, backoff = stow_batch_instances(
                payloads,
                clarus_url,
                http_timeout=http_timeout,
                client=_http,
                tls=tls,
            )
        except Exception as e:
            logging.error("[OUTBOX-WORKER] stow_batch_instances raised: %s", e)
            logging.debug("[OUTBOX-WORKER] traceback:", exc_info=True)
            results, backoff = None, None

        if results is None:
            # Total transport failure not mapped per-SOP: fail the whole batch.
            for uid, ds, blob_path, raw_bytes, modified in _batch:
                outbox.mark_failed(uid, "Batch transport error")
            _batch = []
            _batch_study = None
            _batch_bytes = 0
            with _progress_lock:
                _last_progress = time.time()
            return

        # 2908: 507 Insufficient Storage -> backpressure, NOT a failure.
        # Files go back to pending without a retry bump; the worker pauses
        # for Retry-After seconds (clamped 5..300, default 30).
        if backoff is not None:
            batch_len = len(_batch)
            for uid, ds, blob_path, raw_bytes, modified in _batch:
                outbox.requeue(uid, f"Server 507 - retry after {backoff}s")
            _batch = []
            _batch_study = None
            _batch_bytes = 0
            with _progress_lock:
                _last_progress = time.time()
            outbox_pending_files.set(outbox.pending_count())
            logging.warning(
                "[OUTBOX-WORKER] Server 507 - %d SOP(s) returned to pending, "
                "pausing %.0fs (Retry-After)",
                batch_len,
                backoff,
            )
            _pause_until = time.time() + backoff
            return

        results_by_uid = {uid: (st, hs, msg) for uid, st, hs, msg in results}
        batch_ok = 0

        for uid, ds, blob_path, raw_bytes, modified in _batch:
            status, http_status, msg = results_by_uid.get(
                uid, (0xC000, None, "No outcome")
            )
            # P1-4: watchdog heartbeat after each STOW round.
            with _progress_lock:
                _last_progress = time.time()

            study_uid = str(getattr(ds, "StudyInstanceUID", "")) or ""
            if status == 0x0000:
                # sidecar no longer needed (audit trail embedded in
                # payload). Delete BEFORE mark_delivered so retry keeps it.
                if modified:
                    outbox.delete_mods_sidecar(blob_path)
                outbox.mark_delivered(uid)
                batch_ok += 1
                _delivered_total += 1
                if study_uid and study_uid in _study_stats:
                    _study_stats[study_uid]["delivered"] += 1
                if sop_index is not None:
                    try:
                        series_uid = str(getattr(ds, "SeriesInstanceUID", ""))
                        if uid and study_uid and series_uid:
                            sop_index.insert_sop(uid, study_uid, series_uid)
                    except Exception:
                        pass
            else:
                outbox.mark_failed(uid, msg or "Unknown error")
                _delivered_total += 1
                _delivered_failed += 1
                if study_uid and study_uid in _study_stats:
                    _study_stats[study_uid]["delivered"] += 1
                    _study_stats[study_uid]["failed"] += 1
                retries = outbox._get_retry_count(uid)
                if retries >= outbox.MAX_RETRIES:
                    outbox.move_to_undelivered(uid)

        logging.info(
            "[OUTBOX] Batch delivered %d/%d OK",
            batch_ok,
            len(_batch),
        )
        _batch = []
        _batch_study = None
        _batch_bytes = 0

        # 2908: observability - last successful STOW flush (unix time).
        if batch_ok > 0:
            outbox_last_success_timestamp.set(time.time())
        outbox_pending_files.set(outbox.pending_count())

        # P30: delivery summary -- progress every 100 SOPs or 30s.
        _now = time.time()
        pending = outbox.pending_count()
        if _delivered_total > 0 and (
            _delivered_total % 100 == 0 or _now - _last_summary_log >= 30.0
        ):
            _last_summary_log = _now
            _study_label = f"study={_current_study[:40]}" if _current_study else ""
            logging.info(
                "[OUTBOX] %d delivered, %d failed - %s",
                _delivered_total - _delivered_failed,
                _delivered_failed,
                _study_label,
            )

        # Queue drained: log per-study summaries and reset counters.
        if pending == 0 and not _queue_was_empty:
            _queue_was_empty = True
            _last_summary_log = _now
            for study_uid, s in list(_study_stats.items()):
                mods = ", ".join(sorted(s.get("modalities", set()))) or "??"
                logging.info(
                    "[OUTBOX] Study delivered: study=%s patient=%s modality=%s %d/%d OK",
                    study_uid[:50],
                    s.get("patient", "?"),
                    mods,
                    s.get("delivered", 0) - s.get("failed", 0),
                    s.get("delivered", 0),
                )
            _study_stats.clear()
            _current_study = None
            _delivered_total = 0
            _delivered_failed = 0
        elif pending > 0:
            _queue_was_empty = False

    def _dequeue_safe():
        try:
            return outbox.dequeue()
        except Exception as e:
            logging.error("[OUTBOX-WORKER] dequeue() failed: %s", e)
            logging.debug("[OUTBOX-WORKER] dequeue traceback:", exc_info=True)
            _shutdown_requested.wait(timeout=1.0)
            return (None, None, None, None)

    while not _shutdown_requested.is_set():
        # STORE-FORWARD: if the server is down, pause instead of
        # burning STOW retries (MAX_RETRIES=10 -> undelivered -> manual).
        # Reads the health checker's cached event - no HTTP on the hot path.
        if health_checker is not None and not health_checker.is_healthy():
            if not _was_paused:
                _was_paused = True
                logging.warning(
                    "[OUTBOX-WORKER] Server down - worker paused "
                    "(no STOW, no HTTP); will resume on recovery"
                )
            _shutdown_requested.wait(5.0)
            with _progress_lock:
                _last_progress = time.time()
            continue
        elif _was_paused:
            _was_paused = False
            logging.info("[OUTBOX-WORKER] Server up - delivery resumed")

        # 2908: 507 backpressure - pause for Retry-After seconds (clamped
        # 5..300 by retry_after_seconds, default 30). Files stay pending.
        # WARN once per paused/resumed transition.
        if _pause_until > time.time():
            if not _was_507_paused:
                _was_507_paused = True
                logging.warning(
                    "[OUTBOX-WORKER] Server 507 - paused %.0fs (Retry-After); "
                    "files stay pending",
                    _pause_until - time.time(),
                )
            outbox_pending_files.set(outbox.pending_count())
            with _progress_lock:
                _last_progress = time.time()
            _shutdown_requested.wait(5.0)
            continue
        elif _was_507_paused:
            _was_507_paused = False
            logging.warning("[OUTBOX-WORKER] 507 pause over - delivery resumed")

        # 2908: live pending-count gauge (outbox worker view).
        outbox_pending_files.set(outbox.pending_count())

        # periodic stuck-item reaper -- every 60s, reset in_progress
        # items stuck too long (crashed worker, hung HTTP) or max-retry blobs
        # still on disk so they can be retried.
        _now = time.time()
        if _now - _last_stuck_check >= STUCK_ITEM_CHECK_INTERVAL:
            _last_stuck_check = _now
            outbox.recover_stuck()

        result = _dequeue_safe()

        if result[0] is None:
            # Nothing ready now. If a study tail is buffered, wait up to
            # batch_idle_sec for a same-study item; if none arrives, flush.
            if _batch:
                _shutdown_requested.wait(timeout=batch_idle_sec)
                with _progress_lock:
                    _last_progress = time.time()
                result = _dequeue_safe()
                if result[0] is None:
                    _deliver_batch()
                    continue
            else:
                # P8 + retry: sleep until next_retry_at (bounded by 5s).
                wait = outbox.next_retry_wait_sec()
                _shutdown_requested.wait(timeout=wait)
                # P30/P1-4: watchdog heartbeat -- worker is alive and waiting.
                with _progress_lock:
                    _last_progress = time.time()
                with _progress_lock:
                    dead_sec = time.time() - _last_progress
                if dead_sec > 120 and outbox.pending_count() > 0:
                    logging.warning(
                        "[OUTBOX-WATCHDOG] Worker stalled %.0fs with %d pending - "
                        "running recovery",
                        dead_sec,
                        outbox.pending_count(),
                    )
                    reset_p, reset_r = outbox.recover_stuck()
                    if reset_p > 0 or reset_r > 0:
                        logging.warning(
                            "[OUTBOX-WATCHDOG] Recovery done: %d stuck in_progress, "
                            "%d moved to undelivered",
                            reset_p,
                            reset_r,
                        )
                    with _progress_lock:
                        _last_progress = time.time()
                        _last_stuck_check = time.time()
                continue

        sop_uid, ds, blob_path, raw_bytes = result
        # ds is already a pydicom Dataset from dequeue() -- no dcmread needed.

        # Study context for batching + INFO-level delivery logging.
        try:
            _study_uid = str(getattr(ds, "StudyInstanceUID", ""))
        except Exception:
            _study_uid = ""

        # Study boundary HARD: never mix studies in one batch. Switch flushes.
        if _batch and _study_uid != _batch_study:
            _deliver_batch()

        if _study_uid and _study_uid != _current_study:
            _current_study = _study_uid
            _log_study_start(sop_uid, ds, _study_uid)
            mods_display = (
                ", ".join(sorted(_study_stats[_study_uid].get("modalities", set())))
                or "??"
            )
            logging.info(
                "[OUTBOX] Delivering: study=%s patient=%s modality=%s - %d SOP(s) pending",
                _study_uid[:50],
                _study_stats[_study_uid]["patient"],
                mods_display,
                outbox.pending_count() + 1,
            )

        # apply deferred audit trail (off hot-path). C-STORE handler
        # generated PatientID/Name but skipped (0400,0561); now we apply
        # record_modification() from the .mods sidecar written by enqueue().
        mods_json = outbox.read_mods_sidecar(blob_path)
        dataset_modified = False
        if mods_json:
            from clean_code.validation import record_modification

            try:
                for entry in mods_json:
                    tag = tuple(entry["tag"])
                    record_modification(
                        ds,
                        tag,
                        entry["old"],
                        modifying_operator="DIMSE",
                        modifying_system=bridge_ae_title,
                    )
                dataset_modified = True
                logging.debug(
                    "[OUTBOX-WORKER] Applied %d audit trail entries for SOP %s",
                    len(mods_json),
                    sop_uid,
                )
            except Exception as e:
                logging.warning(
                    "[OUTBOX-WORKER] Failed to apply audit trail for SOP %s: %s",
                    sop_uid,
                    e,
                )
                # Continue delivery -- blob is valid even without audit trail

        # P8: raw pass-through for CAS dedup. Modified instances (raw=None)
        # are re-serialized by stow_batch_instances via dcmwrite.
        _batch.append(
            (
                sop_uid,
                ds,
                blob_path,
                (None if dataset_modified else raw_bytes),
                dataset_modified,
            )
        )
        try:
            _batch_bytes += os.path.getsize(blob_path)  # on-disk size = payload proxy
        except OSError:
            _batch_bytes += len(raw_bytes) if raw_bytes else 0
        _batch_study = _study_uid

        # Flush when count or bytes trigger fires.
        if len(_batch) >= batch_max_sop or _batch_bytes >= batch_max_bytes:
            _deliver_batch()

    if _batch:
        _deliver_batch()  # shutdown: drain any remaining tail first
    _http.close()  # P30: clean up persistent HTTP client
    logging.info("[OUTBOX-WORKER] Worker stopped")


def _start_outbox_workers(
    outbox,
    clarus_url,
    http_timeout,
    sop_index,
    move_jobs,
    batch_max_sop=100,
    batch_max_bytes=96 * 1024 * 1024,
    batch_idle_sec=5.0,
    health_checker=None,
    tls=None,
    bridge_ae_title="CLARUS_BRIDGE",
):
    """Start single background outbox worker thread. KISS."""
    t = threading.Thread(
        target=_outbox_worker,
        args=(
            outbox,
            clarus_url,
            http_timeout,
            sop_index,
            move_jobs,
            batch_max_sop,
            batch_max_bytes,
            batch_idle_sec,
            health_checker,
            tls,
            bridge_ae_title,
        ),
        name="outbox-worker",
        daemon=False,
    )
    t.start()
    logging.info("[OUTBOX-WORKER] Started 1 worker")
    return [t]


def _drain_outbox(outbox):
    """Wait for outbox to empty, then close (graceful shutdown)."""
    pending = outbox.pending_count()
    if pending > 0:
        logging.info("[OUTBOX] Waiting for %d pending SOPs to be delivered...", pending)
        # Poll until empty or shutdown
        while outbox.pending_count() > 0 and not _shutdown_requested.is_set():
            _shutdown_requested.wait(timeout=0.5)
        remaining = outbox.pending_count()
        if remaining > 0:
            logging.warning(
                "[OUTBOX] %d SOPs remain undelivered - will survive restart",
                remaining,
            )
    outbox.close()
    logging.info("[OUTBOX] Closed")


# ---------------------------------------------------------------------------
# Forwarder: _forward_worker - best-effort DIMSE drain of the
# SECOND (non-authoritative) outbox into the second DIMSE server.
#
# The forward channel is fully isolated from the primary STOW drain: it holds
# its own OutboxQueue instance (clean_code/outbox.py, the same class), runs on
# its own thread, and pauses rather than burning retries when the second server
# is unreachable. Whatever happens here NEVER backpressures primary reception.
#
# Reuses the byte-level meta helpers from handlers/cmove.py (import only, no
# edits) to read each stored blob's (sop_class, transfer_syntax) without a full
# pydicom parse. The blob is sent as stored bytes (its own Transfer Syntax).
# ---------------------------------------------------------------------------

# Forward batching: up to 20 SOPs per association (a widely interoperable
# ceiling), drained over a short 0.5 s window to fill each association, then a
# 5 s pause before retrying after an association failure so a dead peer is not
# hammered.
_FORWARD_BATCH_MAX_SOP = 20
_FORWARD_BATCH_WINDOW_S = 0.5
_FORWARD_PAUSE_S = 5.0


def _forward_blob_pair(raw_bytes):
    """Read (sop_class_uid, stored_ts_uid) from a stored DICOM file's meta.

    Uses the byte-level helpers from handlers/cmove.py:805-987 (import, no
    edits). Reads (0002,0002) MediaStorageSOPClassUID and (0002,0010)
    TransferSyntaxUID straight from the preamble-aware meta group, so a
    forward blob is identified WITHOUT a pydicom dataset parse.
    """
    sop_class = _extract_meta_ui(raw_bytes, 0x0002)
    stored_ts = _extract_meta_ui(raw_bytes, 0x0010)
    if sop_class is None:
        # Meta MS SOP class absent - fall back to dataset (0008,0016).
        sop_class = _extract_dataset_ui(raw_bytes, 0x0016, group=0x0008)
    return sop_class, stored_ts


def _forward_blob_size(blob_path):
    """Size in bytes of a forward blob, 0 when it cannot be stat'ed.

    Metrics must never break the drain, so a vanished/renamed blob degrades
    to 0 rather than raising.
    """
    try:
        return os.path.getsize(blob_path)
    except OSError:
        return 0


def _forward_open_assoc(
    calling_ae, dest_ae, host, port, pairs, network_timeout, dimse_timeout, acse_timeout
):
    """Open ONE association carrying one requested context per distinct
    (sop_class, stored_ts) pair of the batch. Returns an
    established association or None on any failure."""
    ae = AE(ae_title=calling_ae)
    ae.network_timeout = network_timeout
    ae.acse_timeout = acse_timeout
    ae.dimse_timeout = dimse_timeout
    for sop_class, ts_uid in sorted(pairs):
        if sop_class and ts_uid:
            ae.add_requested_context(sop_class, ts_uid)
    try:
        assoc = ae.associate(host, port, ae_title=dest_ae)
    except Exception:
        return None
    if not assoc.is_established:
        try:
            assoc.release()
        except Exception:
            pass
        return None
    # Counted only once the association is actually established: a refused
    # association is not an association made.
    try:
        associations_total.labels(role="to", ae=ae_label(dest_ae)).inc()
    except Exception:
        pass
    return assoc


def _forward_process_batch(
    forward_outbox,
    items,
    dest_ae,
    calling_ae,
    host,
    port,
    network_timeout,
    dimse_timeout,
    acse_timeout,
):
    """Deliver ONE already-claimed batch of forward items over a single
    association.

    Args:
        forward_outbox: the forward OutboxQueue.
        items: list of claimed tuples (uid, sop_class, stored_ts, blob_path),
               each ALREADY atomically claimed into forward/in_progress via
               dequeue() (so mark_delivered/mark_failed/requeue know the mover).
        dest_ae/host/port/calling_ae/timeouts: association parameters.

    Returns True if the association was usable (even if individual RSPs were
    non-zero); False if the association could NOT be established at all, in
    which case the CALLER requeues the whole batch (no retry burn) and pauses.

    Per instance it READS every C-STORE-RSP: 0x0000 -> mark_delivered;
    a non-zero RSP -> mark_failed (backoff, then undelivered at max retries);
    a lost/None RSP or an unasendable context mismatch -> requeued without a
    retry burn (association transport issue, not a per-instance failure).

    Items are sent in deterministic uid order: dequeue() claims files in
    os.listdir order, which differs across filesystems (CI failure on Linux
    ext4 vs local NTFS) - sorting here makes delivery order stable and
    reproducible everywhere, and keeps the wire test's exact-order pin valid.
    """
    items = sorted(items, key=lambda item: item[0])
    pairs = sorted({(sop, ts) for (_, sop, ts, _) in items if sop and ts})
    assoc = _forward_open_assoc(
        calling_ae,
        dest_ae,
        host,
        port,
        pairs,
        network_timeout,
        dimse_timeout,
        acse_timeout,
    )
    if assoc is None:
        logging.warning(
            "[FORWARD] Cannot associate with %s at %s:%s - batch %d requeued, "
            "pause %.0fs (no retry burn)",
            dest_ae,
            host,
            port,
            len(items),
            _FORWARD_PAUSE_S,
        )
        return False

    try:
        # accepted = {sop_class: {ts,...}} from the negotiated contexts.
        accepted = {}
        for ctx in assoc.accepted_contexts:
            ts_list = [str(t) for t in (ctx.transfer_syntax or [])]
            accepted.setdefault(str(ctx.abstract_syntax), set()).update(ts_list)

        delivered_any = False
        for uid, sop_class, stored_ts, blob_path in items:
            # Only send when the negotiated context actually covers this
            # sop_class and stored TS; sending stored bytes as-is over a
            # foreign/absent TS would corrupt them. A peer that cannot accept
            # this SOP class / TS is a PEER CAPABILITY RESULT, so it burns a
            # retry (mark_failed -> at max retries -> undelivered), exactly like
            # a non-zero RSP. It is NOT a transient transport condition, so it
            # must NOT requeue-no-burn (which would otherwise spin forever).
            negotiated_ts = accepted.get(sop_class, set())
            if not sop_class or stored_ts not in negotiated_ts:
                logging.warning(
                    "[FORWARD] SOP %s %s@%s context not negotiated - "
                    "mark_failed (capability verdict, retry budget applied "
                    "then undelivered)",
                    uid,
                    sop_class,
                    stored_ts,
                )
                forward_failed_total.inc()
                try:
                    forward_outbox.mark_failed(uid, "context not covered")
                except Exception:
                    pass
                continue

            # Size is read BEFORE the send: mark_delivered removes the blob,
            # so measuring afterwards always yields 0.
            _blob_size = _forward_blob_size(blob_path)
            try:
                _t0 = time.perf_counter()
                status = assoc.send_c_store(blob_path)
                try:
                    cstore_outbound_duration.labels(peer=ae_label(dest_ae)).observe(
                        time.perf_counter() - _t0
                    )
                except Exception:
                    pass
            except Exception as fwd_err:
                logging.warning(
                    "[FORWARD] SOP %s C-STORE raised %s - requeued (no burn)",
                    uid,
                    type(fwd_err).__name__,
                )
                try:
                    forward_outbox.requeue(
                        uid, f"send exception: {type(fwd_err).__name__}"
                    )
                except Exception:
                    pass
                continue

            # READ every C-STORE-RSP.
            if status is not None and status.Status == 0x0000:
                forward_outbox.mark_delivered(uid)
                forward_delivered_total.inc()
                try:
                    bytes_total.labels(direction="tx", peer=ae_label(dest_ae)).inc(
                        _blob_size
                    )
                except Exception:
                    pass
                delivered_any = True
                logging.debug("[FORWARD] SOP %s delivered (0x0000)", uid)
            elif status is None:
                # Lost/aborted association transport - requeue, no retry burn.
                logging.warning(
                    "[FORWARD] SOP %s: no C-STORE-RSP read - requeued (no burn)",
                    uid,
                )
                try:
                    forward_outbox.requeue(uid, "no RSP")
                except Exception:
                    pass
            else:
                # A real non-zero RSP (0xC000/0xA7xx/etc) - a per-instance status.
                # mark_failed bumps the retry counter and (at MAX_RETRIES) calls
                # move_to_undelivered internally (outbox.py semantics).
                msg = str(getattr(status, "Status", status))
                forward_failed_total.inc()
                forward_outbox.mark_failed(uid, f"C-STORE {msg}")
                logging.warning("[FORWARD] SOP %s rejected: %s", uid, msg)
        if delivered_any:
            try:
                forward_last_success_timestamp.set(time.time())
            except Exception:
                pass
        return True
    finally:
        try:
            assoc.release()
        except Exception:
            pass


def _forward_worker(
    forward_outbox,
    dest_ae,
    calling_ae,
    host,
    port,
    network_timeout=30,
    dimse_timeout=30,
    acse_timeout=15,
):
    """Background thread loop: drain forward/pending into the second DIMSE
    server. Runs independently of _outbox_worker; the two queues/threads are
    isolated, so a slow/dead second server CANNOT stall the primary drain."""
    _last_stuck = 0.0
    while not _shutdown_requested.is_set():
        # Periodic stuck recovery (restart replay / crash recovery) - same
        # recover_stuck semantics as the primary outbox (outbox.py).
        now = time.time()
        if now - _last_stuck >= 60.0:
            _last_stuck = now
            try:
                forward_outbox.recover_stuck()
            except Exception:
                pass
        try:
            forward_pending_files.set(forward_outbox.pending_count())
        except Exception:
            pass

        # Drain one batch (up to _FORWARD_BATCH_MAX_SOP within a small window).
        items = []
        window_end = time.time() + _FORWARD_BATCH_WINDOW_S
        while not _shutdown_requested.is_set() and len(items) < _FORWARD_BATCH_MAX_SOP:
            try:
                res = forward_outbox.dequeue()
            except Exception:
                res = (None, None, None, None)
            uid, _ds, blob_path, raw_bytes = res
            if uid is None:
                break
            sop_class, stored_ts = _forward_blob_pair(raw_bytes)
            items.append((uid, sop_class, stored_ts, blob_path))
            if time.time() >= window_end:
                break

        if not items:
            # Empty / nothing ready - sleep until the next retry is due.
            try:
                wait = forward_outbox.next_retry_wait_sec()
            except Exception:
                wait = 1.0
            _shutdown_requested.wait(timeout=max(0.1, wait))
            continue

        assoc_ok = _forward_process_batch(
            forward_outbox,
            items,
            dest_ae,
            calling_ae,
            host,
            port,
            network_timeout,
            dimse_timeout,
            acse_timeout,
        )
        if not assoc_ok:
            # Association-level failure: requeue the WHOLE already-claimed
            # batch without burning retries, then pause (destination probe).
            for uid, _sop, _ts, _bp in items:
                try:
                    forward_outbox.requeue(uid, "association failed")
                except Exception:
                    pass
            _shutdown_requested.wait(timeout=_FORWARD_PAUSE_S)
        try:
            forward_pending_files.set(forward_outbox.pending_count())
        except Exception:
            pass


# ---------------------------------------------------------------------------
# _register_handlers - DIMSE event handler registration
# ---------------------------------------------------------------------------
def _register_handlers(
    args,
    sop_index,
    move_jobs,
    sop_registry,
    outbox,
    health_checker,
    commit_tracker=None,
    forward_outbox=None,
    expose_rejected=False,
):
    """Register DIMSE event handlers. Returns list of (event, handler) tuples.

    `expose_rejected` is bound to the AE this set is registered on: the Expose
    Rejected Instances AE builds its own set with the flag on, every other AE
    uses the default. It only affects the QIDO/WADO query the C-FIND and
    C-MOVE handlers issue - the DIMSE behaviour is identical.
    """
    # Bridge: verbose HTTP logging when DEBUG level is active
    verbose = args.log_level == "DEBUG"

    # Storage Commitment presence, read here rather than threaded in as a
    # parameter: it is consumed by on_n_action below, and the same one-line
    # formula already appears in _start_background_services (which selects the
    # readiness-gate mode). A parameter would add an argument for a single
    # read of a single config flag.
    stc_presence = bool(getattr(args, "storage_commitment_presence", False))

    # P1-3: C-STORE, C-FIND, C-MOVE share the httpx.Client for connection reuse.
    # C-MOVE uses HTTP WADO-RS via httpx.Client for instance retrieval.

    # C-STORE wrapper with pynetdicom error isolation.
    # 0xC001 (CannotUnderstand) is generated by pynetdicom BEFORE our handler
    # when it fails to decode the dataset or negotiate the presentation context.
    # This wrapper catches any Exception that manages to reach our handler,
    # logging the Transfer Syntax and SOP Class UID for root cause analysis.
    def on_c_store(event):
        try:
            return handle_c_store(
                event,
                args.clarus_url,
                verbose,
                sop_index,
                max_store_bytes=(
                    args.max_store_mb * 1024 * 1024 if args.max_store_mb > 0 else 0
                ),
                http_timeout=args.http_timeout,
                sop_registry=sop_registry,
                outbox=outbox,
                health_checker=health_checker,
                store_forward=args.outbox_store_forward,
                fix_empty_patient_id=args.fix_empty_patient_id,
                fix_empty_patient_name=args.fix_empty_patient_name,
                bridge_id_prefix=args.bridge_id_prefix,
                bridge_name_prefix=args.bridge_name_prefix,
                tls=args.tls_pair,
                forward_outbox=forward_outbox,
                bridge_ae_title=args.ae_title,
            )
        except Exception:
            # Catch ALL exceptions to prevent pynetdicom association abort.
            # Log enough context to identify root cause (Transfer Syntax, SOP Class,
            # presentation context) without relying on event.dataset (may be None).
            ctx_uid = "?"
            ts_uid = "?"
            sop_uid = "?"
            try:
                ctx_uid = str(event.context.abstract_syntax) if event.context else "?"
            except Exception:
                pass
            try:
                ts_uid = str(event.context.transfer_syntax) if event.context else "?"
            except Exception:
                pass
            try:
                sop_uid = str(getattr(event.dataset, "SOPInstanceUID", "?"))
            except Exception:
                pass
            logging.error(
                "[BUG-005] C-STORE handler crashed - "
                "AbstractSyntax=%s TransferSyntax=%s SOP=%s",
                ctx_uid,
                ts_uid,
                sop_uid,
            )
            logging.debug(
                "[BUG-005] Full traceback follows:",
                exc_info=True,
            )
            return 0xC000  # Processing Failure - association survives

    def on_c_echo(event):
        return handle_c_echo(event)

    def on_c_find(event):
        yield from handle_c_find(
            event,
            args.clarus_url,
            verbose,
            args.c_find_max_responses,
            http_timeout=args.http_timeout,
            tls=args.tls_pair,
            expose_rejected=expose_rejected,
            qa_url=args.qa_url,
        )

    def on_c_move(event):
        yield from handle_c_move(
            event,
            move_jobs,
            args.peers_file,
            args.clarus_url,
            bridge_ae_title=args.ae_title,
            http_timeout=args.http_timeout,
            sop_registry=sop_registry,
            storage_commit_after_move=not args.no_storage_commit,
            sop_index=sop_index,
            outbound_network_timeout=args.outbound_network_timeout,
            outbound_dimse_timeout=args.outbound_dimse_timeout,
            outbound_acse_timeout=args.outbound_acse_timeout,
            bridge_dimse_host=args.bridge_host,
            bridge_dimse_port=args.bridge_port,
            move_poll_timeout=args.move_poll_timeout,
            tls=args.tls_pair,
            expose_rejected=expose_rejected,
            qa_url=args.qa_url,
        )

    def on_n_action(event):
        yield from handle_n_action(
            event,
            args.clarus_url,
            verbose,
            sop_index,
            http_timeout=args.http_timeout,
            commit_tracker=commit_tracker,
            resolve_scp=lambda ae_title, client_ip: move_jobs.lookup_scp(
                ae_title, client_ip=client_ip
            ),
            tls=args.tls_pair,
            stc_presence=stc_presence,
        )

    # DIMSE association lifecycle handlers.
    # Log association open/close at INFO - admin sees who connected and what they sent.
    def on_assoc_accepted(event):
        # Use id(event.assoc) as unique key - same as C-STORE handler.
        # Python object ID is unique per DICOM connection, available before negotiation.
        ae_title = "?"
        address = "?"
        assoc_id = 0
        try:
            assoc = event.assoc
            assoc_id = id(assoc)
            req = getattr(assoc, "requestor", None)
            if req is not None:
                ae = getattr(req, "ae_title", None)
                if ae and str(ae).strip():
                    ae_title = str(ae).strip()
                addr = getattr(req, "address", None)
                if addr and str(addr).strip():
                    address = str(addr).strip()
        except Exception as e:
            logging.debug("[DIMSE] Failed to extract AE/address: %s", e)
        register_assoc_accepted(assoc_id, ae_title, address)
        return 0x0000  # Always accept

    def on_assoc_released(event):
        assoc_id = 0
        try:
            assoc_id = id(event.assoc)
        except Exception:
            pass
        register_assoc_released(assoc_id)

    handlers = [
        (evt.EVT_C_STORE, on_c_store),
        (evt.EVT_C_ECHO, on_c_echo),
        (evt.EVT_C_FIND, on_c_find),
        (evt.EVT_C_MOVE, on_c_move),
        (evt.EVT_N_ACTION, on_n_action),
        (evt.EVT_ACCEPTED, on_assoc_accepted),
        (evt.EVT_RELEASED, on_assoc_released),
        (evt.EVT_ABORTED, on_assoc_released),  # Same cleanup as release
    ]

    return handlers


# ---------------------------------------------------------------------------
# Main entry point
# ---------------------------------------------------------------------------
def main():
    # - Windows: disable Quick Edit mode (console Mark mode) -
    # On Windows, clicking in a console window pauses ALL output
    # (ENABLE_QUICK_EDIT_MODE). PyInstaller .exe has no Properties dialog
    # to disable it manually, so we do it programmatically at startup.
    # Without this, a single accidental click freezes the bridge.
    if sys.platform == "win32" and sys.stdout is not None:
        try:
            kernel32 = ctypes.windll.kernel32
            ENABLE_QUICK_EDIT_MODE = 0x0040
            ENABLE_EXTENDED_FLAGS = 0x0080
            STD_INPUT_HANDLE = -10
            handle = kernel32.GetStdHandle(STD_INPUT_HANDLE)
            mode = ctypes.c_uint32()
            if kernel32.GetConsoleMode(handle, ctypes.byref(mode)):
                new_mode = (
                    mode.value & ~ENABLE_QUICK_EDIT_MODE
                ) | ENABLE_EXTENDED_FLAGS
                kernel32.SetConsoleMode(handle, new_mode)
        except Exception:
            pass  # Not a console (redirected output, no-pty, etc.)

    cli = _parse_args()

    # CONFIG-DISCIPLINE: config file is the sole source of settings.
    if not os.path.exists(cli.config):
        logging.basicConfig(level=logging.INFO)
        logging.error("Config file not found: %s", cli.config)
        sys.exit(1)

    cfg = ConfParser(cli.config)

    # Validate before populating - fail fast on bad config.
    try:
        _validate_config(cfg)
    except ValueError as e:
        logging.basicConfig(level=logging.INFO)
        logging.error("%s", e)
        logging.debug("Config validation traceback:", exc_info=True)
        sys.exit(1)

    args = _populate_args(cfg)

    # - Resolve relative paths against config file directory -
    # Same as nginx: all relative paths in config are relative to the
    # config file, NOT the CWD. Makes the entire folder portable.
    _config_dir = os.path.dirname(os.path.abspath(cli.config))
    # peers_file and printers_yaml are read by the C-MOVE peer lookup and the
    # print registry; resolved from the CWD they point nowhere under a service
    # manager, and both lookups then silently return nothing.
    _path_keys = [
        "log_file",
        "sop_registry",
        "sop_index_dir",
        "outbox_dir",
        "peers_file",
        "printers_yaml",
    ]
    for key in _path_keys:
        val = getattr(args, key, None)
        if val and not os.path.isabs(val):
            setattr(args, key, os.path.join(_config_dir, val))

    # The patient-id masking key lives beside the outbox: the same directory
    # the data it labels is written to, so moving the outbox moves the
    # correlation with it. Set before the first log line that masks an id.
    set_mask_key_dir(os.path.join(args.outbox_dir, "forward"))

    # Bridge: initialize logging before any output
    _setup_logging(args.log_level, args.log_file)

    # Startup WARN: if worst-case association retry time (attempts x
    # outbound_acse_timeout) reaches into the C-MOVE poll budget, the reactor
    # could hit 0xB000 before a job finishes its association attempts. The
    # standard (PS3.8 9.1.4 Note 2) gives no retry-count guidance, so surface
    # the risk at config load - not at 0xB000 time.
    _retry_total = args.outbound_assoc_attempts * args.outbound_acse_timeout
    if args.move_poll_timeout and _retry_total >= args.move_poll_timeout:
        logging.warning(
            "[CONFIG-WARN] dimse.outbound_assoc_attempts=%d x "
            "outbound_acse_timeout=%d = %ds reaches into "
            "move_poll_timeout=%ds - a wedged target could exhaust the "
            "C-MOVE poll budget across association retries (0xB000 before "
            "delivery). Tune the attempts/timeouts down if this bites.",
            args.outbound_assoc_attempts,
            args.outbound_acse_timeout,
            _retry_total,
            args.move_poll_timeout,
        )

    # Bridge: load SOP class registry
    sop_registry = _load_sop_registry(args.sop_registry)

    # - Duplicate-instance and port checks BEFORE any component starts -
    # `_init_components` calls `move_jobs.recover_in_progress_jobs()`, which
    # resets every in-progress C-MOVE job in the shared database back to
    # pending. A second instance would therefore steal the live first
    # instance's running jobs and only afterwards discover that the ports are
    # taken - the first instance's workers then re-claim the same jobs and
    # both C-STORE them. Refusing before that write is what keeps the guard
    # free of side effects.
    _check_port_available(args.http_port, args.http_print_bind, "metrics HTTP")
    _check_port_available(args.bridge_port, args.bridge_host, "DIMSE")
    _check_dimse_unique(args.bridge_host, args.bridge_port, args.ae_title)

    # PID file: `/var/run/clbridge-{port}.pid` on Linux, next to the script
    # elsewhere, falling back to the script directory when the system
    # location is not writable. Never fatal on a write error - see
    # `_acquire_pid_file`. Named by DIMSE port so multiple bridge instances
    # on the same machine don't clash.
    _pid_path = _acquire_pid_file(args.bridge_port)
    if _pid_path:
        set_pid_path(_pid_path)

    # initialize components
    sop_index, move_jobs, printer_registry, driver_registry = _init_components(args)

    # - Startup summary (INFO: key params; DEBUG: tuning) -
    logging.info("clbridge v%s", VERSION)
    logging.info("[CONFIG] file: %s", cli.config)
    logging.info(
        "[DIMSE] %s:%d AE=%s", args.bridge_host, args.bridge_port, args.ae_title
    )
    logging.info("[DICOMWEB] %s", args.clarus_url)
    logging.info("[OUTBOX] dir=%s cleanup=%s", args.outbox_dir, args.outbox_cleanup)
    # The patient-id masking key is a SECRET that lives on this host's file
    # system: an operator who does not know it exists cannot back it up, and
    # restoring an outbox without it silently breaks the correlation between
    # old and new log tokens. Named here, at startup, next to the directory
    # it sits in.
    logging.info(
        "[MASK] patient-id key: %s",
        os.path.join(args.outbox_dir, "forward", ".patient_id_mask.key"),
    )
    logging.info("[SOP-INDEX] dir=%s", args.sop_index_dir)
    logging.info("[METRICS] %s:%d", args.http_print_bind, args.http_port)
    logging.info(
        "[COERCION] fix_patient_id=%s fix_patient_name=%s id_prefix=%s",
        args.fix_empty_patient_id,
        args.fix_empty_patient_name,
        args.bridge_id_prefix,
    )
    # - Tuning params (DEBUG only) -
    logging.debug(
        "[TUNING] c_find_max=%d c_move_workers=%d http_timeout=%ds "
        "stow_queue_size=%d max_assoc=%d",
        args.c_find_max_responses,
        args.c_move_workers,
        args.http_timeout,
        args.stow_queue_size,
        args.max_associations,
    )
    mx_cstore = getattr(args, "max_concurrent_cstore", None)
    logging.debug(
        "[TUNING] max_concurrent_cstore=%s",
        str(mx_cstore) if mx_cstore else "unlimited",
    )
    # - SOP registry info (DEBUG) -
    logging.debug("SOP registry: %s (%d classes)", args.sop_registry, len(sop_registry))
    mx_store = f"{args.max_store_mb} MB" if args.max_store_mb > 0 else "unlimited"
    logging.debug("C-STORE size limit: %s", mx_store)

    # start background services (HTTP server, workers, cleanup)
    (
        http_server,
        http_thread,
        cmove_thread,
        cleanup_thread,
        sop_cleanup_thread,
        outbox,
        outbox_workers,
        health_checker,
        commit_tracker,
        commit_thread,
        forward_outbox,
        forward_worker_thread,
    ) = _start_background_services(
        args,
        driver_registry,
        printer_registry,
        move_jobs,
        sop_index,
        sop_registry,
    )

    ae = AE(ae_title=args.ae_title)
    # P1 (2608): answer associations ONLY for our own AE title. pynetdicom
    # default is False - any Called AE Title would be accepted, which let a
    # misconfigured peers.conf loop C-MOVE back into the bridge's own SCP.
    ae.require_called_aet = True
    # DIMSE timeouts - from config [dimse] (inbound - outbound).
    # Inbound: bridge as SCP - C-STORE from modality, C-FIND from viewer.
    # inbound_network_timeout: 0 = disabled (SCP should wait for client, ).
    # None tells pynetdicom "never kill idle associations" - viewers hold
    # associations open during C-MOVE (PS3.4 C.4.2.2), sometimes for minutes.
    ae.network_timeout = (
        args.inbound_network_timeout if args.inbound_network_timeout > 0 else None
    )
    ae.acse_timeout = args.inbound_acse_timeout
    ae.dimse_timeout = args.inbound_dimse_timeout
    # [dimse] max_associations: explicit inbound association cap.
    ae.maximum_associations = args.max_associations
    nw = ae.network_timeout
    logging.debug(
        "[DIMSE] Inbound: network=%s acse=%ds dimse=%ds",
        nw if nw is not None else "off",
        ae.acse_timeout,
        ae.dimse_timeout,
    )
    # Bridge: register supported SOP classes from YAML registry
    accepted_uids = _get_sop_registry_uids(sop_registry)

    def _register_contexts(target_ae):
        """Add every SOP class and query/retrieve context to `target_ae`.

        Shared by the ordinary AE and the Expose Rejected Instances AE: the
        second AE exposes the SAME objects with a different QIDO view, so a
        differing context set would make it answer a strictly narrower set
        of requests for no reason.
        """
        # EVRLE is proposed FIRST for image SOP classes (see the module-level
        # IMAGE_TS constant in clean_code.protocol): a native-only viewer
        # accepts the (DX, 4.70) fallback association only with EVRLE first.
        # Negotiation is acceptor-driven, so a 4.80-capable viewer still
        # negotiates 4.80 and a stored 4.80 instance sends as-is. JPEG
        # Lossless 4.57 and Baseline 4.50 are dropped from the proposal -
        # never stored, never produced, and no viewer ever selected them.
        # Non-image SOPs (SR, PR, RTSTRUCT) get uncompressed-only lists.
        _NON_IMAGE_PFX = (
            "1.2.840.10008.5.1.4.1.1.88.",
            "1.2.840.10008.5.1.4.1.1.11.",
            "1.2.840.10008.5.1.4.1.1.104.",
            "1.2.840.10008.5.1.4.1.1.66.",
        )
        _NON_IMAGE_EXACT = {
            "1.2.840.10008.5.1.4.1.1.481.3",
            "1.2.840.10008.5.1.4.1.1.481.4",
            "1.2.840.10008.5.1.4.1.1.481.5",
            "1.2.840.10008.5.1.4.1.1.481.6",
            "1.2.840.10008.5.1.4.1.1.481.7",
            "1.2.840.10008.5.1.4.1.1.481.8",
            "1.2.840.10008.5.1.4.1.1.481.9",
        }
        for sop_uid in accepted_uids:
            entry = (
                sop_registry.get(sop_uid, {}) if isinstance(sop_registry, dict) else {}
            )
            non_image = (
                bool(entry.get("non_image")) if isinstance(entry, dict) else False
            )
            if (
                non_image
                or sop_uid in _NON_IMAGE_EXACT
                or sop_uid.startswith(_NON_IMAGE_PFX)
            ):
                ts_list = NON_IMAGE_TS
            else:
                ts_list = IMAGE_TS
            # The SCP side (incoming C-STORE from viewers and modalities)
            # accepts the broad IMAGE_TS_INCOMING set - local files arrive in
            # their original TS, and a sender whose context is rejected
            # reports a failed export. The SCU proposal keeps IMAGE_TS.
            target_ae.add_supported_context(
                sop_uid, IMAGE_TS_INCOMING if ts_list is IMAGE_TS else NON_IMAGE_TS
            )
            target_ae.add_requested_context(sop_uid, ts_list)
        target_ae.add_supported_context(
            PatientRootQueryRetrieveInformationModelFind, ImplicitVRLittleEndian
        )
        target_ae.add_supported_context(
            StudyRootQueryRetrieveInformationModelFind, ImplicitVRLittleEndian
        )
        target_ae.add_supported_context(
            ModalityWorklistInformationFind, ImplicitVRLittleEndian
        )
        target_ae.add_supported_context(
            StorageCommitmentPushModel, ImplicitVRLittleEndian
        )
        target_ae.add_supported_context(
            StudyRootQueryRetrieveInformationModelMove, ImplicitVRLittleEndian
        )
        target_ae.add_supported_context(
            PatientRootQueryRetrieveInformationModelMove, ImplicitVRLittleEndian
        )

    _register_contexts(ae)
    logging.info(
        "Registered %d SOP classes for C-STORE (from registry)", len(accepted_uids)
    )

    # register handlers
    handlers = _register_handlers(
        args,
        sop_index,
        move_jobs,
        sop_registry,
        outbox,
        health_checker,
        commit_tracker=commit_tracker,
        forward_outbox=forward_outbox,
    )

    # QA AE (AIRA 57.2.1 Expose Rejected Instances). A DIMSE association has
    # no per-query "show rejected" flag, so a designated AE Title is the
    # standard mechanism, and its queries go to a separately configured QA
    # webservice endpoint. It runs on its OWN port with its OWN handler set:
    # the endpoint choice is a property of the AE that accepted the
    # association, so the ordinary AE cannot inherit it.
    #
    # BOTH keys are required. A port without a URL would accept associations
    # and then query the ordinary endpoint, silently returning the hidden
    # view to a QA user; a URL without a port is dead config. Either half
    # alone is a configuration mistake, so it is reported and the second AE
    # is not started.
    expose_ae = None
    expose_handlers = None
    if args.expose_port and args.qa_url:
        expose_ae = AE(ae_title=args.expose_ae_title)
        expose_ae.require_called_aet = True
        expose_ae.network_timeout = ae.network_timeout
        expose_ae.acse_timeout = ae.acse_timeout
        expose_ae.dimse_timeout = ae.dimse_timeout
        expose_ae.maximum_associations = args.max_associations
        _register_contexts(expose_ae)
        expose_handlers = _register_handlers(
            args,
            sop_index,
            move_jobs,
            sop_registry,
            outbox,
            health_checker,
            commit_tracker=commit_tracker,
            forward_outbox=forward_outbox,
            expose_rejected=True,
        )
    elif args.expose_port:
        logging.warning(
            "[DIMSE] [server] expose_port is set but [dicomweb] qa_url is "
            "empty - the QA AE is NOT started; the bridge stays single-AE. "
            "Set qa_url to the backend QA webservice endpoint."
        )
    elif args.qa_url:
        logging.warning(
            "[DIMSE] [dicomweb] qa_url is set but [server] expose_port is "
            "empty - the QA AE is NOT started; the bridge stays single-AE. "
            "Set expose_port to the port the QA workstation will connect to."
        )

    # - Shutdown Monitor Thread -
    # Bridges the async-signal-safe shutdown flag to pynetdicom's event loop.
    # On Windows, select() does NOT wake up on signals (no EINTR).
    # The monitor waits for _shutdown_requested in a regular thread and calls
    # ae.shutdown() - which closes the listening socket - select() wakes up
    # - start_server() returns - main thread proceeds with graceful shutdown.
    shutdown_monitor = threading.Thread(
        target=lambda: (_shutdown_requested.wait(), ae.shutdown()),
        name="shutdown-monitor",
        daemon=True,
    )
    shutdown_monitor.start()

    if expose_ae is not None:
        # Both AEs answer from their own listener thread. The expose server is
        # non-blocking so the main thread can keep serving the ordinary AE;
        # the shutdown monitor above still governs the process.
        logging.info(
            "[DIMSE] Expose Rejected Instances AE '%s' on %s:%d",
            args.expose_ae_title,
            args.bridge_host,
            args.expose_port,
        )

        def _serve_expose():
            try:
                expose_ae.start_server(
                    (args.bridge_host, args.expose_port),
                    block=True,
                    evt_handlers=expose_handlers,
                )
            except Exception as exc:
                logging.error("[DIMSE] Expose AE stopped: %s", exc)
                _shutdown_requested.set()

        threading.Thread(target=_serve_expose, name="dimse-expose", daemon=True).start()

    # Bridge: register signal handlers for graceful shutdown
    signal.signal(signal.SIGTERM, _signal_handler)
    signal.signal(signal.SIGINT, _signal_handler)
    # Windows: Ctrl+Break - graceful shutdown (SIGBREAK)
    if hasattr(signal, "SIGBREAK"):
        signal.signal(signal.SIGBREAK, _signal_handler)

    logging.info("Waiting for DICOM connections...")

    try:
        # Bridge: start_server blocks, but signal handlers will trigger shutdown
        ae.start_server(
            (args.bridge_host, args.bridge_port), block=True, evt_handlers=handlers
        )
    except KeyboardInterrupt:
        # Bridge: Ctrl+C in terminal
        logging.info("KeyboardInterrupt received")
        _shutdown_requested.set()
    finally:
        # Bridge: graceful shutdown sequence
        logging.info("Shutting down DICOM server...")
        if not _shutdown_requested.is_set():
            _shutdown_requested.set()

        # Bridge: allow in-flight operations to complete (30s grace period)
        logging.info("Waiting up to 30s for in-flight operations to complete...")
        ae.shutdown()  # Idempotent - monitor may have already called it
        if expose_ae is not None:
            expose_ae.shutdown()
        logging.info("DICOM server stopped")

        # Bridge: stop HTTP server - before joining threads
        http_server.shutdown()
        logging.info("[HTTP] Bridge server stopped")

        # - stop the producers before draining their queues -
        # The outbox workers exit on `_shutdown_requested`, so joining them
        # FIRST lets each one push its tail batch. Draining and then closing
        # the queue while a worker was still alive made `mark_delivered` /
        # `mark_failed` run against a closed queue, which turned a delivered
        # instance into a logged error.
        _fwd = [forward_worker_thread] if forward_worker_thread is not None else []
        for t in list(outbox_workers) + _fwd:
            if t.is_alive():
                logging.info("[SHUTDOWN] Waiting for thread %s (up to 30s)...", t.name)
                t.join(timeout=30.0)
                if t.is_alive():
                    logging.warning("[SHUTDOWN] Thread %s did not stop in time", t.name)

        # Bridge: drain outbox (graceful shutdown), then close the queues.
        # The forward queue exists only with `[forward] enabled=true`; the
        # default leaves it None and an unguarded close aborts the whole
        # shutdown sequence on the way out.
        _drain_outbox(outbox)
        if forward_outbox is not None:
            forward_outbox.close()

        # Stop the async Storage Commitment resolver.
        commit_tracker.close()

        # - join the remaining background threads -
        # Join C-MOVE workers FIRST - they use move_jobs DB which must stay open.
        _all_threads = [
            cmove_thread,
            cleanup_thread,
            sop_cleanup_thread,
            http_thread,
            commit_thread,
        ]
        for t in _all_threads:
            if t.is_alive():
                logging.info("[SHUTDOWN] Waiting for thread %s (up to 30s)...", t.name)
                t.join(timeout=30.0)
                if t.is_alive():
                    logging.warning("[SHUTDOWN] Thread %s did not stop in time", t.name)

        # Bridge: cleanup resources AFTER threads are done
        sop_index.close()
        move_jobs.close()

        # clean up PID file in normal shutdown sequence
        # (NOT in signal handler - unsafe on Windows)
        if _pid_path and os.path.exists(_pid_path):
            try:
                os.unlink(_pid_path)
                logging.info("Removed PID file %s", _pid_path)
            except OSError:
                pass

        logging.info("Bridge shutdown complete")


if __name__ == "__main__":
    main()
