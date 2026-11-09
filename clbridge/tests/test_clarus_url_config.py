# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""2908: [dicomweb] clarus_url config validation unit tests.

Owner decisions (binding): host/clarus_port REMOVED (no backward
compatibility), clarus_url required, scheme http|https, trailing slash
stripped, mTLS pair symmetric, verify always on.
"""

import os
import shutil
import tempfile

import pytest
from clean_code.config import ConfParser, validate_clarus_url


def _cfg(text):
    """Write a temp config and return a parsed ConfParser."""
    tmp = tempfile.mkdtemp(prefix="conf_url_")
    path = os.path.join(tmp, "clbridge.conf")
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)
    cfg = ConfParser(path)
    cfg._tmpdir = tmp  # attach for cleanup
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


class TestClarusUrlValid:
    def test_http_url_ok(self, conf):
        cfg = conf("[dicomweb]\nclarus_url = http://127.0.0.1:8024/dicomweb\n")
        url, cert, key = validate_clarus_url(cfg)
        assert url == "http://127.0.0.1:8024/dicomweb"
        assert cert is None and key is None

    def test_https_url_ok(self, conf):
        cfg = conf("[dicomweb]\nclarus_url = https://tsami.example.com/dicomweb\n")
        url, _, _ = validate_clarus_url(cfg)
        assert url == "https://tsami.example.com/dicomweb"

    def test_trailing_slash_stripped(self, conf):
        cfg = conf("[dicomweb]\nclarus_url = http://127.0.0.1:8024/dicomweb/\n")
        url, _, _ = validate_clarus_url(cfg)
        assert url == "http://127.0.0.1:8024/dicomweb"

    def test_mtls_pair_accepted(self, conf):
        cfg = conf(
            "[dicomweb]\n"
            "clarus_url = https://tsami.example.com/dicomweb\n"
            "tls_client_cert = /etc/clbridge/client.crt\n"
            "tls_client_key = /etc/clbridge/client.key\n"
        )
        url, cert, key = validate_clarus_url(cfg)
        assert cert == "/etc/clbridge/client.crt"
        assert key == "/etc/clbridge/client.key"


class TestClarusUrlFailFast:
    def test_legacy_host_key_is_startup_error(self, conf):
        cfg = conf("[dicomweb]\nhost = 127.0.0.1\nport = 8024\n")
        with pytest.raises(ValueError, match="host/clarus_port removed"):
            validate_clarus_url(cfg)

    def test_legacy_clarus_port_key_is_startup_error(self, conf):
        cfg = conf("[dicomweb]\nclarus_port = 8024\n")
        with pytest.raises(ValueError, match="host/clarus_port removed"):
            validate_clarus_url(cfg)

    def test_missing_clarus_url_is_error(self, conf):
        cfg = conf("[dicomweb]\nhealth_check_mode = always\n")
        with pytest.raises(ValueError, match="clarus_url is required"):
            validate_clarus_url(cfg)

    def test_ftp_scheme_rejected(self, conf):
        cfg = conf("[dicomweb]\nclarus_url = ftp://127.0.0.1:8024/dicomweb\n")
        with pytest.raises(ValueError, match="invalid scheme"):
            validate_clarus_url(cfg)

    def test_garbage_url_rejected(self, conf):
        cfg = conf("[dicomweb]\nclarus_url = not a url at all\n")
        with pytest.raises(ValueError, match="invalid scheme"):
            validate_clarus_url(cfg)

    def test_no_host_rejected(self, conf):
        cfg = conf("[dicomweb]\nclarus_url = http:///dicomweb\n")
        with pytest.raises(ValueError, match="no host"):
            validate_clarus_url(cfg)

    def test_cert_without_key_rejected(self, conf):
        cfg = conf(
            "[dicomweb]\n"
            "clarus_url = https://tsami.example.com/dicomweb\n"
            "tls_client_cert = /etc/clbridge/client.crt\n"
        )
        with pytest.raises(ValueError, match="must be set together"):
            validate_clarus_url(cfg)

    def test_key_without_cert_rejected(self, conf):
        cfg = conf(
            "[dicomweb]\n"
            "clarus_url = https://tsami.example.com/dicomweb\n"
            "tls_client_key = /etc/clbridge/client.key\n"
        )
        with pytest.raises(ValueError, match="must be set together"):
            validate_clarus_url(cfg)
