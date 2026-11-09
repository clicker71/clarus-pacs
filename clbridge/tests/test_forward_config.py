# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""0409 Stage 1: [forward] config read + validation + metrics skeleton.

Owner decisions (binding): absent or enabled=false changes NO behaviour; when
enabled=true the destination AE/host/port are required and port 1..65535 and
forward_max_queue 1..100000 are enforced. Metrics expose the clarus_bridge_forward_*
family in the scrape output.
"""

import os
import shutil
import tempfile

import pytest
from clean_code.config import ConfParser
from clean_code.main import _populate_args, _validate_config


def _write(text):
    """Write a temp config file and return (path, dir) for cleanup."""
    tmp = tempfile.mkdtemp(prefix="conf_fwd_")
    path = os.path.join(tmp, "clbridge.conf")
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)
    return path, tmp


# A base config valid in every section (so [forward] is the only variable).
BASE = (
    "[server]\nport = 8104\n"
    "[dicomweb]\nclarus_url = http://127.0.0.1:8024/dicomweb\n"
    "[metrics]\nhealth_port = 8105\n"
    "[outbox]\noutbox_dir = clbridge/outbox\n"
)


@pytest.fixture
def conf():
    tmpdirs = []

    def _make(extra):
        path, tmp = _write(BASE + extra)
        tmpdirs.append(tmp)
        return ConfParser(path)

    yield _make
    for td in tmpdirs:
        shutil.rmtree(td, ignore_errors=True)


class TestNoForwardBlock:
    """A deployment with no [forward] section must behave exactly as before."""

    def test_defaults_when_section_absent(self, conf):
        cfg = conf("")
        args = _populate_args(cfg)
        assert args.forward_enabled is False
        assert args.forward_dest_ae is None
        assert args.forward_dest_host is None
        assert args.forward_dest_port == 0
        assert args.forward_max_queue == 20000

    def test_validation_passes_when_section_absent(self, conf):
        cfg = conf("")
        # Must not raise: an upgrade without [forward] is still valid.
        _validate_config(cfg)


class TestDisabledDoesNotRequireDest:
    def test_enabled_false_ignores_missing_dest(self, conf):
        cfg = conf("[forward]\nenabled = false\n")
        _validate_config(cfg)
        args = _populate_args(cfg)
        assert args.forward_enabled is False

    def test_enabled_false_even_with_partial_keys(self, conf):
        cfg = conf("[forward]\nenabled = false\ndest_host = 1.2.3.4\n")
        # explicit false -> host alone ignored, no error
        _validate_config(cfg)


class TestEnabledRequiresDestination:
    def test_enabled_true_needs_ae(self, conf):
        cfg = conf(
            "[forward]\nenabled = true\ndest_host = 192.168.1.20\ndest_port = 104\n"
        )
        with pytest.raises(ValueError, match="forward.dest_ae is required"):
            _validate_config(cfg)

    def test_enabled_true_needs_host(self, conf):
        cfg = conf("[forward]\nenabled = true\ndest_ae = REGIONAL\ndest_port = 104\n")
        with pytest.raises(ValueError, match="forward.dest_host is required"):
            _validate_config(cfg)

    def test_enabled_true_missing_port_is_range_error(self, conf):
        # port absent -> default 0 -> out of range 1..65535
        cfg = conf("[forward]\nenabled = true\ndest_ae = R\ndest_host = 1.2.3.4\n")
        with pytest.raises(ValueError, match="forward.dest_port"):
            _validate_config(cfg)

    def test_enabled_true_full_valid(self, conf):
        cfg = conf(
            "[forward]\n"
            "enabled = true\n"
            "dest_ae = REGIONAL_ARCHIVE\n"
            "dest_host = 192.168.1.20\n"
            "dest_port = 104\n"
            "forward_max_queue = 5000\n"
        )
        _validate_config(cfg)  # must not raise
        args = _populate_args(cfg)
        assert args.forward_enabled is True
        assert args.forward_dest_ae == "REGIONAL_ARCHIVE"
        assert args.forward_dest_host == "192.168.1.20"
        assert args.forward_dest_port == 104
        assert args.forward_max_queue == 5000


class TestRanges:
    def test_dest_port_low_rejected(self, conf):
        cfg = conf(
            "[forward]\nenabled = true\ndest_ae=R\ndest_host=1.2.3.4\ndest_port=0\n"
        )
        with pytest.raises(ValueError, match="forward.dest_port"):
            _validate_config(cfg)

    def test_dest_port_high_rejected(self, conf):
        cfg = conf(
            "[forward]\nenabled = true\ndest_ae=R\ndest_host=1.2.3.4\ndest_port=70000\n"
        )
        with pytest.raises(ValueError, match="forward.dest_port"):
            _validate_config(cfg)

    def test_forward_max_queue_low_rejected(self, conf):
        cfg = conf(
            "[forward]\nenabled = true\ndest_ae=R\ndest_host=1.2.3.4\ndest_port=104\n"
            "forward_max_queue = 0\n"
        )
        with pytest.raises(ValueError, match="forward.forward_max_queue"):
            _validate_config(cfg)

    def test_forward_max_queue_high_rejected(self, conf):
        cfg = conf(
            "[forward]\nenabled = true\ndest_ae=R\ndest_host=1.2.3.4\ndest_port=104\n"
            "forward_max_queue = 100001\n"
        )
        with pytest.raises(ValueError, match="forward.forward_max_queue"):
            _validate_config(cfg)

    def test_forward_max_queue_default_and_bounds_ok(self, conf):
        cfg = conf(
            "[forward]\nenabled = true\ndest_ae=R\ndest_host=1.2.3.4\ndest_port=104\n"
            "forward_max_queue = 1\n"
        )
        _validate_config(cfg)


class TestMetricsSkeleton:
    def test_forward_metrics_registered(self):
        # Importing clean_code.metrics registers the family (idempotent).
        import clean_code.metrics as m  # noqa: F401  (registration side effect)
        from prometheus_client import REGISTRY, generate_latest

        text = generate_latest(REGISTRY).decode()
        for name in (
            "clarus_bridge_forward_pending",
            "clarus_bridge_forward_pending_files",
            "clarus_bridge_forward_last_success_timestamp",
            "clarus_bridge_forward_delivered_total",
            "clarus_bridge_forward_failed_total",
            "clarus_bridge_forward_undelivered_total",
            "clarus_bridge_forward_dropped_total",
        ):
            assert name in text, f"metric {name} must appear in scrape output"
