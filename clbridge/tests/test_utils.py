# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Unit tests for clarus_bridge.utils — auth, metrics, multipart streaming."""

import pytest
from clean_code.streaming import (
    DICM_MAGIC,
    DICM_OFFSET,
    _is_valid_dicom,
    _parse_content_length,
    _stream_multipart_instances,
)
from clean_code.utils import _measure_dataset_bytes, mask_patient_id
from pydicom import Dataset

# ── Helpers for building valid DICOM-like test data ───────────────────


def _make_dicom_body(size: int, seed: int = 0) -> bytes:
    """Build a DICOM-like byte string with magic bytes at offset 128."""
    assert size >= 132, f"DICOM body must be at least 132 bytes, got {size}"
    body = bytearray(size)
    # Write DICM magic at offset 128
    body[DICM_OFFSET : DICM_OFFSET + 4] = DICM_MAGIC
    # Fill rest with varying bytes (not all zeros, to avoid boundary-like patterns)
    for i in range(size):
        if DICM_OFFSET <= i < DICM_OFFSET + 4:
            continue
        body[i] = (i + seed) % 251 + 1  # Avoid 0x00, 0x0A, 0x0D in artificial data
    return bytes(body)


def _make_part(
    boundary: str,
    content_length: int,
    body: bytes | None = None,
    extra_headers: str = "",
) -> bytes:
    """Build a single MIME part with Content-Length header."""
    if body is None:
        body = _make_dicom_body(content_length)
    return (
        f"--{boundary}\r\n"
        f"Content-Type: application/dicom\r\n"
        f"Content-Length: {content_length}\r\n"
        f"{extra_headers}"
        f"\r\n"
    ).encode("ascii") + body


def _make_inter_part_delimiter(boundary: str) -> bytes:
    """Build inter-part boundary delimiter: \\r\\n--BOUNDARY\\r\\n"""
    return f"\r\n--{boundary}\r\n".encode("ascii")


def _make_final_delimiter(boundary: str) -> bytes:
    """Build final boundary delimiter: \\r\\n--BOUNDARY--\\r\\n"""
    return f"\r\n--{boundary}--\r\n".encode("ascii")


# ── Tests ────────────────────────────────────────────────────────────


class TestMeasureDatasetBytes:
    """Tests for _measure_dataset_bytes()."""

    def test_measures_dataset_size(self):
        ds = Dataset()
        ds.PatientName = "Test"
        size = _measure_dataset_bytes(ds)
        assert size > 0

    def test_with_event_fallback(self):
        ds = Dataset()
        ds.PatientName = "Test"
        size = _measure_dataset_bytes(ds, event=None)
        assert size > 0


class MockResponse:
    """Mock requests.Response for _stream_multipart_instances tests."""

    def __init__(self, chunks, boundary, content_type=None):
        self._chunks = chunks
        self._boundary = boundary
        self.headers = {
            "Content-Type": content_type
            or f'multipart/related; type="application/dicom"; boundary={boundary}'
        }

    def iter_content(self, chunk_size=65536):
        yield from self._chunks


