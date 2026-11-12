# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Bounded FIFO prefetch for the C-MOVE read path (B1, 2508).

A reader thread pulls `(dicom_bytes, idx)` from a source generator and fills a
bounded queue; the sender drains it. Structural backpressure is the bounded
queue itself: the reader blocks when it is full, the sender when it is empty,
so memory is bounded by `max_parts` and the Clarus server leg stays busy while
the sender is blocked on C-STORE network I/O - raising end-to-end C-MOVE
toward the server ceiling (the serial pipeline left the server idle during
each C-STORE).
"""

import logging
import os
import queue
import threading

from clean_code.streaming import SpilledPart

# Default queue depth: 4 parts (~20 MB for typical CT slices), FIFO.
# A module-level constant so production and tests observe the same value.
DEFAULT_PREFETCH_PARTS = 4

# Sentinel: the reader has finished (EOF) or hit a fatal error to surface.
_EOF = object()


class PartPrefetcher:
    """Bounded FIFO prefetch bridging a reader thread and the sender loop.

    Usage:
        pref = PartPrefetcher(_stream_multipart_instances(resp)).start()
        for dicom_bytes, idx in pref:
            ... send ...
        # On early exit (cancel / shutdown / fatal error):
        pref.stop()
    """

    def __init__(self, source, max_parts=DEFAULT_PREFETCH_PARTS, on_stop=None):
        self._q = queue.Queue(maxsize=max_parts)
        self._source = source
        self._error = None
        self._stop = threading.Event()
        # Called ONCE by `stop()` BEFORE the join. The reader may be blocked
        # inside the source (an `iter_content` on a slow or half-open HTTP
        # response), where no queue drain can reach it; closing the response
        # makes that read return, so the join does not sit out its whole
        # timeout on every early exit.
        self._on_stop = on_stop
        self._thread = threading.Thread(
            target=self._fill, name="cmove-prefetch", daemon=True
        )

    def _fill(self):
        try:
            for item in self._source:
                if self._stop.is_set():
                    # The item is already pulled from the source and will
                    # never be consumed - release a spilled part here, since
                    # the queue cannot show it to the drain.
                    self._discard(item)
                    break
                # Blocks when the queue is full - this is the backpressure.
                self._q.put(item)
                if self._stop.is_set():
                    # Paired with the check above: a stop that landed while
                    # we were blocked in put() is seen HERE, before the source
                    # is asked for one more item that nothing would consume.
                    break
        except Exception as e:  # surface a reader failure to the sender
            self._error = e
        finally:
            try:
                self._q.put(_EOF)
            except Exception:
                pass

    def start(self):
        """Start the reader thread. Returns self."""
        self._thread.start()
        return self

    def __iter__(self):
        """Yield `(dicom_bytes, idx)` as the reader produces them, in order."""
        while True:
            item = self._q.get()
            if item is _EOF:
                if self._error is not None:
                    raise self._error
                return
            yield item

    def _drain(self):
        """Discard queued items, unlinking any spilled part they carry."""
        while True:
            try:
                item = self._q.get_nowait()
            except queue.Empty:
                return
            self._discard(item)

    @staticmethod
    def _discard(item):
        """Unlink the temp file of a discarded, never-consumed part."""
        part = item[0] if isinstance(item, tuple) and item else None
        if isinstance(part, SpilledPart):
            try:
                os.unlink(part.path)
            except OSError:
                pass

    def stop(self):
        """Force the reader to finish and wait for it (non-fatal, idempotent).

        Called when the sender exits early (cancel / shutdown / fatal error).
        The reader may be blocked on `put` while the queue is full; draining
        the queue frees a slot so it can observe the stop flag and exit.
        A drained part that was spilled to disk is unlinked here: nothing
        downstream will ever consume it, so the temp file would otherwise
        stay behind (the parser drops only malformed parts, and the sender
        loop unlinks only the part it has already taken).

        Drained TWICE: the first pass frees the slot the reader is blocked on,
        and the reader then parks the part it was holding, so a pass that
        finished before that wake-up would miss it and leave its spill file
        behind. The second pass after the join catches exactly that item.

        `on_stop` runs BEFORE the join: the drain only frees a reader blocked
        in `put`, while a reader blocked inside the source (iterating a slow
        or half-open response) is only reachable by closing what it reads.
        Without the callback every early exit waited out the whole 5 s join.
        """
        self._stop.set()
        if self._on_stop is not None:
            try:
                self._on_stop()
            except Exception:
                pass
        self._drain()
        if self._thread.is_alive():
            self._thread.join(timeout=5.0)
            if self._thread.is_alive():
                logging.warning("[B1-prefetch] reader thread did not exit in 5s")
        self._drain()
