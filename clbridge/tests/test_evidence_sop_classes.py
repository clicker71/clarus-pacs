# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""RAD-66 / AI-result evidence SOP classes in the shipped registry.

RAD-66 (Rejection Note Stored) arrives as a Key Object Selection via C-STORE,
and AIRA results arrive as SR/SEG/PR/RTSTRUCT/Parametric Map. The bridge's
built-in registry is images-only, so without these entries in
`sop_classes.yaml` every one of them would be refused at C-STORE and the
rejection-note mechanism would never see a note.

Every UID below is quoted from `standard/ps3.6/sop-class-uids.json`; none is
written from memory.
"""

import os

import pytest
from clean_code.registry import _is_sop_accepted, _load_sop_registry

SHIPPED = os.path.join(os.path.dirname(os.path.dirname(__file__)), "sop_classes.yaml")

# (UID, expected `non_image`) - from standard/ps3.6/sop-class-uids.json.
EVIDENCE = [
    ("1.2.840.10008.5.1.4.1.1.88.59", True),  # Key Object Selection Document
    ("1.2.840.10008.5.1.4.1.1.88.22", True),  # Enhanced SR
    ("1.2.840.10008.5.1.4.1.1.88.33", True),  # Comprehensive SR
    ("1.2.840.10008.5.1.4.1.1.88.34", True),  # Comprehensive 3D SR
    ("1.2.840.10008.5.1.4.1.1.66.4", True),  # Segmentation
    ("1.2.840.10008.5.1.4.1.1.481.3", True),  # RT Structure Set
    ("1.2.840.10008.5.1.4.1.1.11.1", True),  # Grayscale Softcopy PR
    ("1.2.840.10008.5.1.4.1.1.104.1", True),  # Encapsulated PDF
    ("1.2.840.10008.5.1.4.1.1.30", False),  # Parametric Map (has Pixel Data)
]


@pytest.fixture(scope="module")
def registry():
    return _load_sop_registry(SHIPPED)


@pytest.mark.parametrize("uid,non_image", EVIDENCE)
def test_evidence_class_is_accepted(registry, uid, non_image):
    entry = registry.get(uid)
    assert entry is not None, f"{uid} missing from sop_classes.yaml"
    assert entry.get("accept") is True, f"{uid} must be accepted at C-STORE"


@pytest.mark.parametrize("uid,non_image", EVIDENCE)
def test_non_image_flag_matches_pixel_data_expectation(registry, uid, non_image):
    """SR/KO/PR/RTSTRUCT have no Pixel Data; Parametric Map does.

    The flag drives the association proposal: non-image classes get the
    uncompressed-only list, images the full passthrough set. Getting it wrong
    on Parametric Map would offer only IVRLE/EVRLE for a class that routinely
    arrives compressed.
    """
    assert bool(registry[uid].get("non_image")) is non_image


def test_a_rejection_note_would_pass_the_registry_gate(registry):
    """The whole point: a KOS must not be refused at C-STORE."""
    assert _is_sop_accepted(registry, "1.2.840.10008.5.1.4.1.1.88.59") is True


def test_kos_is_marked_non_image(registry):
    # A KOS is a document, not an image: offering it the image transfer-syntax
    # set would propose contexts it can never use.
    assert registry["1.2.840.10008.5.1.4.1.1.88.59"].get("non_image") is True
