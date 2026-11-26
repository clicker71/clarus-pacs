"""P7 - startup, resource and security findings.

Each test here is written to fail if its fix is removed: the guard, not the
happy path, is what is asserted.
"""

import logging
import os
import threading
import time

import pytest


# ---------------------------------------------------------------------------
# registry.py:37 - a section header with an inline comment
# ---------------------------------------------------------------------------
class TestRegistrySectionHeaderComment:
    def _write(self, tmp_path, text):
        path = tmp_path / "sop_classes.yaml"
        path.write_text(text, encoding="utf-8")
        return str(path)

    def test_a_commented_section_header_keeps_its_entries(self, tmp_path, caplog):
        """Without the fix the header line ends on the comment, the section is
        never recognised and its `accept: false` is discarded - so CT comes
        back accepted from the built-ins."""
        from clean_code.registry import _load_sop_registry

        path = self._write(
            tmp_path,
            "sop_classes: # local policy\n"
            "  1.2.840.10008.5.1.4.1.1.2:\n"
            "    name: CT Image Storage\n"
            "    accept: false\n",
        )
        with caplog.at_level(logging.ERROR):
            registry = _load_sop_registry(path)

        assert registry["1.2.840.10008.5.1.4.1.1.2"]["accept"] is False


# ---------------------------------------------------------------------------
# registry.py:65 - '#' inside a quoted value
# ---------------------------------------------------------------------------
class TestRegistryQuotedHash:
    def test_a_hash_inside_quotes_survives(self, tmp_path):
        from clean_code.registry import _load_printer_registry

        path = tmp_path / "printers.yaml"
        path.write_text(
            "printers:\n"
            "  FILM:\n"
            '    name: "Film #2" # lobby\n'
            "    host: 127.0.0.1\n",
            encoding="utf-8",
        )
        registry = _load_printer_registry(str(path))

        assert registry["FILM"]["name"] == "Film #2"


# ---------------------------------------------------------------------------
# registry.py:152 - no silent restore of a rejected entry
# ---------------------------------------------------------------------------
class TestRegistryNoSilentRestore:
    def test_a_file_whose_only_entry_is_refused_fails_startup(self, tmp_path, caplog):
        from clean_code.registry import RegistryConfigError, _load_sop_registry

        path = tmp_path / "sop_classes.yaml"
        path.write_text(
            "sop_classes:\n"
            "  1.2.840.10008.5.1.4.1.1.2:\n"
            "    name: CT Image Storage\n"
            "    accept: no\n",
            encoding="utf-8",
        )
        with caplog.at_level(logging.ERROR):
            with pytest.raises(RegistryConfigError):
                _load_sop_registry(str(path))

    def test_a_valid_accept_false_entry_still_loads(self, tmp_path):
        """The finding is explicit: `accept: false` must survive."""
        from clean_code.registry import _load_sop_registry

        path = tmp_path / "sop_classes.yaml"
        path.write_text(
            "sop_classes:\n"
            "  1.2.840.10008.5.1.4.1.1.2:\n"
            "    name: CT Image Storage\n"
            "    accept: false\n",
            encoding="utf-8",
        )
        registry = _load_sop_registry(str(path))

        assert registry["1.2.840.10008.5.1.4.1.1.2"]["accept"] is False

    def test_a_missing_file_still_falls_back(self, tmp_path):
        """The missing-file fallback is documented behaviour and must stay."""
        from clean_code.registry import _BUILTIN_SOP_CLASSES, _load_sop_registry

        registry = _load_sop_registry(str(tmp_path / "absent.yaml"))

        assert registry == _BUILTIN_SOP_CLASSES()


# ---------------------------------------------------------------------------
# utils.py:50 - no logging from the signal handler
# ---------------------------------------------------------------------------
class TestSignalHandlerDoesNotLog:
    def test_the_handler_sets_the_event_and_logs_nothing(self, monkeypatch):
        """logging is not reentrant: a handler that logs can deadlock the
        interpreter against a thread already holding the logging lock."""
        from clean_code import utils

        calls = []
        monkeypatch.setattr(
            utils.logging, "warning", lambda *a, **k: calls.append(a), raising=False
        )
        monkeypatch.setattr(
            utils.logging, "info", lambda *a, **k: calls.append(a), raising=False
        )
        monkeypatch.setattr(
            utils.logging, "error", lambda *a, **k: calls.append(a), raising=False
        )
        utils._shutdown_requested.clear()
        utils._signal_seen.clear()
        try:
            utils._signal_handler(15, None)

            assert utils._shutdown_requested.is_set()
            assert calls == [], "the signal handler must not touch logging"
            assert utils.take_signal_seen() == 15
        finally:
            # The event is process-global: leaving it set makes every later
            # test in the run believe a shutdown was requested.
            utils._shutdown_requested.clear()
            utils._signal_seen.clear()


