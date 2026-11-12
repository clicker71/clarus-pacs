# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Unit tests for clarus_bridge.registry — SOP class registry."""

from clean_code.registry import (
    _BUILTIN_SOP_CLASSES,
    _get_sop_registry_uids,
    _is_sop_accepted,
)


class TestBuiltinSopClasses:
    """Tests for _BUILTIN_SOP_CLASSES()."""

    def test_returns_expected_number_of_classes(self):
        registry = _BUILTIN_SOP_CLASSES()
        assert len(registry) == 13

    def test_current_ultrasound_multiframe_is_accepted(self):
        """A cine loop arrives as ...3.1 on current equipment; without it a
        Multi-frame US study is rejected whenever no sop_classes.yaml exists."""
        registry = _BUILTIN_SOP_CLASSES()
        assert _is_sop_accepted(registry, "1.2.840.10008.5.1.4.1.1.3.1") is True

    def test_current_nuclear_medicine_is_accepted(self):
        """NM is ...20 on current equipment; ...5 is the Retired UID."""
        registry = _BUILTIN_SOP_CLASSES()
        assert _is_sop_accepted(registry, "1.2.840.10008.5.1.4.1.1.20") is True

    def test_verification_is_accepted(self):
        registry = _BUILTIN_SOP_CLASSES()
        assert registry["1.2.840.10008.1.1"]["accept"] is True

    def test_ct_storage_is_accepted(self):
        registry = _BUILTIN_SOP_CLASSES()
        assert registry["1.2.840.10008.5.1.4.1.1.2"]["accept"] is True


class TestIsSopAccepted:
    """Tests for _is_sop_accepted()."""

    def test_accepted_class_returns_true(self):
        registry = _BUILTIN_SOP_CLASSES()
        assert _is_sop_accepted(registry, "1.2.840.10008.1.1") is True

    def test_unknown_class_returns_false(self):
        registry = _BUILTIN_SOP_CLASSES()
        assert _is_sop_accepted(registry, "9.9.9.9.9") is False

    def test_disabled_class_returns_false(self):
        registry = {"1.2.3": {"name": "Test", "accept": False}}
        assert _is_sop_accepted(registry, "1.2.3") is False


class TestGetSopRegistryUids:
    """Tests for _get_sop_registry_uids()."""

    def test_returns_all_accepted_uids(self):
        registry = _BUILTIN_SOP_CLASSES()
        uids = _get_sop_registry_uids(registry)
        assert len(uids) == 13
        assert "1.2.840.10008.1.1" in uids

    def test_filters_out_disabled(self):
        registry = {
            "1.2.3": {"name": "A", "accept": True},
            "4.5.6": {"name": "B", "accept": False},
        }
        uids = _get_sop_registry_uids(registry)
        assert uids == ["1.2.3"]
