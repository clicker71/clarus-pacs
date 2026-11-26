# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""T3 config: [server] require_known_peers and the peers.conf allowlist.

The knob decides whether the calling-AE filter is armed; the titles come from
peers.conf, the same file the C-MOVE destination and the N-EVENT requestor
already resolve through, so a peer is configured in exactly one place.
"""

import os
import shutil
import tempfile

import pytest
from clean_code.config import ConfParser
from clean_code.main import _load_calling_ae_allowlist, _populate_args

BASE = (
    "[server]\nport = 8104\n"
    "[dicomweb]\nclarus_url = http://127.0.0.1:8024/dicomweb\n"
    "[metrics]\nhealth_port = 8105\n"
    "[outbox]\noutbox_dir = clbridge/outbox\n"
)

PEERS = """
# comment lines are ignored
WEASIS_AE  192.168.1.0/24:11113
DCST4  127.0.0.1:11113
DMWLT4  127.0.0.1:11114
CT_ROOM  10.0.0.5:104
"""


@pytest.fixture
def workspace():
    dirs = []

    def _make(conf_text=BASE, peers_text=PEERS):
        tmp = tempfile.mkdtemp(prefix="conf_peers_")
        dirs.append(tmp)
        conf_path = os.path.join(tmp, "clbridge.conf")
        with open(conf_path, "w", encoding="utf-8") as fh:
            fh.write(conf_text)
        peers_path = os.path.join(tmp, "peers.conf")
        if peers_text is not None:
            with open(peers_path, "w", encoding="utf-8") as fh:
                fh.write(peers_text)
        return ConfParser(conf_path), peers_path

    yield _make
    for d in dirs:
        shutil.rmtree(d, ignore_errors=True)


class TestRequireKnownPeersKnob:
    """Default ON: an unknown caller is a typo or an unauthorised peer."""

    def test_default_is_on(self, workspace):
        cfg, _peers = workspace()
        assert _populate_args(cfg).require_known_peers is True

    @pytest.mark.parametrize("value", ["true", "1", "yes", "on", "TRUE"])
    def test_truthy_spellings_arm_the_filter(self, workspace, value):
        # A later [server] section reopens the same section, so the key lands
        # where _populate_args reads it.
        cfg, _peers = workspace(BASE + f"[server]\nrequire_known_peers = {value}\n")
        assert _populate_args(cfg).require_known_peers is True

    @pytest.mark.parametrize("value", ["false", "0", "no", "off"])
    def test_falsy_spellings_disarm_it(self, workspace, value):
        cfg, _peers = workspace(BASE + f"[server]\nrequire_known_peers = {value}\n")
        assert _populate_args(cfg).require_known_peers is False

    def test_a_nonsense_value_is_a_startup_error(self, workspace):
        """get_bool maps anything unrecognised to False, which would silently
        DISARM the allowlist: a typo must not read as "off"."""
        from clean_code.main import _validate_config

        cfg, _peers = workspace(BASE + "[server]\nrequire_known_peers = tru\n")
        with pytest.raises(ValueError, match="require_known_peers"):
            _validate_config(cfg)

    def test_a_valid_value_passes_validation(self, workspace):
        from clean_code.main import _validate_config

        cfg, _peers = workspace(BASE + "[server]\nrequire_known_peers = true\n")
        _validate_config(cfg)  # must not raise


class TestAllowlistFromPeersConf:
    def test_titles_come_from_the_file(self, workspace):
        _cfg, peers = workspace()
        titles = _load_calling_ae_allowlist(peers, enabled=True)
        assert set(titles) == {"WEASIS_AE", "DCST4", "DMWLT4", "CT_ROOM"}

    def test_our_own_test_aes_are_listed(self, workspace):
        """The prompt's requirement: blocking our own probes would make the
        filter untestable in the field."""
        _cfg, peers = workspace()
        titles = set(_load_calling_ae_allowlist(peers, enabled=True))
        assert "DCST4" in titles, "the DICOM C-Store Tester must stay allowed"
        assert "DMWLT4" in titles, "the MWL tester must stay allowed"

    def test_a_cidr_row_contributes_its_title_once(self, workspace):
        """An AE with both a subnet row and an exact row is one AE."""
        peers_text = "VIEWER  192.168.1.0/24:11113\nVIEWER  10.0.0.5:11113\n"
        _cfg, peers = workspace(peers_text=peers_text)
        titles = _load_calling_ae_allowlist(peers, enabled=True)
        assert titles.count("VIEWER") == 1

    def test_titles_are_normalised_to_upper_case(self, workspace):
        _cfg, peers = workspace(peers_text="ct_room  10.0.0.5:104\n")
        assert _load_calling_ae_allowlist(peers, enabled=True) == ["CT_ROOM"]

    def test_disabled_knob_yields_an_empty_list(self, workspace):
        """An empty list is what disarms pynetdicom's filter."""
        _cfg, peers = workspace()
        assert _load_calling_ae_allowlist(peers, enabled=False) == []

    def test_a_missing_file_does_not_raise(self, workspace):
        _cfg, peers = workspace(peers_text=None)
        assert _load_calling_ae_allowlist(peers, enabled=True) == []

    def test_an_empty_file_yields_nothing(self, workspace):
        _cfg, peers = workspace(peers_text="# nothing here\n")
        assert _load_calling_ae_allowlist(peers, enabled=True) == []


class TestShippedPeersConf:
    """The file in the tree must keep our probes and document the port rule."""

    def _shipped(self):
        here = os.path.dirname(os.path.abspath(__file__))
        return os.path.join(os.path.dirname(here), "peers.conf")

    def test_shipped_file_lists_the_test_aes(self):
        titles = set(_load_calling_ae_allowlist(self._shipped(), enabled=True))
        assert {"DCST4", "DMWLT4"} <= titles, titles

    def test_shipped_file_explains_the_port_rule(self):
        with open(self._shipped(), encoding="utf-8") as fh:
            text = fh.read()
        assert "CT_ROOM" in text, "the commented CT_ROOM example is missing"
        lowered = text.lower()
        assert (
            "outbound" in lowered and "c-move" in lowered
        ), "peers.conf must say the port is only the outbound C-MOVE target"
        assert (
            "store-only" in lowered or "store only" in lowered
        ), "peers.conf must say a store-only modality needs no port"