# ---------------------------------------------------------------------------
# prefetch.py:138 - stop() cannot be held by a blocked close
# ---------------------------------------------------------------------------
class TestPrefetchStopDoesNotBlockOnClose:
    def test_stop_returns_within_its_bound_when_close_blocks(self):
        """A reader blocked inside `iter_content` is only reachable by closing
        the response, but `resp.close()` can itself wait for that in-flight
        read. Run inline it sat outside the join bound and held `stop()`."""
        from clean_code.prefetch import PartPrefetcher

        release = threading.Event()

        def _blocking_close():
            release.wait(30)

        pref = PartPrefetcher([], on_stop=_blocking_close).start()

        started = time.time()
        pref.stop()
        elapsed = time.time() - started
        release.set()

        assert elapsed < 5.0, f"stop() was held for {elapsed:.1f}s by a blocked close"


# ---------------------------------------------------------------------------
# config.py - a URL carrying credentials is refused
# ---------------------------------------------------------------------------
class TestClarusUrlUserinfo:
    def _conf(self, tmp_path, url):
        from clean_code.config import ConfParser

        path = tmp_path / "clbridge.conf"
        path.write_text(f"[dicomweb]\nclarus_url = {url}\n", encoding="utf-8")
        return ConfParser(str(path))

    @pytest.mark.parametrize(
        "url",
        [
            "http://user:secret@clarus.local:8080",
            "http://user@clarus.local:8080",
        ],
    )
    def test_userinfo_is_refused(self, tmp_path, url):
        """Credentials in the URL end up in the startup log line."""
        from clean_code.config import validate_clarus_url

        with pytest.raises(ValueError):
            validate_clarus_url(self._conf(tmp_path, url))

    def test_a_plain_url_still_passes(self, tmp_path):
        from clean_code.config import validate_clarus_url

        url, _cert, _key = validate_clarus_url(
            self._conf(tmp_path, "http://clarus.local:8080")
        )
        assert url == "http://clarus.local:8080"


# ---------------------------------------------------------------------------
# config.py - TLS files resolve relative to the config file
# ---------------------------------------------------------------------------
class TestResolveConfigPath:
    def test_a_config_relative_file_wins_over_cwd(self, tmp_path):
        from clean_code.config import resolve_config_path

        cfg_dir = tmp_path / "etc"
        cfg_dir.mkdir()
        (cfg_dir / "ca.pem").write_text("x", encoding="utf-8")
        cfg_file = cfg_dir / "clbridge.conf"
        cfg_file.write_text("", encoding="utf-8")

        got = resolve_config_path("ca.pem", str(cfg_file))

        assert os.path.abspath(got) == os.path.abspath(str(cfg_dir / "ca.pem"))

    def test_an_absolute_path_is_returned_unchanged(self, tmp_path):
        from clean_code.config import resolve_config_path

        target = tmp_path / "ca.pem"
        target.write_text("x", encoding="utf-8")

        assert resolve_config_path(str(target), str(tmp_path / "c.conf")) == str(target)

    def test_an_unknown_relative_path_is_left_alone(self, tmp_path):
        from clean_code.config import resolve_config_path

        cfg_file = tmp_path / "clbridge.conf"
        cfg_file.write_text("", encoding="utf-8")

        assert resolve_config_path("nowhere.pem", str(cfg_file)) == "nowhere.pem"


# ---------------------------------------------------------------------------
# main.py:1023 - only report a PID path the process actually owns
# ---------------------------------------------------------------------------
class TestPidFileOwnership:
    def test_a_failed_write_is_reported_so_the_next_candidate_is_tried(
        self, monkeypatch, tmp_path
    ):
        """A write that fails owns nothing; reporting the path anyway made the
        caller skip the fallback location."""
        from clean_code import main as bridge_main

        monkeypatch.setattr(
            bridge_main,
            "_write_pid_file",
            lambda p: False,
        )
        bad = tmp_path / "ro"
        bad.mkdir()
        good = tmp_path / "rw"
        good.mkdir()
        monkeypatch.setattr(
            bridge_main, "_default_pid_path", lambda port: str(bad / "b.pid")
        )
        monkeypatch.setattr(
            bridge_main, "_fallback_pid_path", lambda port: str(good / "b.pid")
        )

        assert bridge_main._acquire_pid_file(11112) == ""