class TestMultipartEdgeCases:
    """Edge cases for _stream_multipart_instances() — P10 Content-Length framing."""

    def test_empty_body_returns_nothing(self):
        """Final delimiter only, no parts — yields nothing."""
        boundary = "abc123"
        body = f"--{boundary}--\r\n".encode("ascii")
        resp = MockResponse([body], boundary)
        results = list(_stream_multipart_instances(resp))
        assert results == []

    def test_boundary_appears_in_dicom_data(self):
        """Boundary-like bytes inside DICOM part — Content-Length prevents collision."""
        boundary = "abc123"
        boundary_bytes = f"--{boundary}".encode("ascii")
        # Build DICOM body with boundary bytes INSIDE it
        dicom_body = bytearray(200)
        dicom_body[DICM_OFFSET : DICM_OFFSET + 4] = DICM_MAGIC
        # Embed boundary bytes in the middle of pixel data
        dicom_body[150 : 150 + len(boundary_bytes)] = boundary_bytes
        dicom_body = bytes(dicom_body)

        part = _make_part(boundary, len(dicom_body), body=dicom_body)
        body = part + _make_final_delimiter(boundary)
        resp = MockResponse([body], boundary)
        results = list(_stream_multipart_instances(resp))
        assert len(results) == 1
        part_bytes, part_index, part_ts = results[0]
        # The boundary bytes inside DICOM data must be preserved
        assert boundary_bytes in part_bytes
        assert len(part_bytes) == len(dicom_body)
        assert part_ts is None  # no transfer-syntax param in this fixture

    def test_truncated_chunk_without_boundary(self):
        """Stream ends mid-part without final boundary — flush remaining."""
        boundary = "abc123"
        dicom_body = _make_dicom_body(200)  # Must be >= 132 for DICM magic
        part = _make_part(boundary, len(dicom_body), body=dicom_body)
        resp = MockResponse([part], boundary)
        results = list(_stream_multipart_instances(resp))
        assert len(results) == 1
        part_bytes, part_index, part_ts = results[0]
        assert len(part_bytes) == len(dicom_body)

    def test_multiple_parts(self):
        """Two parts, each with Content-Length — both extracted correctly."""
        boundary = "abc123"
        body1 = _make_dicom_body(200, seed=1)
        body2 = _make_dicom_body(300, seed=2)
        data = (
            _make_part(boundary, len(body1), body=body1)
            + _make_inter_part_delimiter(boundary)
            + _make_part(boundary, len(body2), body=body2).split(b"\r\n", 1)[
                1
            ]  # strip first boundary
            + _make_final_delimiter(boundary)
        )
        # Actually, let's build this properly:
        # Part 1: --BOUNDARY\r\n headers \r\n\r\n body
        # Delim:  \r\n--BOUNDARY\r\n
        # Part 2: headers \r\n\r\n body
        # Final:  \r\n--BOUNDARY--\r\n
        data = (
            _make_part(boundary, len(body1), body=body1)
            + _make_inter_part_delimiter(boundary)
            + f"Content-Type: application/dicom\r\nContent-Length: {len(body2)}\r\n\r\n".encode(
                "ascii"
            )
            + body2
            + _make_final_delimiter(boundary)
        )
        resp = MockResponse([data], boundary)
        results = list(_stream_multipart_instances(resp))
        assert len(results) == 2
        assert results[0][0] == body1
        assert results[1][0] == body2

    def test_missing_content_length_raises(self):
        """Fail closed: no Content-Length → ValueError."""
        boundary = "abc123"
        body = (
            f"--{boundary}\r\n"
            f"Content-Type: application/dicom\r\n"
            f"\r\n"
            f"SOME_DATA_WITHOUT_LENGTH"
            f"\r\n--{boundary}--\r\n"
        ).encode("ascii")
        resp = MockResponse([body], boundary)
        with pytest.raises(ValueError, match="Content-Length"):
            list(_stream_multipart_instances(resp))


class TestParseContentLength:
    """Tests for _parse_content_length()."""

    def test_parses_valid(self):
        headers = b"Content-Type: application/dicom\r\nContent-Length: 12345\r\n"
        assert _parse_content_length(headers) == 12345

    def test_returns_none_when_missing(self):
        headers = b"Content-Type: application/dicom\r\n\r\n"
        assert _parse_content_length(headers) is None

    def test_returns_none_when_malformed(self):
        headers = b"Content-Length: abc\r\n"
        assert _parse_content_length(headers) is None

    def test_case_insensitive(self):
        headers = b"content-length: 999\r\n"
        assert _parse_content_length(headers) == 999


class TestIsValidDicom:
    """Tests for _is_valid_dicom()."""

    def test_valid_dicom(self):
        data = _make_dicom_body(200)
        assert _is_valid_dicom(data) is True

    def test_too_short(self):
        data = b"\x00" * 100
        assert _is_valid_dicom(data) is False

    def test_no_magic_bytes(self):
        data = bytearray(200)
        # No DICM at offset 128
        assert _is_valid_dicom(bytes(data)) is False

    def test_magic_at_wrong_offset(self):
        data = bytearray(200)
        data[0:4] = b"DICM"  # Wrong offset
        assert _is_valid_dicom(bytes(data)) is False


