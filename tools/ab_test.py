# -*- coding: utf-8 -*-
# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

r"""ab_test.py - universal DIMSE / DICOMweb™ A/B bench tool (upload then read).

Modes:
  -d DIR   DIMSE:    C-STORE upload, then C-MOVE read to an in-process SCP.
                     Target: clbridge (127.0.0.1:8104, AE CLBRIDGE) or Orthanc
                     (e.g. -d DIR --port 4242 --aet ORTHANC).
  -w DIR   DICOMweb: STOW-RS multipart upload, then WADO-RS read.
  Trademark note: DICOM® is a registered trademark of NEMA; DICOMweb™ is a
trademark of NEMA. Clarus is not affiliated with or endorsed by NEMA.
                     Target: Clarus (127.0.0.1:8024, base /dicomweb).

DIR is scanned recursively for *.dcm. Not a dumb hammer: the tool ramps the
in-flight window (concurrent DIMSE associations / concurrent HTTP requests)
from 1 upward per --chunk-mb and locks it when throughput plateaus
(--max-window caps it). A rejected new association / refused HTTP marks the
wall (real backpressure) and stops ramping.

Examples:
  python ab_test.py -d D:\Clarus\tmp\corpus --port 8104
  python ab_test.py -d D:\Clarus\tmp\corpus --port 4242 --aet ORTHANC
  python ab_test.py -w D:\Clarus\tmp\corpus --port 8024

Bridge notes (read phase):
  - C-MOVE destinations must be listed in the bridge's peers.conf, e.g.
    "ABTEST_SCP  127.0.0.1:8204" (lazyload=true picks it up automatically).
  - The bridge executes study-level C-MOVE jobs; use --move-level STUDY.
  - Bridge C-STORE is store-and-forward: 0x0000 means "accepted to the
    outbox", not "committed to Clarus". Pass
    --drain-url http://127.0.0.1:8105/metrics so the read phase starts only
    after the outbox drained, otherwise the C-MOVE may see a not-yet-visible
    study and fail with 0xB000.

Orthanc notes (read phase):
  - Orthanc (Docker) reaches our SCP at --scp-host (host.docker.internal
    for Docker Desktop, not 127.0.0.1).
  - Orthanc needs DicomAlwaysAllowMove=true or the destination AE registered
    as a known modality, otherwise C-MOVE is rejected.
"""

import argparse
import collections
import os
import queue
import re
import shutil
import sys
import tempfile
import threading
import time
from dataclasses import dataclass

import requests
from pydicom import dcmread, Dataset
from pydicom.uid import ImplicitVRLittleEndian
from pynetdicom import AE, evt
from pynetdicom.sop_class import (
    StudyRootQueryRetrieveInformationModelMove,
    StorageCommitmentPushModel,
)


# ---------------------------------------------------------------------------
# Data model
# ---------------------------------------------------------------------------
@dataclass
class FileItem:
    path: str
    size: int
    study: str
    series: str
    sop: str          # SOP Instance UID
    ts: str
    sop_class: str    # SOP Class UID


def scan_dir(root):
    """Recursively collect *.dcm files (tags read without pixel data)."""
    items = []
    tags = ["StudyInstanceUID", "SeriesInstanceUID", "SOPInstanceUID", "SOPClassUID"]
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for fn in sorted(filenames):
            if not fn.lower().endswith(".dcm"):
                continue
            path = os.path.join(dirpath, fn)
            try:
                ds = dcmread(path, stop_before_pixels=True, specific_tags=tags)
            except Exception as e:
                print("scan skip %s: %s" % (path, e))
                continue
            ts = str(getattr(ds.file_meta, "TransferSyntaxUID", "") or "")
            items.append(FileItem(
                path=path,
                size=os.path.getsize(path),
                study=str(getattr(ds, "StudyInstanceUID", "") or ""),
                series=str(getattr(ds, "SeriesInstanceUID", "") or ""),
                sop=str(getattr(ds, "SOPInstanceUID", "") or ""),
                ts=ts,
                sop_class=str(getattr(ds, "SOPClassUID", "") or ""),
            ))
    return items