# ---------------------------------------------------------------------------
# main.py:960 - exclusive ownership through recovery
# ---------------------------------------------------------------------------
class TestStartupLock:
    def _point_at(self, monkeypatch, tmp_path):
        from clean_code import main as bridge_main

        monkeypatch.setattr(
            bridge_main, "_default_pid_path", lambda port: str(tmp_path / "b.pid")
        )
        monkeypatch.setattr(
            bridge_main, "_fallback_pid_path", lambda port: str(tmp_path / "b.pid")
        )
        monkeypatch.setattr(bridge_main, "_startup_lock_handle", None)
        return bridge_main

    def test_a_second_acquirer_is_refused(self, monkeypatch, tmp_path):
        """Proved from a SECOND PROCESS: both `flock` and `msvcrt.locking`
        succeed on a second handle owned by the SAME process, so an in-process
        check would only fail because some earlier test happened to hold it."""
        import subprocess
        import sys

        bridge_main = self._point_at(monkeypatch, tmp_path)

        pkg_dir = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        holder = (
            "import sys, time\n"
            "sys.path.insert(0, {pkg!r})\n"
            "from clean_code import main as m\n"
            "m._default_pid_path = lambda p: {pid!r}\n"
            "m._fallback_pid_path = lambda p: {pid!r}\n"
            "m._startup_lock_handle = None\n"
            "print('HELD' if m._acquire_startup_lock(11112) else 'REFUSED',"
            " flush=True)\n"
            "time.sleep(20)\n"
        ).format(pkg=pkg_dir, pid=str(tmp_path / "b.pid"))
        seeker = (
            "import sys\n"
            "sys.path.insert(0, {pkg!r})\n"
            "from clean_code import main as m\n"
            "m._default_pid_path = lambda p: {pid!r}\n"
            "m._fallback_pid_path = lambda p: {pid!r}\n"
            "m._startup_lock_handle = None\n"
            "print(m._acquire_startup_lock(11112))\n"
        ).format(pkg=pkg_dir, pid=str(tmp_path / "b.pid"))

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
            ), f"a second process took a held startup lock: {r.stdout} {r.stderr}"
        finally:
            holder_proc.kill()
            holder_proc.wait(timeout=30)
            bridge_main._startup_lock_handle = None


# ---------------------------------------------------------------------------
# main.py:2162 - a peer that rejects every context burns the retry budget
# ---------------------------------------------------------------------------
class TestForwardRejectedContexts:
    def _open(self, monkeypatch, rejected):
        from clean_code import main as bridge_main

        class _Assoc:
            is_established = False
            rejected_contexts = list(rejected)

            def release(self):
                return None

        class _AE:
            network_timeout = None
            acse_timeout = None
            dimse_timeout = None
            connection_timeout = None

            def __init__(self, ae_title=None):
                pass

            def add_requested_context(self, *a):
                return None

            def associate(self, *a, **k):
                return _Assoc()

        monkeypatch.setattr(bridge_main, "AE", _AE)

        return bridge_main._forward_open_assoc(
            "CLBRIDGE",
            "DEST",
            "127.0.0.1",
            11112,
            [("1.2.3", "1.2.840.10008.1.2.1")],
            5,
            5,
            5,
        )

    def test_every_context_rejected_is_not_mistaken_for_transport(self, monkeypatch):
        from clean_code import main as bridge_main

        bridge_main._LAST_OPEN_REFUSED = False
        try:
            assert self._open(monkeypatch, ["1.2.3"]) is None
            assert bridge_main._LAST_OPEN_REFUSED is True
        finally:
            bridge_main._LAST_OPEN_REFUSED = False

    def test_an_unreachable_peer_is_transport(self, monkeypatch):
        """An unreachable peer also reports an aborted association; only
        rejected_contexts is a capability verdict."""
        from clean_code import main as bridge_main

        bridge_main._LAST_OPEN_REFUSED = True
        try:
            assert self._open(monkeypatch, []) is None
            assert bridge_main._LAST_OPEN_REFUSED is False
        finally:
            bridge_main._LAST_OPEN_REFUSED = False


