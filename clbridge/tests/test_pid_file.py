# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""PID-file policy tests (stage 4, task H).

Incident 04.09: the bridge wrote its PID file next to the script, did not
catch PermissionError, crashed, and systemd restarted it in a loop. The
policy under test is:

  1. Linux default `/var/run/clbridge-{port}.pid`, else next to the script;
  2. a write error is a WARN, never a crash;
  3. a live foreign pid is FATAL;
  4. a stale pid is overwritten (nginx style).
"""

import logging
import os
import sys

import clean_code.main as _main_mod
import pytest
from clean_code.main import (
    _acquire_pid_file,
    _default_pid_path,
    _fallback_pid_path,
    _write_pid_file,
)

# ── path resolution ───────────────────────────────────────────────────


def test_linux_default_is_var_run(monkeypatch):
    monkeypatch.setattr(sys, "platform", "linux")
    # Compare via os.path.join so the assertion holds on a Windows dev box
    # too (this suite runs on Windows and on the Pi).
    assert _default_pid_path(11112) == os.path.join("/var/run", "clbridge-11112.pid")
    assert _default_pid_path(1).replace("\\", "/").startswith("/var/run/")


def test_non_linux_default_is_next_to_script(monkeypatch):
    monkeypatch.setattr(sys, "platform", "win32")
    path = _default_pid_path(11112)
    assert path.endswith("clbridge-11112.pid")
    assert "/var/run" not in path.replace("\\", "/")
    # The directory is a real, existing directory - the script dir.
    assert os.path.isdir(os.path.dirname(path))


def test_fallback_is_next_to_script(monkeypatch):
    monkeypatch.setattr(sys, "platform", "linux")
    path = _fallback_pid_path(11112)
    assert path.endswith("clbridge-11112.pid")
    assert not path.startswith("/var/run")
    assert os.path.isdir(os.path.dirname(path))


# ── _write_pid_file: write, stale, live ───────────────────────────────


def test_writes_own_pid(tmp_path):
    target = tmp_path / "clbridge-1.pid"
    _write_pid_file(str(target))
    assert target.read_text().strip() == str(os.getpid())


def test_stale_pid_is_overwritten(tmp_path, caplog):
    target = tmp_path / "clbridge-1.pid"
    # A pid that cannot be running: pid 0 is never a normal process, and
    # os.kill(0, 0) on POSIX signals the process group. Use a very high pid
    # that does not exist on this machine.
    dead_pid = 999_999_999
    target.write_text(str(dead_pid))

    with caplog.at_level(logging.WARNING):
        _write_pid_file(str(target))

    assert target.read_text().strip() == str(os.getpid())
    assert any("Stale PID file" in r.message for r in caplog.records)


def test_live_foreign_pid_is_fatal(tmp_path, monkeypatch):
    """A live pid that is NOT us must abort the process.

    The pid has to be genuinely different from ours: a file naming the CURRENT
    process is not a competing instance (that is the restart-in-a-container
    case), so main only refuses a FOREIGN live pid.
    """
    target = tmp_path / "clbridge-1.pid"
    target.write_text("999999")

    calls = {}

    def fake_is_alive(pid):
        calls["pid"] = pid
        return True  # alive -> the file names a running FOREIGN process

    def fake_exit(code):
        calls["exit"] = code
        raise SystemExit(code)

    # Patch the module's own probe: `_pid_is_alive` uses ctypes on Windows and
    # os.kill elsewhere, so patching os.kill only works on one platform.
    monkeypatch.setattr(_main_mod, "_pid_is_alive", fake_is_alive)
    monkeypatch.setattr(os, "_exit", fake_exit)

    with pytest.raises(SystemExit) as excinfo:
        _write_pid_file(str(target))

    assert excinfo.value.code == 1
    assert calls["exit"] == 1


def test_pid_file_naming_this_process_is_not_fatal(tmp_path, monkeypatch, caplog):
    """A PID file holding OUR pid must not abort startup.

    After a crash / SIGKILL the file survives, and in a container with a stable
    filesystem across `docker restart` the new process gets the same pid - the
    liveness probe then answered "alive" for the CURRENT process and the bridge
    refused to start on every restart.
    """
    target = tmp_path / "clbridge-1.pid"
    target.write_text(str(os.getpid()))

    def fake_exit(code):
        raise AssertionError(f"refused to start with our own pid (exit {code})")

    monkeypatch.setattr(_main_mod, "_pid_is_alive", lambda pid: True)
    monkeypatch.setattr(os, "_exit", fake_exit)

    with caplog.at_level(logging.WARNING):
        _write_pid_file(str(target))  # must not raise

    assert target.read_text().strip() == str(os.getpid())


def test_unwritable_directory_warns_and_does_not_raise(caplog, monkeypatch):
    """PermissionError on open must degrade to a warning (the 04.09 crash)."""

    def fake_open(path, flags, mode=0o777):
        raise PermissionError(13, "Permission denied", path)

    monkeypatch.setattr(os, "open", fake_open)

    with caplog.at_level(logging.WARNING):
        _write_pid_file("/var/run/clbridge-1.pid")  # must not raise

    assert any("Cannot create PID file" in r.message for r in caplog.records)
    assert any(r.levelno == logging.WARNING for r in caplog.records)
    # Crucially: NOT logged as FATAL / error.
    assert not any(r.levelno >= logging.ERROR for r in caplog.records)


# ── _acquire_pid_file: automatic resolution and degradation ───────────


def test_acquire_resolves_automatically(tmp_path, monkeypatch):
    """No override exists: the platform default is what gets written.

    Replaces the former `test_acquire_uses_override_when_set` - the
    CLBRIDGE_PID_FILE env override was removed by owner verdict 13.09.
    """
    target = tmp_path / "auto.pid"
    monkeypatch.setattr("clean_code.main._default_pid_path", lambda port: str(target))
    monkeypatch.setattr("clean_code.main._fallback_pid_path", lambda port: str(target))

    path = _acquire_pid_file(11112)
    assert path == str(target)
    assert target.exists()
    assert target.read_text().strip() == str(os.getpid())


def test_acquire_falls_back_when_default_unwritable(tmp_path, monkeypatch):
    """Default dir rejected -> next-to-script is used, no exception."""
    monkeypatch.setattr(
        "clean_code.main._default_pid_path",
        lambda port: str(tmp_path / "missing-dir" / f"clbridge-{port}.pid"),
    )
    good = tmp_path / "good.pid"
    monkeypatch.setattr("clean_code.main._fallback_pid_path", lambda port: str(good))

    acquired = _acquire_pid_file(11112)
    assert acquired == str(good)
    assert good.exists()


def test_acquire_gives_up_without_crashing(tmp_path, monkeypatch, caplog):
    """No writable location at all -> WARN, empty path, no exception."""
    missing = str(tmp_path / "nope" / "a.pid")
    monkeypatch.setattr("clean_code.main._default_pid_path", lambda port: missing)
    monkeypatch.setattr("clean_code.main._fallback_pid_path", lambda port: missing)

    with caplog.at_level(logging.WARNING):
        acquired = _acquire_pid_file(11112)

    assert acquired == ""
    assert any("No writable PID file location" in r.message for r in caplog.records)