def fmt_mb(n):
    return "%.1f MB" % (n / 1e6)


# ---------------------------------------------------------------------------
# Throughput meter + adaptive window ramp
# ---------------------------------------------------------------------------
class Meter:
    """Accumulates bytes; snapshots (bytes, seconds) per chunk threshold."""

    def __init__(self, chunk_bytes):
        self._lock = threading.Lock()
        self._chunk_bytes = chunk_bytes
        self._t0 = time.monotonic()
        self._acc = 0.0
        self._chunks = collections.deque()
        self.total = 0.0
        self.count = 0

    def add(self, nbytes, count=1):
        with self._lock:
            self.total += nbytes
            self.count += count
            self._acc += nbytes
            if self._acc >= self._chunk_bytes:
                now = time.monotonic()
                self._chunks.append((self._acc, now - self._t0))
                self._t0 = now
                self._acc = 0.0

    def pop_chunk(self):
        with self._lock:
            return self._chunks.popleft() if self._chunks else None


class Ramp:
    """Linear window growth with plateau detection and wall marking."""

    def __init__(self, max_w, gain=0.03, plateau_ticks=2):
        self.w = 1
        self.max_w = max_w
        self.gain = gain
        self.plateau_ticks = plateau_ticks
        self.ticks = 0
        self.best = 0.0
        self.locked = False
        self.wall = None

    def on_chunk(self, rate):
        """Returns True if one more worker should be spawned."""
        if self.locked:
            return False
        if rate <= 0 or (self.best > 0 and rate < self.best * (1.0 + self.gain)):
            self.ticks += 1
        else:
            self.ticks = 0
            if rate > self.best:
                self.best = rate
        if self.ticks >= self.plateau_ticks:
            self.locked = True
            return False
        if self.w < self.max_w:
            self.w += 1
            return True
        return False

    def mark_wall(self, why):
        self.locked = True
        self.wall = why


class PhaseState:
    def __init__(self, total_items):
        self._lock = threading.Lock()
        self.total = total_items
        self.processed = 0
        self.ok = 0
        self.failed = 0
        self.meter = None

    def record(self, nbytes, ok, count=1):
        with self._lock:
            self.processed += count
            if ok:
                self.ok += count
            else:
                self.failed += count
        self.meter.add(nbytes, count)

    def all_done(self):
        with self._lock:
            return self.processed >= self.total


def run_pipeline(spawn, stop, state, ramp, meter, verbose, label):
    """Threaded pipeline: workers consume a queue, coordinator ramps the window."""
    workers = []
    spawn(workers)
    chunk_no = 0
    while not state.all_done():
        snap = meter.pop_chunk()
        if snap is not None:
            chunk_no += 1
            nbytes, dt = snap
            rate = nbytes / dt if dt > 0 else 0.0
            if verbose:
                print("  %-6s chunk %2d W=%2d  %8.1f MB in %6.1f s = %7.1f MB/s"
                      % (label, chunk_no, ramp.w, nbytes / 1e6, dt, rate / 1e6))
            if ramp.on_chunk(rate):
                spawn(workers)
        workers = [w for w in workers if w.is_alive()]
        if ramp.wall and not workers and not state.all_done():
            print("ABORT: all workers died (%s); %d/%d processed"
                  % (ramp.wall, state.processed, state.total))
            return False
        time.sleep(0.15)
    stop.set()
    for w in workers:
        w.join(5)
    return True


def report(phase, state, meter, ramp, t0):
    dt = time.monotonic() - t0
    rate = meter.total / dt / 1e6 if dt > 0 else 0.0
    wall = (" (wall: %s)" % ramp.wall) if ramp.wall else ""
    print("[%s] window=%d%s | items=%d ok=%d failed=%d | %s in %.1f s = %.1f MB/s"
          % (phase, ramp.w, wall, state.processed, state.ok, state.failed,
             fmt_mb(meter.total), dt, rate))


