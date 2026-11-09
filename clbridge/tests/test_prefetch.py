# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Unit tests for the C-MOVE B1 prefetch queue (2508_writer_wado_cmove_pipeline).

PartPrefetcher: a reader thread fills a bounded FIFO from a source generator,
the sender drains it; backpressure is structural (bounded queue). Tests cover
fill/drain ordering, backpressure bounding, stop-on-cancel unblock, and reader
error propagation.
"""

import time

from clean_code.prefetch import DEFAULT_PREFETCH_PARTS, PartPrefetcher


def _wait_reader_blocked(pref, timeout=2.0):
    """Wait until the reader thread has blockingly filled the queue."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        # Queue is full once qsize() == maxsize; the reader then blocks on put.
        if pref._q.full():
            return True
        time.sleep(0.01)
    return pref._q.full()


def test_prefetch_preserves_fifo_order_and_all_parts():
    """Fill + drain: all N parts come out in source order through a 2-slot queue."""
    n = 20
    src = ((f"part-{i}", i) for i in range(n))
    pref = PartPrefetcher(src, max_parts=2).start()
    out = list(pref)
    assert [d for d, _ in out] == [f"part-{i}" for i in range(n)]
    assert [i for _, i in out] == list(range(n))


def test_prefetch_default_is_four_parts():
    """Architect 25.08: default queue depth is 4 parts (~20 MB), FIFO."""
    assert DEFAULT_PREFETCH_PARTS == 4


def test_prefetch_backpressure_bounds_buffered_parts():
    """The reader blocks on a full queue - it must not buffer the whole source.

    Drain one item at a time and confirm the reader's buffered count never
    exceeds the configured max.
    """
    n = 12
    produced = []

    def source():
        for i in range(n):
            produced.append(i)
            yield (f"part-{i}", i)

    max_parts = 2
    pref = PartPrefetcher(source(), max_parts=max_parts).start()
    # Drain slowly; at any moment the reader must not be more than max_parts
    # buffered + 1 in-flight ahead of consumption (structural backpressure).
    consumed = 0
    for _ in pref:
        consumed += 1
        # Allow the reader to catch up between drains.
        time.sleep(0.005)
        assert (
            len(produced) - consumed <= max_parts + 1
        ), "reader over-buffered beyond the bounded queue"
    assert consumed == n


def test_prefetch_stop_unblocks_reader_on_full_queue():
    """stop() drains the full queue so a reader blocked on put can exit."""
    produced = []

    def source():
        i = 0
        while True:
            produced.append(i)
            yield (f"part-{i}", i)
            i += 1

    pref = PartPrefetcher(source(), max_parts=2).start()
    assert _wait_reader_blocked(pref), "reader should fill the 2-slot queue"
    pref.stop()
    assert not pref._thread.is_alive(), "reader thread must exit after stop()"


def test_prefetch_stop_idempotent():
    """stop() may be called more than once without error."""
    src = ((f"part-{i}", i) for i in range(5))
    pref = PartPrefetcher(src, max_parts=2).start()
    list(pref)  # drain to completion (reader already finished)
    pref.stop()
    pref.stop()  # second call is a no-op
    assert not pref._thread.is_alive()


def test_prefetch_propagates_reader_error():
    """A reader error is surfaced to the sender via the iterator."""

    def source():
        yield ("part-0", 0)
        raise ValueError("boom")

    pref = PartPrefetcher(source(), max_parts=2).start()
    got = []
    raised = None
    try:
        for item in pref:
            got.append(item)
    except ValueError as e:
        raised = e
    assert got == [("part-0", 0)]
    assert raised is not None and str(raised) == "boom"


def test_stop_removes_the_spill_files_still_queued():
    """An early exit must not leak part temp files.

    A spilled part holds its excess bytes in a temp file. The parser cleans
    only malformed parts and the send loop unlinks only the part it already
    took, so without this drain cleanup a cancelled or failed large study
    leaves up to max_parts oversized files behind.

    The source yields MORE parts than the queue holds, so the reader blocks in
    put() with a slot occupied by every part the pipeline accepted. Files are
    created by the source itself - a pre-created file the source never yields
    would be nobody's leak and would make this test lie.
    """
    import os
    import tempfile

    from clean_code.streaming import SpilledPart

    created = []

    def source():
        for i in range(DEFAULT_PREFETCH_PARTS + 2):
            fd, path = tempfile.mkstemp(prefix=f"spill{i}_", suffix=".part")
            os.close(fd)
            created.append(path)
            yield SpilledPart(head=b"head", path=path), i, "1.2.840.10008.1.2.1"

    gen = source()
    pref = PartPrefetcher(gen, max_parts=DEFAULT_PREFETCH_PARTS).start()
    assert _wait_reader_blocked(pref), "reader never filled the queue"
    pref.stop()
    gen.close()

    # A generator runs its body only up to the next yield, so a part it never
    # yielded was never created either - every file listed here went through
    # the pipeline and must be gone.
    assert [p for p in created if os.path.exists(p)] == []


def test_second_drain_unlinks_a_part_put_after_the_first_drain():
    """The second drain is what removes a part parked after the first one.

    Without a gate the race is decided by thread timing: either drain may be
    the one that catches the reader's held part. Here the reader is pinned
    inside put() for the LAST part until the first drain has returned, so that
    part can only enter the queue afterwards - only the second drain can
    unlink it. The reader never sees the stop flag for that part, so its own
    cleanup does not help.
    """
    import os
    import tempfile
    import threading

    from clean_code.streaming import SpilledPart

    created = []
    last_idx = DEFAULT_PREFETCH_PARTS

    def source():
        for i in range(last_idx + 1):
            fd, path = tempfile.mkstemp(prefix=f"race{i}_", suffix=".part")
            os.close(fd)
            created.append(path)
            yield SpilledPart(head=b"head", path=path), i, "1.2.840.10008.1.2.1"

    gen = source()
    pref = PartPrefetcher(gen, max_parts=DEFAULT_PREFETCH_PARTS)

    put_blocked = threading.Event()
    first_drain_done = threading.Event()
    real_put = pref._q.put

    def gated_put(item, *args, **kwargs):
        is_last_part = isinstance(item, tuple) and len(item) > 1 and item[1] == last_idx
        if is_last_part:
            put_blocked.set()
            first_drain_done.wait(timeout=5)
        real_put(item, *args, **kwargs)

    pref._q.put = gated_put

    real_drain = pref._drain
    drains = []

    def gated_drain():
        real_drain()
        drains.append(1)
        if len(drains) == 1:
            first_drain_done.set()

    pref._drain = gated_drain

    pref.start()
    assert put_blocked.wait(timeout=5), "reader never reached the last part"
    pref.stop()
    gen.close()

    # The last part reached the queue only after the first drain returned, so
    # its file survives exactly when the second drain is gone.
    assert [p for p in created if os.path.exists(p)] == []