# ---------------------------------------------------------------------------
# main.py:1809 / :1853 - the outbox worker honours a health flip and a 507
# ---------------------------------------------------------------------------
class TestOutboxWorkerPauses:
    """The worker is a closure over a dozen collaborators; the two guards are
    asserted on its source, in the same style as the batch-state regression
    that already pins this function."""

    def _code(self):
        import inspect

        from clean_code import main as bridge_main

        src = inspect.getsource(bridge_main._outbox_worker)
        return "\n".join(line.split("#", 1)[0] for line in src.splitlines())

    def test_health_is_rechecked_after_the_idle_wait(self):
        """The pre-wait check cannot see a server that died during the wait,
        and flushing then charged ten retries for a known failure."""
        code = self._code()
        after_wait = code.split("_shutdown_requested.wait(timeout=batch_idle_sec)")[1]

        assert "health_checker.is_healthy()" in after_wait.split("_deliver_batch()")[0]

    def test_a_study_switch_gives_the_item_back_when_a_pause_started(self):
        """The dequeued item belongs to the NEXT study - the flush's 507 was
        not about it - so sending it burned nothing it earned."""
        code = self._code()

        assert "_pause_until > time.time()" in code
        assert 'outbox.requeue(sop_uid, "retry-after pause")' in code


# ---------------------------------------------------------------------------
# main.py:2248 - the audit temp file dies with the serialisation
# ---------------------------------------------------------------------------
class TestForwardTempFileCleanup:
    def test_a_failed_save_as_unlinks_the_partial_file(self, monkeypatch, tmp_path):
        """A partial copy of a patient image must not survive the failure."""
        import pydicom
        from clean_code import main as bridge_main

        blob = tmp_path / "blob.dcm"
        ds = pydicom.Dataset()
        ds.SOPInstanceUID = "1.2.3"
        ds.SOPClassUID = "1.2.840.10008.5.1.4.1.1.2"
        ds.file_meta = pydicom.dataset.FileMetaDataset()
        ds.file_meta.TransferSyntaxUID = pydicom.uid.ExplicitVRLittleEndian
        ds.file_meta.MediaStorageSOPClassUID = ds.SOPClassUID
        ds.file_meta.MediaStorageSOPInstanceUID = ds.SOPInstanceUID
        ds.save_as(str(blob), enforce_file_format=True)

        created = []

        class _Outbox:
            def read_mods_sidecar(self, path):
                return [{"tag": [0x0008, 0x103E], "old": "STUDY"}]

            def mark_failed(self, uid, why):
                raise AssertionError("must not fail the item")

        class _Ctx:
            context_id = 1
            abstract_syntax = "1.2.840.10008.5.1.4.1.1.2"
            transfer_syntax = ["1.2.840.10008.1.2.1"]

        class _Assoc:
            accepted_contexts = [_Ctx()]

            def send_c_store(self, path):
                raise AssertionError("nothing should be sent")

            def release(self):
                return None

        def _fake_save_as(self, path, **kw):
            created.append(path)
            with open(path, "wb") as f:
                f.write(b"partial")
            raise OSError("disk full")

        monkeypatch.setattr(
            bridge_main, "_forward_open_assoc", lambda *a, **k: _Assoc()
        )
        monkeypatch.setattr(bridge_main, "_LAST_OPEN_REFUSED", False, raising=False)
        monkeypatch.setattr(
            bridge_main, "record_modification", lambda *a, **k: None, raising=False
        )
        monkeypatch.setattr(pydicom.Dataset, "save_as", _fake_save_as)

        bridge_main._forward_process_batch(
            _Outbox(),
            [("1.2.3", "1.2.840.10008.5.1.4.1.1.2", "1.2.840.10008.1.2.1", str(blob))],
            "DEST",
            "CLBRIDGE",
            "127.0.0.1",
            11112,
            5,
            5,
            5,
        )

        assert created, "the serialisation path was not exercised"
        leftovers = [p for p in created if os.path.exists(p)]
        assert leftovers == [], f"temporary DICOM files left behind: {leftovers}"


# ---------------------------------------------------------------------------
# main.py:3053 - a QA bind conflict is told apart from other bind failures
# ---------------------------------------------------------------------------
class TestIsAddrInUse:
    def test_posix_and_windows_codes_are_recognised(self):
        from clean_code import main as bridge_main

        assert bridge_main._is_addr_in_use(OSError(98, "Address already in use"))
        assert bridge_main._is_addr_in_use(OSError(10048, "in use"))

    def test_an_unrelated_bind_failure_is_not_a_conflict(self):
        from clean_code import main as bridge_main

        assert not bridge_main._is_addr_in_use(OSError(13, "Permission denied"))