def wait_for_drain(cfg):
    """Wait until the bridge outbox is empty, then let the read phase start.

    cfg.drain_url: Prometheus /metrics URL (bridge, e.g. http://127.0.0.1:8105).
    Polls the clarus_bridge_outbox_pending gauge until it reads 0, then waits
    a few seconds for the outbox idle-flush tail. Falls back to a fixed
    cfg.read_delay sleep when the endpoint is unreachable or the gauge is
    absent (Orthanc and other synchronous SCPs need no drain at all).
    """
    if not cfg.drain_url:
        time.sleep(cfg.read_delay)
        return 0.0
    t0 = time.monotonic()
    deadline = t0 + 300.0
    fails = 0
    while time.monotonic() < deadline:
        try:
            r = requests.get(cfg.drain_url, timeout=5)
        except requests.RequestException:
            fails += 1
            if fails >= 3:
                print("drain: endpoint unreachable, fallback sleep %.0f s"
                      % cfg.read_delay)
                time.sleep(cfg.read_delay)
                return cfg.read_delay
            time.sleep(2.0)
            continue
        fails = 0
        if r.status_code == 200:
            m = re.search(r"^clarus_bridge_outbox_pending\s+([\d.]+)",
                          r.text, re.M)
            if m and float(m.group(1)) == 0.0:
                time.sleep(6.0)  # outbox idle-flush tail (batch_idle_sec <= 5)
                dt = time.monotonic() - t0
                print("drain: outbox_pending=0 after %.1f s" % dt)
                return dt
        time.sleep(2.0)
    print("drain: TIMEOUT after 300 s, continuing anyway")
    return 300.0