class TestMaskPatientId:
    """PatientID never reaches a log line raw: a national/MRN identifier has
    no safe partial mask (its prefix is site-coded), so it is hashed."""

    def test_hash_is_not_the_raw_value(self):
        assert mask_patient_id("12345") != "12345"

    def test_hash_is_twelve_hex_chars(self):
        token = mask_patient_id("12345")
        assert len(token) == 12
        assert all(c in "0123456789abcdef" for c in token)

    def test_hash_is_deterministic(self):
        assert mask_patient_id("12345") == mask_patient_id("12345")
        assert mask_patient_id("12345") != mask_patient_id("12346")

    def test_empty_id_is_the_placeholder(self):
        assert mask_patient_id("") == "***"

    def test_raw_id_does_not_appear_in_the_token(self):
        assert "98765432" not in mask_patient_id("98765432")

    def test_the_token_is_a_keyed_hmac_not_a_bare_digest(self):
        """A bare SHA-256 of a low-entropy id is reversible by enumeration, so
        the token must CHANGE when the key changes. That is the only
        observable difference between a keyed MAC and an unkeyed digest."""
        import shutil
        import tempfile

        import clean_code.utils as utils

        root_a = tempfile.mkdtemp(prefix="clbridge-mask-a-")
        root_b = tempfile.mkdtemp(prefix="clbridge-mask-b-")
        try:
            utils.set_mask_key_dir(root_a)
            first = mask_patient_id("12345")
            # Same key, same token: the log stays correlatable.
            assert mask_patient_id("12345") == first

            # The token must BE the HMAC, not merely change with the key.
            # Recomputing it from the key the module is using proves the
            # construction (HMAC-SHA256, UTF-8 message, first 12 hex chars);
            # a truncated bare digest or a different key ordering would pass
            # the "it changed" assertion above and fail here.
            import hashlib
            import hmac

            expected = hmac.new(
                utils._mask_key(), b"12345", hashlib.sha256
            ).hexdigest()[:12]
            assert first == expected, (
                "the token must be HMAC-SHA256(key, patient_id)[:12]: "
                f"expected {expected}, got {first}"
            )

            utils.set_mask_key_dir(root_b)
            second = mask_patient_id("12345")
            assert second != first, (
                "a different key must produce a different token - an unkeyed "
                "sha256 would return the same digest either way"
            )
            expected_b = hmac.new(
                utils._mask_key(), b"12345", hashlib.sha256
            ).hexdigest()[:12]
            assert second == expected_b, (expected_b, second)

            # Going back to the first key reproduces the first token exactly,
            # so nothing about the token depends on call order.
            utils.set_mask_key_dir(root_a)
            assert mask_patient_id("12345") == first
        finally:
            utils.set_mask_key_dir("")
            shutil.rmtree(root_a, ignore_errors=True)
            shutil.rmtree(root_b, ignore_errors=True)

    def test_a_short_key_file_is_replaced_not_read_back(self):
        """A truncated key file is unusable, and re-reading it would hand back
        the same bytes while the log claimed a regeneration. The replacement
        must actually reach disk: an exclusive create refuses to overwrite an
        existing file, so this path cannot go through it."""
        import os
        import shutil
        import tempfile

        import clean_code.utils as utils

        root = tempfile.mkdtemp(prefix="clbridge-mask-short-")
        try:
            key_path = os.path.join(root, ".patient_id_mask.key")
            with open(key_path, "wb") as handle:
                handle.write(b"abcd")  # 4 bytes: not a key

            utils.set_mask_key_dir(root)
            token_from_short = mask_patient_id("12345")

            with open(key_path, "rb") as handle:
                replacement = handle.read()
            assert len(replacement) == 32, (
                "the short key must be REPLACED on disk, got "
                f"{len(replacement)} bytes"
            )
            assert replacement != b"abcd", "the old bytes are gone"

            # The token is computed from the NEW key, not the short one: a
            # fresh read of the same file must reproduce it exactly.
            utils.set_mask_key_dir(root)
            assert (
                mask_patient_id("12345") == token_from_short
            ), "the token must come from the replacement key that is now on disk"

            # And the replacement is a real key: it changes the token.
            import hashlib

            short_digest = hashlib.sha256(b"abcd").hexdigest()[:12]
            assert token_from_short != short_digest, (
                "a token derived from the 4-byte file would mean the "
                "replacement never happened"
            )
        finally:
            utils.set_mask_key_dir("")
            shutil.rmtree(root, ignore_errors=True)

    def test_the_key_file_is_created_once_and_reused(self):
        import os
        import shutil
        import tempfile

        import clean_code.utils as utils

        root = tempfile.mkdtemp(prefix="clbridge-mask-key-")
        try:
            utils.set_mask_key_dir(root)
            token = mask_patient_id("12345")
            key_path = os.path.join(root, ".patient_id_mask.key")
            assert os.path.exists(key_path), "the key is persisted, not per-call"
            with open(key_path, "rb") as handle:
                assert len(handle.read()) == 32

            # A fresh process (cache cleared) reads the same file and yields
            # the same token: that is what makes old log lines correlatable.
            utils.set_mask_key_dir(root)
            assert mask_patient_id("12345") == token
        finally:
            utils.set_mask_key_dir("")
            shutil.rmtree(root, ignore_errors=True)
