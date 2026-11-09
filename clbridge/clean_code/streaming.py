# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge  -  streaming multipart parser (P10: Content-Length-based framing).

Contains _stream_multipart_instances (yields DICOM parts from streaming
multipart/related response). Parts within the spill threshold stay fully in
memory; larger parts spill the excess into a system temp file (W3, 0209).

P10: replaced boundary-searching state machine with Content-Length-based
framing. Boundary is used ONLY for verification, not for finding part
boundaries. This eliminates false boundary matches in binary pixel data.
"""

import logging
import os
import tempfile
from collections import namedtuple

# Defence in depth: prevent unbounded buffer growth on malformed multipart
MAX_BUFFER_SIZE = 100 * 1024 * 1024  # 100 MB

# Parts above this size are yielded as SpilledPart - the first
# SPILL_THRESHOLD body bytes stay in memory, the excess streams into a temp
# file. Injectable per call via the spill_threshold parameter (tests), never
# monkeypatched.
SPILL_THRESHOLD = 16 * 1024 * 1024  # 16 MB

# Yield element for an oversized part. `head` carries the first
# spill_threshold body bytes (kept in memory), `path` points at a temp file
# holding the rest. Namedtuple over (is_path, payload) tuples: attribute
# access beats indexed nested tuples at the two call sites (parser + loop).
SpilledPart = namedtuple("SpilledPart", "head path")

# DICOM magic bytes at offset 128 (PS3.10 sec.7.1)
DICM_MAGIC = b"DICM"
DICM_OFFSET = 128
DICM_MIN_SIZE = 132  # 128 + 4 magic bytes


# ---------------------------------------------------------------------------
# P10 B2: DICOM magic bytes verification (defence in depth)
# ---------------------------------------------------------------------------
def _is_valid_dicom(data: bytes) -> bool:
    """Check DICOM magic bytes ('DICM' at offset 128). PS3.10 sec.7.1."""
    if len(data) < DICM_MIN_SIZE:
        return False
    return data[DICM_OFFSET : DICM_OFFSET + 4] == DICM_MAGIC


# ---------------------------------------------------------------------------
# P10 B1: Content-Length extraction from MIME part headers
# ---------------------------------------------------------------------------
def _parse_content_length(headers_bytes: bytes) -> int | None:
    """Extract Content-Length from MIME part headers.

    Returns int or None if header is missing or malformed.
    """
    for line in headers_bytes.split(b"\r\n"):
        if line.lower().startswith(b"content-length:"):
            try:
                return int(line.split(b":", 1)[1].strip())
            except (ValueError, IndexError):
                return None
    return None


def _parse_part_transfer_syntax(headers_bytes: bytes) -> str | None:
    """Extract the `transfer-syntax=` parameter from the part's
    Content-Type header (3008 stored-first: the server names the ACTUAL
    syntax of the part bytes). Handles quoted and unquoted values and
    other parameters before/after. Returns None when absent.
    """
    for line in headers_bytes.split(b"\r\n"):
        if not line.lower().startswith(b"content-type:"):
            continue
        for part in line.split(b";")[1:]:
            kv = part.strip()
            if not kv.lower().startswith(b"transfer-syntax="):
                continue
            value = kv.split(b"=", 1)[1].strip().strip(b'"')
            value = value.decode("ascii", "replace").strip()
            return value or None
        return None
    return None


# ---------------------------------------------------------------------------
# P10 B1: Content-Length-based streaming multipart parser
# ---------------------------------------------------------------------------
def _stream_multipart_instances(
    response, chunk_size=65536, spill_threshold=SPILL_THRESHOLD
):
    """Generator: yield (part, part_index, part_ts) from streaming multipart response.

    part_ts is the `transfer-syntax=` parameter of the part's Content-Type
    header (None when the server did not emit it - old server builds). The
    bridge reads it first in the actual_ts resolution chain, so the send
    loop never trusts a DICOM parser for the stored syntax.

    P10: Content-Length-based framing. Boundary is used ONLY for verification.
    Each MIME part MUST include Content-Length header (fail closed if missing).

    Protocol on the wire (from Clarus Server S1):
        --BOUNDARY\\r\\n
        Content-Type: application/dicom\\r\\n
        Content-Length: N\\r\\n
        \\r\\n
        <N bytes DICOM>
        \\r\\n--BOUNDARY\\r\\n
        ... next part ...
        \\r\\n--BOUNDARY--\\r\\n

    Args:
        response: requests.Response with stream=True
        chunk_size: read buffer size (default 64KB)
        spill_threshold: body bytes kept in memory per part (default 16 MB)

    Yields:
        (part, part_index, part_ts)  -  part is the raw DICOM body as bytes
        (the whole part fit in memory) or SpilledPart(head, path) when the
        part exceeded spill_threshold. The head carries the first
        spill_threshold bytes - enough for the DICM magic check and the
        whole file meta group the send loop reads.

    Raises:
        ValueError: if buffer exceeds MAX_BUFFER_SIZE, boundary missing,
                    or Content-Length missing (fail closed)
    """
    # Extract boundary from Content-Type header
    content_type = response.headers.get("Content-Type", "")
    boundary = None
    for param in content_type.split(";"):
        param = param.strip()
        if param.startswith("boundary="):
            boundary = param.split("=", 1)[1].strip().strip('"')
            break
    if not boundary:
        raise ValueError("Multipart: no boundary in Content-Type header")

    # Wire-format delimiters (as sent by Clarus Server S1/S2)
    boundary_bytes = ("--" + boundary).encode("ascii")
    first_delimiter = boundary_bytes + b"\r\n"  # --BOUNDARY\r\n
    # PS3.18 sec.8.7.2: each part body is followed by CRLF, then the next boundary.
    # Content-Length may or may not include this trailing CRLF. Handle both:
    #   Case A (CRLF in Content-Length): <body>\r\n--BOUNDARY\r\n
    #   Case B (CRLF outside):          <body>\r\n\r\n--BOUNDARY\r\n
    next_delimiter = b"\r\n" + boundary_bytes + b"\r\n"  # \r\n--BOUNDARY\r\n
    next_delimiter_alt = (
        b"\r\n\r\n" + boundary_bytes + b"\r\n"
    )  # \r\n\r\n--BOUNDARY\r\n
    final_delimiter = b"\r\n" + boundary_bytes + b"--"  # \r\n--BOUNDARY--
    final_delimiter_alt = b"\r\n\r\n" + boundary_bytes + b"--"  # \r\n\r\n--BOUNDARY--
    len_next_delimiter = len(next_delimiter)
    len_next_delimiter_alt = len(next_delimiter_alt)

    buffer = b""
    part_index = 0
    preamble_skipped = False
    # W3 (0209) spill state for the oversized part currently being received.
    spill_file = None  # open temp file, None when no spill is active
    spill_path = None  # spill_file.name while active
    spill_written = 0  # body bytes already written to the file
    spill_total = 0  # Content-Length of the spilled part
    spill_body_start = 0  # buffer offset where the part body begins

    try:
        for chunk in response.iter_content(chunk_size=chunk_size):
            if not chunk:
                continue
            if spill_file is None:
                buffer += chunk
            else:
                # W3: cap the in-memory head at spill_threshold, stream the
                # body overflow to the temp file and keep only the framing
                # bytes (trailing CRLF + next delimiter) in the buffer - it
                # stays bounded no matter how large the part is.
                avail = len(buffer) - spill_body_start
                missing = spill_total - spill_written - avail
                to_head = min(spill_threshold - avail, len(chunk))
                buffer += chunk[:to_head]
                to_file = min(len(chunk) - to_head, missing - to_head)
                if to_file > 0:
                    spill_file.write(chunk[to_head : to_head + to_file])
                    spill_written += to_file
                buffer += chunk[to_head + to_file :]

            # Defence in depth: prevent unbounded growth. W3 note: with a
            # spill active the in-memory buffer is capped at spill_threshold
            # bytes, so this check is inert for the head of an oversized
            # part; it remains the guard for malformed streams (broken
            # framing never completes a part, nothing is ever consumed).
            if len(buffer) > MAX_BUFFER_SIZE:
                raise ValueError(
                    f"Multipart buffer exceeded {MAX_BUFFER_SIZE} bytes "
                    "- possible malformed response"
                )

            while True:
                # -- Phase 0: skip preamble, find first boundary ---------
                if not preamble_skipped:
                    idx = buffer.find(first_delimiter)
                    if idx == -1:
                        break  # Wait for more data
                    buffer = buffer[idx + len(first_delimiter) :]
                    preamble_skipped = True
                    # Continue to Phase 1 (don't break  -  headers follow immediately)

                # -- Phase 1: parse part headers -------------------------
                header_end = buffer.find(b"\r\n\r\n")
                if header_end == -1:
                    break  # Wait for complete headers

                # Defence: empty headers (header_end == 0) means the \r\n\r\n
                # at buffer start is a trailing CRLF from previous part body,
                # NOT the start of a new part. Check for closing delimiter.
                if header_end == 0:
                    if buffer.startswith(final_delimiter_alt):
                        return  # Clean finish (trailing CRLF + closing delimiter)
                    if buffer.startswith(next_delimiter_alt):
                        buffer = buffer[len_next_delimiter_alt:]
                        continue  # Next part (trailing CRLF + next delimiter)
                    # Not a delimiter  -  break and wait for more data
                    break

                headers_bytes = buffer[:header_end]
                body_start = header_end + 4  # Skip \r\n\r\n

                # P10: Content-Length is MANDATORY (S1  -  server provides it)
                content_length = _parse_content_length(headers_bytes)
                if content_length is None:
                    raise ValueError(
                        f"[STREAM] Part {part_index}: missing Content-Length "
                        "- server must provide Content-Length per MIME part (P10 S1). "
                        "Boundary-based framing is unsafe for binary DICOM data."
                    )
                # 3008 stored-first: per-part transfer-syntax parameter.
                part_ts = _parse_part_transfer_syntax(headers_bytes)

                # W3: parts above the threshold spill to a temp file.
                if spill_file is None and content_length > spill_threshold:
                    spill_file = tempfile.NamedTemporaryFile(
                        prefix="clbridge_spill_",
                        suffix=".part",
                        delete=False,
                    )
                    spill_path = spill_file.name
                    spill_written = 0
                    spill_total = content_length
                    spill_body_start = body_start
                    # The current chunk may already carry body bytes past
                    # the threshold - drain exactly the body overflow to the
                    # file; framing bytes (trailing CRLF + delimiter) stay
                    # in the buffer for Phase 4.
                    body_end = body_start + content_length
                    overflow = buffer[body_start + spill_threshold : body_end]
                    if overflow:
                        spill_file.write(overflow)
                        spill_written += len(overflow)
                    buffer = buffer[: body_start + spill_threshold] + buffer[body_end:]

                # -- Phase 2: extract DICOM body by length ---------------
                body_end = body_start + content_length
                if spill_file is None:
                    if len(buffer) < body_end:
                        break  # Wait for complete body
                    part = buffer[body_start:body_end]
                    buffer = buffer[body_end:]
                    check = part
                else:
                    # Head capped in memory, the excess already on disk.
                    if (len(buffer) - body_start) + spill_written < content_length:
                        break  # Wait for complete body
                    part = SpilledPart(
                        buffer[body_start : body_start + spill_threshold],
                        spill_path,
                    )
                    buffer = buffer[body_start + spill_threshold :]
                    spill_file.close()
                    spill_file = None
                    spill_path = None
                    spill_written = 0
                    check = part.head

                # -- Phase 3: yield DICOM body (Content-Length is authoritative) -
                body_valid = _is_valid_dicom(check)
                if body_valid:
                    yield (part, part_index, part_ts)
                    part_index += 1
                else:
                    if isinstance(part, SpilledPart):
                        # No consumer will ever see this part - drop the file.
                        try:
                            os.unlink(part.path)
                        except OSError:
                            pass
                    logging.warning(
                        "[STREAM] Part %d: invalid DICOM (no magic bytes), skipping",
                        part_index,
                    )

                # -- Phase 4: skip past next delimiter -----------------
                # Content-Length is authoritative  -  body already extracted.
                # Just find and skip the delimiter between parts.
                if buffer.startswith(final_delimiter):
                    return  # Clean finish
                if buffer.startswith(final_delimiter_alt):
                    return  # Clean finish (CRLF outside Content-Length)

                if buffer.startswith(next_delimiter):
                    buffer = buffer[len_next_delimiter:]
                    continue  # Next part
                if buffer.startswith(next_delimiter_alt):
                    buffer = buffer[len_next_delimiter_alt:]
                    continue  # Next part (CRLF outside Content-Length)

                # Delimiter not in buffer yet  -  wait for next HTTP chunk
                break

        # -- Flush: stream ended without final delimiter ----------------
        # W3: with a spill still active the oversized part never completed -
        # never yield a truncated head, let the finally drop the file.
        if preamble_skipped and len(buffer) > 0 and spill_file is None:
            # Try to extract one last part from remaining buffer
            header_end = buffer.find(b"\r\n\r\n")
            if header_end != -1:
                headers_bytes = buffer[:header_end]
                body_start = header_end + 4
                content_length = _parse_content_length(headers_bytes)
                part_ts = _parse_part_transfer_syntax(headers_bytes)
                if (
                    content_length is not None
                    and len(buffer) >= body_start + content_length
                ):
                    dicom_body = buffer[body_start : body_start + content_length]
                    if _is_valid_dicom(dicom_body):
                        yield (dicom_body, part_index, part_ts)
                elif content_length is None:
                    # Fallback: use whatever is left as body (no Content-Length header).
                    # Some non-standard DICOMweb implementations omit Content-Length.
                    body = buffer[body_start:].rstrip(b"\r\n")
                    if body and _is_valid_dicom(body):
                        yield (body, part_index, part_ts)
    finally:
        # W3: never leak a spill file - a truncated stream, a fatal parse
        # error or an early consumer exit all land here. A completed part
        # resets spill_file before it is yielded, so only the part that was
        # still being received is cleaned up here.
        if spill_file is not None:
            try:
                spill_file.close()
            except OSError:
                pass
            try:
                os.unlink(spill_path)
            except OSError:
                pass
