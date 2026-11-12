# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""1013 rabbit pass 6, B2 - `clarus_url` with port 0 was accepted.

`urlsplit("http://host:0/dicomweb").port` returns 0 without raising, so the
`except ValueError` branch never fired and the bridge started against an
endpoint it cannot reach while the error message promised "1 and 65535".
"""

import os
import shutil
import tempfile

import pytest
from clean_code.config import ConfParser, validate_clarus_url


def _cfg(text):
    tmp = tempfile.mkdtemp(prefix="conf_url6_")
    path = os.path.join(tmp, "clbridge.conf")
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)
    cfg = ConfParser(path)
    cfg._tmpdir = tmp
    return cfg


@pytest.fixture
def conf():
    cfgs = []

    def _make(text):
        cfg = _cfg(text)
        cfgs.append(cfg)
        return cfg

    yield _make
    for cfg in cfgs:
        shutil.rmtree(cfg._tmpdir, ignore_errors=True)


class TestB2PortZero:
    def test_port_zero_is_a_startup_error(self, conf):
        cfg = conf("[dicomweb]\nclarus_url = http://127.0.0.1:0/dicomweb\n")
        with pytest.raises(ValueError, match="port 0"):
            validate_clarus_url(cfg)

    def test_port_zero_is_reported_with_the_claimed_range(self, conf):
        """The message must name the range the code now actually enforces."""
        cfg = conf("[dicomweb]\nclarus_url = http://127.0.0.1:0/dicomweb\n")
        with pytest.raises(ValueError, match="between 1 and 65535"):
            validate_clarus_url(cfg)

    def test_a_real_port_still_passes(self, conf):
        cfg = conf("[dicomweb]\nclarus_url = http://127.0.0.1:8024/dicomweb\n")
        url, _, _ = validate_clarus_url(cfg)
        assert url == "http://127.0.0.1:8024/dicomweb"

    def test_a_url_without_a_port_still_passes(self, conf):
        """No port is `None`, not 0: the scheme default applies downstream, so
        rejecting it would break `https://host/dicomweb`."""
        cfg = conf("[dicomweb]\nclarus_url = https://tsami.example.com/dicomweb\n")
        url, _, _ = validate_clarus_url(cfg)
        assert url == "https://tsami.example.com/dicomweb"

    def test_an_out_of_range_port_is_still_a_syntax_error(self, conf):
        cfg = conf("[dicomweb]\nclarus_url = http://127.0.0.1:99999/dicomweb\n")
        with pytest.raises(ValueError, match="invalid port"):
            validate_clarus_url(cfg)

    def test_the_port_is_none_not_zero_without_one(self):
        """Pins the premise behind keeping `None` valid."""
        from urllib.parse import urlsplit

        assert urlsplit("https://host/dicomweb").port is None
        assert urlsplit("http://host:0/dicomweb").port == 0
