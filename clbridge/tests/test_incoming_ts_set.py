# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""T2: IMAGE_TS_INCOMING must mirror what the server can ingest as-is.

The SCP side has no fallback - a rejected presentation context is a failed
export - so a sender whose file is HTJ2K, JPEG 2000 Part 2 or MPEG used to be
refused before it could send one byte. The list is appended to, never
reordered: uncompressed stays first because acceptor-driven negotiation picks
the first supported match when a sender offers several syntaxes.
"""

import pytest
from clean_code.protocol import IMAGE_TS, IMAGE_TS_INCOMING
from pydicom.uid import UID
from pynetdicom import AE

# Syntaxes added on 21.11, with the server-side reason each one is safe.
NEW_TS = {
    "1.2.840.10008.1.2.4.201": "HTJ2K Lossless",
    "1.2.840.10008.1.2.4.202": "HTJ2K Lossless RPCL",
    "1.2.840.10008.1.2.4.203": "HTJ2K Lossy",
    "1.2.840.10008.1.2.4.92": "JPEG 2000 Part 2 Lossless",
    "1.2.840.10008.1.2.4.93": "JPEG 2000 Part 2",
    "1.2.840.10008.1.2.4.100": "MPEG2 MP@ML",
    "1.2.840.10008.1.2.4.101": "MPEG2 MP@HL",
    "1.2.840.10008.1.2.4.102": "MPEG-4 AVC/H.264 HP L4.1",
    "1.2.840.10008.1.2.4.103": "MPEG-4 AVC/H.264 BD",
    "1.2.840.10008.1.2.4.104": "MPEG-4 AVC/H.264 HP L4.2 2D",
    "1.2.840.10008.1.2.4.105": "MPEG-4 AVC/H.264 HP L4.2 3D",
}

# The set that existed before this change, in its original order.
PREVIOUS_TS = [
    "1.2.840.10008.1.2.1",  # Explicit VR LE
    "1.2.840.10008.1.2",  # Implicit VR LE
    "1.2.840.10008.1.2.4.80",  # JPEG-LS Lossless
    "1.2.840.10008.1.2.4.70",  # JPEG Lossless SV1
    "1.2.840.10008.1.2.4.57",  # JPEG Lossless P14
    "1.2.840.10008.1.2.4.50",  # JPEG Baseline
    "1.2.840.10008.1.2.4.51",  # JPEG Extended
    "1.2.840.10008.1.2.5",  # RLE
    "1.2.840.10008.1.2.4.81",  # JPEG-LS Near-Lossless
    "1.2.840.10008.1.2.4.90",  # JPEG 2000 Lossless
    "1.2.840.10008.1.2.4.91",  # JPEG 2000
]


class TestListShape:
    def test_uncompressed_stays_first(self):
        """Negotiation is acceptor-driven: the first supported match wins, so
        the native syntaxes must stay at the head of the list."""
        assert IMAGE_TS_INCOMING[0] == "1.2.840.10008.1.2.1"
        assert IMAGE_TS_INCOMING[1] == "1.2.840.10008.1.2"

    def test_every_previous_entry_keeps_its_position(self):
        """Appended, not reordered: the prefix is byte-identical to the list
        this change extended."""
        assert IMAGE_TS_INCOMING[: len(PREVIOUS_TS)] == PREVIOUS_TS

    @pytest.mark.parametrize("uid", sorted(NEW_TS))
    def test_each_new_syntax_is_present(self, uid):
        assert uid in IMAGE_TS_INCOMING, f"{NEW_TS[uid]} ({uid}) missing"

    def test_the_list_has_no_duplicates(self):
        assert len(IMAGE_TS_INCOMING) == len(set(IMAGE_TS_INCOMING))

    def test_incoming_is_a_superset_of_the_transcode_set(self):
        """The incoming list may be broader, never narrower: every syntax the
        bridge is willing to PRODUCE must be one it accepts."""
        assert set(IMAGE_TS) <= set(IMAGE_TS_INCOMING)

    @pytest.mark.parametrize("uid", sorted(NEW_TS))
    def test_the_new_syntax_extends_beyond_the_transcode_set(self, uid):
        """These are exactly the entries the transcode set does NOT carry -
        the reason the two lists are separate."""
        assert uid not in IMAGE_TS, f"{uid} is not one of the appended entries"


class TestPynetdicomAcceptsEveryEntry:
    """A UID the installed pynetdicom does not know would raise at AE build
    time and take the whole bridge down, so each one is registered for real."""

    @pytest.mark.parametrize("uid", sorted(NEW_TS))
    def test_add_supported_context_accepts_it(self, uid):
        ae = AE(ae_title="T")
        ae.add_supported_context("1.2.840.10008.5.1.4.1.1.2", [UID(uid)])
        registered = ae.supported_contexts
        assert any(
            uid in str(ctx.transfer_syntax) for ctx in registered
        ), f"{uid} was silently dropped by pynetdicom"

    @pytest.mark.parametrize("uid", sorted(NEW_TS))
    def test_pydicom_knows_its_name(self, uid):
        """A UID with no DICOM name would print as a bare number in every log
        line, which is the signature of a typo in the table."""
        name = UID(uid).name
        assert name and name != uid, f"{uid} has no UID name in pydicom"


class TestNegotiationOverTheWholeList:
    """The SCP must be able to ACCEPT a context for each appended syntax."""

    @pytest.mark.parametrize("uid", sorted(NEW_TS))
    def test_a_context_request_for_it_is_accepted(self, uid):
        ae = AE(ae_title="T")
        ae.add_supported_context(
            "1.2.840.10008.5.1.4.1.1.2", [UID(u) for u in IMAGE_TS_INCOMING]
        )
        accepted = {
            str(ts) for ctx in ae.supported_contexts for ts in ctx.transfer_syntax
        }
        assert accepted == set(IMAGE_TS_INCOMING), (
            "every syntax in IMAGE_TS_INCOMING must be registered as a "
            f"supported context; missing={set(IMAGE_TS_INCOMING) - accepted}"
        )
        assert uid in accepted