# ---------------------------------------------------------------------------
# DIMSE engine (pynetdicom, plain threads - verified with pynetdicom 3.0.4)
# ---------------------------------------------------------------------------
class DimseEngine:
    def __init__(self, cfg, recv_dir):
        self.cfg = cfg
        self.recv_dir = recv_dir
        self.ae = AE(ae_title=cfg.scu_aet)
        self.ae.acse_timeout = 15
        self.ae.dimse_timeout = 60
        self.ae.network_timeout = 300
        self._recv_lock = threading.Lock()
        self._recv_seq = 0
        self._hint_lock = threading.Lock()
        self._hint_printed = False
        self.received = 0.0
        self.received_files = 0
        self.nevent_received = 0
        self._read_meter = None

    def register_contexts(self, items):
        ctx = {}
        for it in items:
            ctx.setdefault(it.sop_class, set()).add(it.ts)
        for sop, tss in ctx.items():
            tsl = sorted(t for t in tss if t)
            self.ae.add_requested_context(sop, tsl)
            self.ae.add_supported_context(sop, tsl)
        self.ae.add_requested_context(
            StudyRootQueryRetrieveInformationModelMove, ImplicitVRLittleEndian)
        self.ae.add_supported_context(StorageCommitmentPushModel)

    # -- SCP side ----------------------------------------------------------
    def on_store(self, event):
        # Save the RAW received bytes, no pydicom decode. The bridge sends
        # bare datasets (no Part 10 preamble); pydicom 3.x dcmread() refuses
        # them without force=True, so event.dataset raises here and pynetdicom
        # answers the peer with 0xC211. Raw write is also faster - the bench
        # SCP only counts bytes.
        try:
            raw = getattr(event.request, "DataSet", None)
            data = raw.getvalue() if raw is not None else None
            if not data:
                return 0xA900
            with self._recv_lock:
                self._recv_seq += 1
                seq = self._recv_seq
            path = os.path.join(self.recv_dir, "m%06d.dcm" % seq)
            with open(path, "wb") as fh:
                fh.write(data)
            with self._recv_lock:
                self.received += len(data)
                self.received_files += 1
            if self._read_meter is not None:
                self._read_meter.add(len(data))
        except Exception:
            return 0xC211
        return 0x0000

    def on_n_action(self, event):
        # pynetdicom 3.x: an N-ACTION handler must return (status, action_reply).
        # Storage Commitment requests carry no Action Reply dataset -> None.
        ds = Dataset()
        ds.Status = 0x0000
        return ds, None

    def on_n_event_report(self, event):
        # pynetdicom 3.x: an N-EVENT-REPORT handler must return
        # (status, dataset). This closes the Storage Commitment cycle the
        # bridge opens after each C-MOVE (N-ACTION -> N-EVENT-REPORT).
        with self._recv_lock:
            self.nevent_received += 1
        return 0x0000, None

    def start_scp(self, port):
        self.ae.start_server(
            ("0.0.0.0", port),
            block=False,
            evt_handlers=[
                (evt.EVT_C_STORE, self.on_store),
                (evt.EVT_N_ACTION, self.on_n_action),
                (evt.EVT_N_EVENT_REPORT, self.on_n_event_report),
            ],
        )

    def shutdown(self):
        try:
            self.ae.shutdown()
        except Exception:
            pass

    # -- SCU side ----------------------------------------------------------
    def new_assoc(self):
        try:
            a = self.ae.associate(self.cfg.host, self.cfg.port,
                                  ae_title=self.cfg.aet)
            if a.is_established:
                return a
        except Exception:
            pass
        return None

    @staticmethod
    def close_quiet(assoc):
        try:
            assoc.release()
        except Exception:
            pass

    def upload(self, items, chunk_mb, max_w, verbose):
        q = queue.Queue()
        for it in items:
            q.put(it)
        state = PhaseState(len(items))
        state.meter = Meter(chunk_mb * 1e6)
        ramp = Ramp(max_w)
        stop = threading.Event()
        engine = self

        def worker():
            assoc = None
            while not stop.is_set():
                try:
                    item = q.get(timeout=0.3)
                except queue.Empty:
                    continue
                if assoc is None:
                    assoc = engine.new_assoc()
                    if assoc is None:
                        ramp.mark_wall("A-ASSOCIATE failed or rejected")
                        q.put(item)
                        return
                ok = False
                try:
                    status = None
                    for _ in range(4):
                        status = assoc.send_c_store(item.path)
                        if status is None or status.Status != 0xA700:
                            break
                        # 0xA700 Out of Resources: legal transient rejection
                        # (PS3.7 C.4) - the bridge answers it while Clarus is
                        # unreachable (health poller cycle is 5 s). Retry with
                        # 1.5 s gaps to cover one full poll cycle.
                        time.sleep(1.5)
                    ok = status is not None and status.Status == 0x0000
                except Exception:
                    engine.close_quiet(assoc)
                    assoc = None
                state.record(item.size, ok)
            if assoc is not None:
                engine.close_quiet(assoc)

        def spawn(workers):
            t = threading.Thread(target=worker, daemon=True)
            t.start()
            workers.append(t)

        ok = run_pipeline(spawn, stop, state, ramp, state.meter, verbose, "upload")
        return state, state.meter, ramp, ok

    def move_identifier(self, mv, level):
        ds = Dataset()
        ds.QueryRetrieveLevel = level
        ds.StudyInstanceUID = mv[0]
        if level != "STUDY":
            ds.SeriesInstanceUID = mv[1]
        if level == "INSTANCE":
            ds.SOPInstanceUID = mv[2]
        return ds

    def read(self, units, level, chunk_mb, max_w, verbose):
        q = queue.Queue()
        for u in units:
            q.put(u)
        state = PhaseState(len(units))
        state.meter = Meter(chunk_mb * 1e6)
        meter = Meter(chunk_mb * 1e6)  # bytes counted by the SCP handler
        self._read_meter = meter
        ramp = Ramp(max_w)
        stop = threading.Event()
        engine = self
        scp_aet = self.cfg.scp_aet

        def worker():
            assoc = None
            while not stop.is_set():
                try:
                    mv = q.get(timeout=0.3)
                except queue.Empty:
                    continue
                if assoc is None:
                    assoc = engine.new_assoc()
                    if assoc is None:
                        ramp.mark_wall("A-ASSOCIATE failed or rejected")
                        q.put(mv)
                        return
                ok = False
                try:
                    final = None
                    for status, _ in assoc.send_c_move(
                        engine.move_identifier(mv, level), scp_aet,
                        StudyRootQueryRetrieveInformationModelMove,
                    ):
                        final = status
                    ok = final is not None and final.Status == 0x0000
                    if final is not None and final.Status == 0xA801:
                        with engine._hint_lock:
                            if not engine._hint_printed:
                                engine._hint_printed = True
                                print("  C-MOVE rejected 0xA801: add '%s  %s:%d' "
                                      "to the bridge peers.conf"
                                      % (scp_aet, engine.cfg.scp_host,
                                         engine.cfg.scp_port))
                except Exception:
                    engine.close_quiet(assoc)
                    assoc = None
                state.record(0, ok)
            if assoc is not None:
                engine.close_quiet(assoc)

        def spawn(workers):
            t = threading.Thread(target=worker, daemon=True)
            t.start()
            workers.append(t)

        ok = run_pipeline(spawn, stop, state, ramp, meter, verbose, "read")
        time.sleep(2.0)  # grace for trailing C-STORE writes into the SCP
        self._read_meter = None
        return state, meter, ramp, ok


