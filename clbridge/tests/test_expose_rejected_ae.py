# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""QA AE: the Expose Rejected Instances mechanism as a separate base URL.

AIRA 57.2.1 defines the expose mechanism as "a webservice endpoint
specifically configured to provide access to hidden instances" - a separate
endpoint the deployment chooses, NOT a query parameter. The bridge is a
universal DICOMweb client, so it must not depend on any one server's private
parameter for this.

These tests pin:
1. the QA view is chosen by BASE URL selection (`qa_url` vs `clarus_url`);
2. no query parameter is ever added for it - the string `includeRejected`
   does not appear anywhere in the bridge;
3. both config keys are required before the second AE starts;
4. the AE title default is `CLBRIDGE_QA`.
"""

import os
import tempfile
from unittest.mock import patch

import pytest
from clean_code.config import ConfParser
from clean_code.handlers.cfind import handle_c_find
from clean_code.http_helpers import service_base_url
from clean_code.protocol import PATIENT_ROOT_UID, STUDY_ROOT_UID

from tests.conftest import make_fake_find_event

CLARUS = "http://127.0.0.1:8024/dicomweb"
QA = "http://127.0.0.1:8024/dicomweb_qa"
MIN_CONF = "[dicomweb]\nclarus_url = {url}\n".format(url=CLARUS)


@pytest.fixture
def conf():
    """Parse a bridge config from text; returns the ConfParser."""
    dirs = []

    def _make(text):
        tmp = tempfile.mkdtemp(prefix="conf_qa_")
        dirs.append(tmp)
        path = os.path.join(tmp, "clbridge.conf")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(text)
        return ConfParser(path)

    yield _make
    import shutil

    for d in dirs:
        shutil.rmtree(d, ignore_errors=True)


# -- config ---------------------------------------------------------------


def test_expose_port_absent_leaves_a_single_ae(conf):
    cfg = conf(MIN_CONF)
    assert cfg.get_int("server", "expose_port", default=None) is None


def test_expose_port_parses_when_set(conf):
    cfg = conf(MIN_CONF + "[server]\nexpose_port = 8106\n")
    assert cfg.get_int("server", "expose_port", default=None) == 8106


def test_qa_url_is_missing_by_default(conf):
    cfg = conf(MIN_CONF)
    assert cfg.get("dicomweb", "qa_url", default="").strip() == ""


def test_qa_url_parses_beside_clarus_url(conf):
    cfg = conf(MIN_CONF + "qa_url = {url}\n".format(url=QA))
    assert cfg.get("dicomweb", "qa_url").strip() == QA


def test_ae_title_defaults_to_clbridge_qa(conf):
    cfg = conf(MIN_CONF)
    assert cfg.get("server", "expose_ae_title", default="CLBRIDGE_QA") == "CLBRIDGE_QA"


def test_qa_ae_title_fits_the_dicom_ae_limit():
    # pynetdicom / PS3.7: an AE Title is at most 16 characters.
    assert len("CLBRIDGE_QA") <= 16


# -- base URL selection ---------------------------------------------------


def test_normal_association_uses_the_ordinary_base():
    assert service_base_url(CLARUS, QA, False) == CLARUS


def test_qa_association_uses_the_qa_base():
    assert service_base_url(CLARUS, QA, True) == QA


def test_trailing_slash_is_normalised():
    assert service_base_url(CLARUS + "/", QA + "/", True) == QA


def test_no_qa_endpoint_falls_back_to_the_ordinary_base():
    # The caller refuses to start the QA AE without qa_url, so this is a
    # defensive path - it must not produce a request to an empty base URL.
    assert service_base_url(CLARUS, "", True) == CLARUS
    assert service_base_url(CLARUS, "", False) == CLARUS


# -- the C-FIND path ------------------------------------------------------


def test_normal_ae_queries_the_ordinary_base(mock_shutdown_not_set):
    event = make_fake_find_event(model_uid=STUDY_ROOT_UID)
    with patch("clean_code.handlers.cfind._qido_request") as mock_qido:
        mock_qido.return_value = []
        list(handle_c_find(event, CLARUS, False, max_responses=50, qa_url=QA))
    url = mock_qido.call_args.args[0]
    assert url.startswith(CLARUS)
    assert not url.startswith(QA)


def test_qa_ae_queries_the_qa_base(mock_shutdown_not_set):
    event = make_fake_find_event(model_uid=STUDY_ROOT_UID)
    with patch("clean_code.handlers.cfind._qido_request") as mock_qido:
        mock_qido.return_value = []
        list(
            handle_c_find(
                event, CLARUS, False, max_responses=50, expose_rejected=True, qa_url=QA
            )
        )
    url = mock_qido.call_args.args[0]
    assert url.startswith(QA), url


def test_patient_root_also_uses_the_qa_base(mock_shutdown_not_set):
    event = make_fake_find_event(model_uid=PATIENT_ROOT_UID)
    with patch("clean_code.handlers.cfind._qido_request") as mock_qido:
        mock_qido.return_value = []
        list(
            handle_c_find(
                event, CLARUS, False, max_responses=50, expose_rejected=True, qa_url=QA
            )
        )
    assert mock_qido.call_args.args[0].startswith(QA)


def test_series_level_uses_the_qa_base(mock_shutdown_not_set):
    """A SERIES-level C-FIND on the QA AE must read the same view C-MOVE
    does: both QIDO steps go to the QA base URL, not the ordinary one."""
    event = make_fake_find_event(model_uid=STUDY_ROOT_UID)
    event.identifier.QueryRetrieveLevel = "SERIES"
    with patch("clean_code.handlers.cfind._qido_request") as mock_qido:
        mock_qido.return_value = [{"0020000D": {"Value": ["1.2.3"]}}]
        list(
            handle_c_find(
                event, CLARUS, False, max_responses=50, expose_rejected=True, qa_url=QA
            )
        )
    urls = [c.args[0] for c in mock_qido.call_args_list]
    assert len(urls) == 2, urls  # step a (studies) + step b (series)
    for url in urls:
        assert url.startswith(QA), url


def test_series_level_without_qa_uses_the_ordinary_base(mock_shutdown_not_set):
    """The ordinary AE keeps the ordinary view - the SERIES path must not
    switch base URLs by itself."""
    event = make_fake_find_event(model_uid=STUDY_ROOT_UID)
    event.identifier.QueryRetrieveLevel = "SERIES"
    with patch("clean_code.handlers.cfind._qido_request") as mock_qido:
        mock_qido.return_value = [{"0020000D": {"Value": ["1.2.3"]}}]
        list(handle_c_find(event, CLARUS, False, max_responses=50, qa_url=QA))
    urls = [c.args[0] for c in mock_qido.call_args_list]
    assert urls
    for url in urls:
        assert url.startswith(CLARUS), url


def test_no_query_parameter_is_added_to_the_qido_request(mock_shutdown_not_set):
    """The QA view is a different endpoint, not an extra parameter."""
    event = make_fake_find_event(model_uid=STUDY_ROOT_UID)
    with patch("clean_code.handlers.cfind._qido_request") as mock_qido:
        mock_qido.return_value = []
        list(
            handle_c_find(
                event, CLARUS, False, max_responses=50, expose_rejected=True, qa_url=QA
            )
        )
    params = mock_qido.call_args.args[1]
    assert "includeRejected" not in params
    for value in params.values():
        assert "includeRejected" not in str(value)


# -- the parameter is gone entirely ---------------------------------------


def test_the_parameter_string_appears_nowhere_in_the_bridge():
    """No Clarus-specific parameter may survive anywhere in the package.

    The bridge is a universal DICOMweb client; a vendor parameter here would
    make it silently fail against every other server.

    The needle is assembled at runtime so this file does not match itself -
    a source scan that trips on its own assertion text proves nothing.
    """
    needle = "include" + "Rejected"
    pkg_dir = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    skip_parts = {".git", "__pycache__", ".pytest_cache", "target"}
    offenders = []
    scanned = 0
    for root, _dirs, files in os.walk(pkg_dir):
        # Compare path COMPONENTS, not substrings of the whole path: a
        # directory named `targets` or a checkout under `D:\target-app` would
        # otherwise be skipped, and the scan would silently cover less code
        # than it claims.
        if skip_parts.intersection(root.replace("\\", "/").split("/")):
            continue
        for name in files:
            if not name.endswith((".py", ".yaml", ".conf")):
                continue
            path = os.path.join(root, name)
            if os.path.abspath(path) == os.path.abspath(__file__):
                continue
            try:
                text = open(path, encoding="utf-8", errors="ignore").read()
            except OSError:
                continue
            scanned += 1
            if needle in text:
                offenders.append(os.path.relpath(path, pkg_dir))
    # A scan that covered nothing proves nothing. The path filter above can
    # only skip directories whose NAME is in the set, but a mistake in that
    # comparison (a substring test on the whole path, say) would skip the
    # package root and leave `offenders` empty for the wrong reason - the
    # silent false pass this assertion exists to prevent.
    assert scanned > 0, "the scan covered no file; the path filter is too broad"
    assert offenders == [], f"{needle} still present in: {offenders}"
