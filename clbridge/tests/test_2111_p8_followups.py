# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""P8 - follow-ons to the P7 startup/resource/security fixes.

Each test fails when its fix is reverted, so the assertion targets the defect
the finding described rather than the shape of the guard.
"""

import os
import threading

import pytest


# ---------------------------------------------------------------------------
# registry.py:107 - a skipped header must not inherit the previous class
# ---------------------------------------------------------------------------
class TestRegistrySkippedHeader:
    def test_an_unsupported_header_does_not_reach_the_previous_class(self, tmp_path):
        """`MR: &policy` is not a header this parser understands. Leaving the
        previous entry active put the `accept: false` below it onto CT."""
        from clean_code.registry import _load_sop_registry

        path = tmp_path / "sop_classes.yaml"
        path.write_text(
            "sop_classes:\n"
            "  1.2.840.10008.5.1.4.1.1.2:\n"
            "    name: CT Image Storage\n"
            "    accept: true\n"
            "MR: &policy\n"
            "    accept: false\n",
            encoding="utf-8",
        )

        registry = _load_sop_registry(str(path))

        assert registry["1.2.840.10008.5.1.4.1.1.2"]["accept"] is True


# ---------------------------------------------------------------------------
# registry.py:148 - an unreadable custom registry fails closed
# ---------------------------------------------------------------------------
class TestRegistryFailsClosed:
    def test_a_directory_instead_of_a_file_is_a_configuration_error(self, tmp_path):
        """A present-but-unreadable registry must not silently install the
        built-ins, which re-enable a class the file was meant to disable."""
        from clean_code.registry import RegistryConfigError, _load_sop_registry

        with pytest.raises(RegistryConfigError):
            _load_sop_registry(str(tmp_path))

    def test_a_missing_file_still_falls_back(self, tmp_path):
        from clean_code.registry import _BUILTIN_SOP_CLASSES, _load_sop_registry

        assert _load_sop_registry(str(tmp_path / "absent.yaml")) == (
            _BUILTIN_SOP_CLASSES()
        )

    def test_an_undecodable_file_is_a_configuration_error(self, tmp_path):
        from clean_code.registry import RegistryConfigError, _load_sop_registry

        path = tmp_path / "sop_classes.yaml"
        path.write_bytes(b"\xff\xfe\x00sop_classes:\n\x00")

        with pytest.raises(RegistryConfigError):
            _load_sop_registry(str(path))


# ---------------------------------------------------------------------------
# config.py:115 / :118 - query, fragment, and credentials in diagnostics
# ---------------------------------------------------------------------------
class TestClarusUrlShape:
    def _conf(self, tmp_path, url):
        from clean_code.config import ConfParser

        path = tmp_path / "clbridge.conf"
        path.write_text(f"[dicomweb]\nclarus_url = {url}\n", encoding="utf-8")
        return ConfParser(str(path))

    @pytest.mark.parametrize(
        "url",
        [
            "https://pacs.example/dicomweb?site=a",
        ],
    )
    def test_a_query_string_is_refused(self, tmp_path, url):
        """STOW appends `/studies` as a string, so a query would swallow it."""
        from clean_code.config import validate_clarus_url

        with pytest.raises(ValueError):
            validate_clarus_url(self._conf(tmp_path, url))

    def test_a_fragment_is_refused(self, tmp_path):
        """A fragment is never sent, so a base URL carrying one would address
        a different resource than the operator wrote.

        Validation is called on the value directly: `ConfParser` treats '#' as
        the start of an inline comment, so a fragment cannot survive the config
        file - but `validate_clarus_url` is also reachable with a value built
        elsewhere (env, defaults, tests).
        """
        from clean_code.config import validate_clarus_url

        cfg = self._conf(tmp_path, "https://pacs.example/x")
        cfg.values[("dicomweb", "clarus_url")] = "https://pacs.example/dicomweb#frag"

        with pytest.raises(ValueError):
            validate_clarus_url(cfg)

    def test_a_plain_base_with_trailing_slash_still_passes(self, tmp_path):
        from clean_code.config import validate_clarus_url

        url, _cert, _key = validate_clarus_url(
            self._conf(tmp_path, "https://pacs.example/dicomweb/")
        )

        assert url == "https://pacs.example/dicomweb"

    def test_a_scheme_error_does_not_echo_credentials(self, tmp_path):
        """The scheme check ran before the credential check, so its message
        quoted the raw URL - password included."""
        from clean_code.config import validate_clarus_url

        secret = "s3cr3t"
        with pytest.raises(ValueError) as exc:
            validate_clarus_url(
                self._conf(tmp_path, f"ftp://user:{secret}@pacs.example/dicomweb")
            )

        assert secret not in str(exc.value)

    def test_a_port_error_does_not_echo_credentials(self, tmp_path):
        from clean_code.config import validate_clarus_url

        secret = "s3cr3t"
        with pytest.raises(ValueError) as exc:
            validate_clarus_url(
                self._conf(tmp_path, f"https://user:{secret}@pacs.example:abc/x")
            )

        assert secret not in str(exc.value)

    def test_a_redacted_url_still_names_its_own_fault(self, tmp_path):
        """Redaction must not cost the operator the diagnosis."""
        from clean_code.config import validate_clarus_url

        with pytest.raises(ValueError) as exc:
            validate_clarus_url(
                self._conf(tmp_path, "ftp://user:pw@pacs.example/dicomweb")
            )

        assert "invalid scheme" in str(exc.value)
        assert "pacs.example" in str(exc.value)


# ---------------------------------------------------------------------------
# prefetch.py:147 - the close callback runs once
# ---------------------------------------------------------------------------
class TestPrefetchOnStopOnce:
    def test_two_stops_run_the_callback_once(self):
        """Two calls used to start two close threads against one response."""
        from clean_code.prefetch import PartPrefetcher

        gate = threading.Event()
        calls = []

        def _on_stop():
            calls.append(1)
            gate.wait(30)

        pref = PartPrefetcher([], on_stop=_on_stop).start()
        try:
            pref.stop()
            pref.stop()
        finally:
            gate.set()

        assert len(calls) == 1, f"on_stop ran {len(calls)} times"


# ---------------------------------------------------------------------------
# main.py:998 - a failed PID write falls back like a failed create
# ---------------------------------------------------------------------------
class TestPidWriteFailure:
    def test_a_failed_write_returns_false_and_removes_the_record(
        self, monkeypatch, tmp_path
    ):
        """A buffered write reports its error on close; it must not escape."""
        from clean_code import main as bridge_main

        path = tmp_path / "b.pid"

        class _Failing:
            """A file object whose buffered write fails, as a full disk does.

            `__exit__` must close the descriptor it wraps: the production code
            relies on the handle being released before it unlinks the record,
            which is the behaviour under test.
            """

            def __init__(self, fd, mode=None):
                self._fd = fd

            def __enter__(self):
                return self

            def __exit__(self, *a):
                os.close(self._fd)
                return False

            def write(self, _s):
                raise OSError("no space left on device")

            def flush(self):
                return None

            def fileno(self):
                return self._fd

        monkeypatch.setattr(bridge_main.os, "fdopen", _Failing)

        assert bridge_main._write_pid_file(str(path)) is False
        assert not path.exists(), "the truncated record must not survive"


# ---------------------------------------------------------------------------
# main.py:1803 - the queue is active as soon as an item is claimed
# ---------------------------------------------------------------------------
class TestOutboxQueueActiveOnClaim:
    def test_claiming_an_item_clears_the_idle_marker(self):
        """A one-SOP-per-study trickle always drained before each flush, so
        `pending == 0` held on every summary and the stats never cleared."""
        import inspect

        from clean_code import main as bridge_main

        src = inspect.getsource(bridge_main._outbox_worker)
        code = "\n".join(line.split("#", 1)[0] for line in src.splitlines())
        after_claim = code.split("sop_uid, ds, blob_path, raw_bytes = result")[1]

        assert "_queue_was_empty = False" in after_claim.split("_batch")[0]


# ---------------------------------------------------------------------------
# main.py:1942 - health is rechecked on BOTH post-wait paths
# ---------------------------------------------------------------------------
class TestOutboxHealthRecheckBothPaths:
    def test_the_recheck_precedes_the_study_boundary_flush(self):
        """The idle branch rechecked; a dequeue that returned another study's
        item went on to the boundary flush unchecked."""
        import inspect

        from clean_code import main as bridge_main

        src = inspect.getsource(bridge_main._outbox_worker)
        code = "\n".join(line.split("#", 1)[0] for line in src.splitlines())
        wait_block = code.split("_shutdown_requested.wait(timeout=batch_idle_sec)")[1]
        head = wait_block.split("if result[0] is None:")[0]

        assert "health_checker.is_healthy()" in head
        assert 'outbox.requeue(result[0], "server down")' in head


# ---------------------------------------------------------------------------
# main.py:3125 - an armed peer filter with no peers fails closed
# ---------------------------------------------------------------------------
class TestPeerFilterFailsClosed:
    def test_an_empty_allowlist_with_the_knob_on_refuses_to_start(self, tmp_path):
        from clean_code import main as bridge_main

        peers = tmp_path / "peers.conf"
        peers.write_text("# nobody\n", encoding="utf-8")

        class _AE:
            require_called_aet = False
            require_calling_aet = []

        args = type(
            "A",
            (),
            {
                "peers_file": str(peers),
                "require_known_peers": True,
            },
        )()

        assert bridge_main._load_calling_ae_allowlist(str(peers), True) == []
        with pytest.raises(RuntimeError):
            bridge_main._apply_calling_ae_allowlist(_AE(), args)

    def test_the_knob_off_keeps_the_filter_off(self, tmp_path):
        from clean_code import main as bridge_main

        peers = tmp_path / "peers.conf"
        peers.write_text("# nobody\n", encoding="utf-8")

        class _AE:
            require_called_aet = False
            require_calling_aet = []

        args = type(
            "A",
            (),
            {
                "peers_file": str(peers),
                "require_known_peers": False,
            },
        )()
        bridge_main._apply_calling_ae_allowlist(_AE(), args)


# ---------------------------------------------------------------------------
# main.py:3320 - no blocking listener after shutdown has begun
# ---------------------------------------------------------------------------
class TestListenerNotStartedAfterShutdown:
    def test_the_blocking_listener_is_skipped_once_shutdown_is_requested(self):
        """pynetdicom 3.0.4 only stops servers registered when shutdown() ran,
        so a listener started afterwards would never be stopped. Drives the
        real guard: a requested shutdown must leave `start_server` uncalled."""
        from clean_code import main as bridge_main

        calls = []

        class _AE:
            def start_server(self, *a, **k):
                calls.append(a)

        started = bridge_main._start_main_listener(_AE(), "127.0.0.1", 11112, [])

        assert started is True
        bridge_main._shutdown_requested.set()
        try:
            started = bridge_main._start_main_listener(_AE(), "127.0.0.1", 11112, [])
        finally:
            bridge_main._shutdown_requested.clear()

        assert started is False
        assert len(calls) == 1, "the listener was started after shutdown"


# ---------------------------------------------------------------------------
# main.py:3526 - the state directory is locked before recovery
# ---------------------------------------------------------------------------
class TestStateLock:
    def _point_at(self, monkeypatch, tmp_path):
        from clean_code import main as bridge_main

        monkeypatch.setattr(bridge_main, "_state_lock_handle", None)
        return bridge_main

    def test_a_second_acquirer_on_the_same_state_dir_is_refused(
        self, monkeypatch, tmp_path
    ):
        """Exclusion is proved from a SECOND PROCESS.

        `msvcrt.locking` and `flock` both succeed on a second handle owned by
        the same process, so an in-process check would pass with the lock
        disabled - only a foreign owner makes the OS refuse.
        """
        import subprocess
        import sys

        from clean_code import main as bridge_main

        state = tmp_path / "state"
        state.mkdir()
        monkeypatch.setattr(bridge_main, "_state_lock_handle", None)

        pkg_dir = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        holder = (
            "import sys, time\n"
            "sys.path.insert(0, {pkg!r})\n"
            "from clean_code import main as m\n"
            "m._state_lock_handle = None\n"
            "print('HELD' if m._acquire_state_lock({state!r}) else 'REFUSED',"
            " flush=True)\n"
            "time.sleep(20)\n"
        ).format(pkg=pkg_dir, state=str(state))
        seeker = (
            "import sys\n"
            "sys.path.insert(0, {pkg!r})\n"
            "from clean_code import main as m\n"
            "m._state_lock_handle = None\n"
            "print(m._acquire_state_lock({state!r}))\n"
        ).format(pkg=pkg_dir, state=str(state))

        holder_proc = subprocess.Popen(
            [sys.executable, "-c", holder],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        try:
            first = holder_proc.stdout.readline().strip()
            assert first == "HELD", f"the holder could not take the lock: {first}"
            r = subprocess.run(
                [sys.executable, "-c", seeker],
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=60,
            )
            assert r.stdout.strip().endswith(
                "False"
            ), f"a second process acquired a held state lock: {r.stdout} {r.stderr}"
        finally:
            holder_proc.kill()
            holder_proc.wait(timeout=30)
            bridge_main._state_lock_handle = None

    def test_distinct_state_dirs_can_run_together(self, monkeypatch, tmp_path):
        """The finding is explicit: different ports AND different state must
        still be allowed."""
        from clean_code import main as bridge_main

        one = tmp_path / "a"
        two = tmp_path / "b"
        one.mkdir()
        two.mkdir()

        monkeypatch.setattr(bridge_main, "_state_lock_handle", None)
        handles = []
        try:
            assert bridge_main._acquire_state_lock(str(one)) is True
            handles.append(bridge_main._state_lock_handle)
            # The OS lock is per file, so an independent directory is
            # independent even though the module keeps one handle.
            assert bridge_main._acquire_state_lock(str(two)) is True
            handles.append(bridge_main._state_lock_handle)
        finally:
            for h in handles:
                if h is not None:
                    h.close()
            bridge_main._state_lock_handle = None