# ---------------------------------------------------------------------------
# DICOMweb engine (requests, thread per worker)
# ---------------------------------------------------------------------------
class WebEngine:
    def __init__(self, cfg, recv_dir):
        self.cfg = cfg
        self.recv_dir = recv_dir
        self.base = "http://%s:%d%s" % (cfg.host, cfg.port, cfg.base.rstrip("/"))
        self.studies_url = self.base + "/studies"
        self._seq_lock = threading.Lock()
        self._seq = 0
        self._hint_lock = threading.Lock()
        self._hint_printed = False

    def batches(self, items, batch_size):
        by_study = {}
        for it in items:
            by_study.setdefault(it.study, []).append(it)
        out = []
        for lst in by_study.values():
            for i in range(0, len(lst), batch_size):
                out.append(lst[i:i + batch_size])
        return out

    def upload(self, batches, chunk_mb, max_w, verbose):
        q = queue.Queue()
        for b in batches:
            q.put(b)
        state = PhaseState(len(batches))
        state.meter = Meter(chunk_mb * 1e6)
        ramp = Ramp(max_w)
        stop = threading.Event()
        engine = self

        def worker():
            sess = requests.Session()
            while not stop.is_set():
                try:
                    batch = q.get(timeout=0.3)
                except queue.Empty:
                    continue
                fhs = []
                files = []
                nbytes = 0
                ok = False
                try:
                    for i, it in enumerate(batch):
                        fh = open(it.path, "rb")
                        fhs.append(fh)
                        files.append(("dicom", ("%04d.dcm" % i, fh,
                                                "application/dicom")))
                        nbytes += it.size
                    resp = sess.post(engine.studies_url, files=files,
                                     timeout=(15, 600))
                    ok = resp.status_code in (200, 201, 202, 204)
                    if not ok:
                        with engine._hint_lock:
                            if not engine._hint_printed:
                                engine._hint_printed = True
                                print("  HTTP %d on STOW: %s"
                                      % (resp.status_code,
                                         resp.text[:120].replace("\n", " ")))
                except Exception:
                    ok = False
                finally:
                    for fh in fhs:
                        try:
                            fh.close()
                        except Exception:
                            pass
                state.record(nbytes, ok)
            sess.close()

        def spawn(workers):
            t = threading.Thread(target=worker, daemon=True)
            t.start()
            workers.append(t)

        ok = run_pipeline(spawn, stop, state, ramp, state.meter, verbose, "upload")
        return state, state.meter, ramp, ok

    def read(self, units, chunk_mb, max_w, verbose):
        q = queue.Queue()
        for u in units:
            q.put(u)
        state = PhaseState(len(units))
        state.meter = Meter(chunk_mb * 1e6)
        ramp = Ramp(max_w)
        stop = threading.Event()
        engine = self

        def worker():
            sess = requests.Session()
            while not stop.is_set():
                try:
                    unit = q.get(timeout=0.3)
                except queue.Empty:
                    continue
                study, series = unit
                url = "%s/studies/%s/series/%s" % (engine.base, study, series)
                n = 0
                ok = False
                try:
                    with sess.get(url, stream=True, timeout=(15, 600)) as r:
                        if r.status_code == 200:
                            with engine._seq_lock:
                                engine._seq += 1
                                seq = engine._seq
                            path = os.path.join(engine.recv_dir,
                                                "w%06d.dcm" % seq)
                            with open(path, "wb") as fh:
                                for chunk in r.iter_content(1 << 20):
                                    if chunk:
                                        fh.write(chunk)
                                        n += len(chunk)
                            ok = n > 0
                except Exception:
                    ok = False
                state.record(n, ok)
            sess.close()

        def spawn(workers):
            t = threading.Thread(target=worker, daemon=True)
            t.start()
            workers.append(t)

        ok = run_pipeline(spawn, stop, state, ramp, state.meter, verbose, "read")
        return state, state.meter, ramp, ok


# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------
def main(argv=None):
    ap = argparse.ArgumentParser(
        description="DIMSE/DICOMweb A/B bench: upload then read, adaptive window.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__.split("Examples:")[-1],
    )
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("-d", dest="mode", action="store_const", const="dimse",
                   help="DIMSE: C-STORE upload + C-MOVE read")
    g.add_argument("-w", dest="mode", action="store_const", const="web",
                   help="DICOMweb: STOW-RS upload + WADO-RS read")
    ap.add_argument("dir", help="root directory scanned recursively for *.dcm")
    ap.add_argument("--host", default="127.0.0.1", help="target host (default 127.0.0.1)")
    ap.add_argument("--port", type=int, default=None,
                    help="target port (default: 8104 for -d, 8024 for -w)")
    ap.add_argument("--aet", default="CLBRIDGE",
                    help="-d: target AE title (default CLBRIDGE, Orthanc: ORTHANC)")
    ap.add_argument("--scu-aet", default="ABTEST_SCU", help="-d: our AE title")
    ap.add_argument("--scp-aet", default="ABTEST_SCP",
                    help="-d: C-MOVE destination AE title (our in-process SCP)")
    ap.add_argument("--scp-port", type=int, default=None,
                    help="-d: our SCP listen port (default target port + 100)")
    ap.add_argument("--scp-host", default="127.0.0.1",
                    help="-d: address the SERVER reaches our SCP at "
                         "(Docker Orthanc: host.docker.internal)")
    ap.add_argument("--base", default="/dicomweb", help="-w: URL prefix (default /dicomweb)")
    ap.add_argument("--move-level", choices=["STUDY", "SERIES", "INSTANCE"],
                    default="STUDY",
                    help="-d: C-MOVE level (default STUDY; bridge executes "
                         "study-level jobs regardless)")
    ap.add_argument("--stow-batch", type=int, default=20,
                    help="-w: instances per multipart POST, same-study batches (default 20)")
    ap.add_argument("--chunk-mb", type=float, default=32.0,
                    help="ramp decision chunk in MB (default 32)")
    ap.add_argument("--max-window", type=int, default=16,
                    help="max concurrent associations / HTTP requests (default 16)")
    ap.add_argument("--read-delay", type=float, default=5.0,
                    help="-d: seconds between upload and read when no drain "
                         "signal (default 5)")
    ap.add_argument("--drain-url", default=None,
                    help="-d: poll this /metrics URL until the bridge outbox "
                         "is empty before reading "
                         "(e.g. http://127.0.0.1:8105/metrics)")
    ap.add_argument("--only-upload", action="store_true")
    ap.add_argument("--only-read", action="store_true")
    ap.add_argument("--keep", action="store_true",
                    help="keep the temp dir with received files")
    ap.add_argument("-v", "--verbose", action="store_true",
                    help="per-chunk rate lines")
    args = ap.parse_args(argv)

    if args.only_upload and args.only_read:
        ap.error("--only-upload and --only-read are mutually exclusive")
    if args.chunk_mb < 1:
        ap.error("--chunk-mb must be >= 1")
    if args.stow_batch < 1:
        ap.error("--stow-batch must be >= 1")
    if args.max_window < 1:
        ap.error("--max-window must be >= 1")

    items = scan_dir(args.dir)
    if not items:
        print("no *.dcm found under %s" % args.dir)
        return 1

    port = args.port or (8104 if args.mode == "dimse" else 8024)
    scp_port = args.scp_port or (port + 100)

    studies = sorted(set(it.study for it in items))
    series = sorted(set((it.study, it.series) for it in items))
    total_bytes = sum(it.size for it in items)

    print("=== ab_test.py -%s %s %d files (%s) / %d studies / %d series ==="
          % (args.mode, args.dir, len(items), fmt_mb(total_bytes),
             len(studies), len(series)))

    recv_dir = tempfile.mkdtemp(prefix="abtest_recv_")
    rc = 0
    try:
        if args.mode == "dimse":
            eng = DimseEngine(args, recv_dir)
            eng.register_contexts(items)
            t_up = 0.0
            if not args.only_read:
                print("[upload] C-STORE -> %s:%d (%s)" % (args.host, port, args.aet))
                t0 = time.monotonic()
                st, meter, ramp, ok = eng.upload(
                    items, args.chunk_mb, args.max_window, args.verbose)
                report("upload", st, meter, ramp, t0)
                t_up = time.monotonic() - t0
                rc = 0 if (ok and st.ok == st.total) else 2
            if not args.only_upload:
                if args.move_level == "STUDY":
                    units = [(s,) for s in studies]
                elif args.move_level == "SERIES":
                    units = series
                else:
                    units = sorted(set((it.study, it.series, it.sop) for it in items))
                drain_dt = wait_for_drain(args)
                if not args.only_read and drain_dt > 0:
                    print("[ingest] upload %.1f s + drain %.1f s = %.1f s total"
                          % (t_up, drain_dt, t_up + drain_dt))
                eng.cfg.scp_port = scp_port
                eng.start_scp(scp_port)
                print("[read] C-MOVE level=%s -> %s (dest %s:%d, listening :%d)"
                      % (args.move_level, args.scp_aet, args.scp_host,
                         scp_port, scp_port))
                t0 = time.monotonic()
                st, meter, ramp, ok = eng.read(
                    units, args.move_level, args.chunk_mb, args.max_window,
                    args.verbose)
                report("read", st, meter, ramp, t0)
                print("  SCP received %d instances" % eng.received_files)
                # Give the bridge time to close the Storage Commitment cycle:
                # after a C-MOVE it sends an N-ACTION SC request and then a
                # single N-EVENT-REPORT per batch. Stay alive briefly so the
                # report lands before shutdown (read timing is already done).
                time.sleep(10.0)
                print("  SCP N-EVENT-REPORT received: %d" % eng.nevent_received)
                eng.shutdown()
                if rc == 0 and (not ok or st.ok != st.total):
                    rc = 2
        else:
            eng = WebEngine(args, recv_dir)
            if not args.only_read:
                print("[upload] STOW-RS -> %s (batch=%d)" % (eng.base, args.stow_batch))
                batches = eng.batches(items, args.stow_batch)
                t0 = time.monotonic()
                st, meter, ramp, ok = eng.upload(
                    batches, args.chunk_mb, args.max_window, args.verbose)
                report("upload", st, meter, ramp, t0)
                rc = 0 if (ok and st.ok == st.total) else 2
            if not args.only_upload:
                print("[read] WADO-RS <- %s" % eng.base)
                t0 = time.monotonic()
                st, meter, ramp, ok = eng.read(
                    series, args.chunk_mb, args.max_window, args.verbose)
                report("read", st, meter, ramp, t0)
                if rc == 0 and (not ok or st.ok != st.total):
                    rc = 2
    finally:
        if args.keep:
            print("received kept in %s" % recv_dir)
        else:
            shutil.rmtree(recv_dir, ignore_errors=True)
    return rc


if __name__ == "__main__":
    sys.exit(main())
