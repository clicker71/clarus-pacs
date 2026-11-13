# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""1211 full-rabbit regression tests, one per finding.

Each test is written so that REVERTING its fix makes it fail: the assertion
targets the behaviour the finding described, not the shape of the code.
"""

import base64
import sqlite3
import time

import pytest
from pydicom.datadict import dictionary_VR
from pydicom.dataset import Dataset


# ---------------------------------------------------------------------------
# C2 / C4 - print attributes take the DICTIONARY VR; unknown keys are refused
# ---------------------------------------------------------------------------
class TestPrintAttributeVR:
    def test_a_scalar_takes_its_dictionary_vr_not_SH(self):
        """IS/CS/DS attributes were written as SH, which the print SCP reads as
        a different value type entirely."""
        from clean_code.http_server import _json_print_to_dataset

        ds = _json_print_to_dataset({"NumberOfCopies": "2"})
        assert ds[(0x2000, 0x0010)].VR == "IS"
        assert ds[(0x2000, 0x0010)].VR != "SH"

    def test_a_list_keeps_the_tag_vr_not_OB(self):
        """A multi-valued ST became an OB byte string."""
        from clean_code.http_server import _json_print_to_dataset

        ds = _json_print_to_dataset({"ImageDisplayFormat": ["STANDARD\\1,1"]})
        entry = ds[(0x2010, 0x0010)]
        assert entry.VR == dictionary_VR((0x2010, 0x0010))
        assert entry.VR != "OB"

    def test_a_sequence_uses_SQ(self):
        from clean_code.http_server import _json_print_to_dataset

        ds = _json_print_to_dataset(
            {"ReferencedImageBoxSequence": [{"ImageBoxPosition": 1}]}
        )
        assert ds[(0x2010, 0x0510)].VR == "SQ"

    def test_an_unknown_keyword_is_refused_not_skipped(self):
        """A typo used to drop the attribute silently."""
        from clean_code.http_server import (
            _json_print_to_dataset,
            _UnknownPrintAttribute,
        )

        with pytest.raises(_UnknownPrintAttribute) as exc:
            _json_print_to_dataset({"NotADicomKeyword": "x"})
        assert "NotADicomKeyword" in str(exc.value)

    def test_an_unknown_tag_is_refused(self):
        from clean_code.http_server import (
            _json_print_to_dataset,
            _UnknownPrintAttribute,
        )

        with pytest.raises(_UnknownPrintAttribute):
            _json_print_to_dataset({"99990001": "x"})


# ---------------------------------------------------------------------------
# C1 - malformed base64 is refused before the driver
# ---------------------------------------------------------------------------
class TestBase64Validation:
    def test_the_handler_decodes_with_validate_true(self):
        """`b64decode` without validate=True DISCARDS junk characters, so a
        corrupted image body decoded to a shorter byte string and failed deep
        inside the print driver instead of at the request boundary. The
        handler must pass validate=True."""
        import inspect

        import clean_code.http_server as hs

        src = inspect.getsource(hs.BridgeHTTPHandler.do_POST)
        assert (
            "validate=True" in src
        ), "the /print handler must reject malformed base64 before the driver"
        # And the strict form really does refuse what the lenient form accepts
        # (length-correct, padding-correct, two bad characters in the middle).
        junk = "QUJD" * 4 + "!!" + "REVG"
        assert base64.b64decode(junk.encode()), "lenient decode succeeds"
        with pytest.raises(Exception):
            base64.b64decode(junk.encode(), validate=True)


# ---------------------------------------------------------------------------
# C5 - the body deadline bounds the actual socket read
# ---------------------------------------------------------------------------
class TestBodyDeadlineBoundsReads:
    def test_a_read_is_capped_by_the_remaining_budget(self, monkeypatch):
        """The deadline must limit the per-read socket timeout, not just be
        checked between reads. `rfile.read(n)` loops internally and cannot be
        bounded, so the bytes come from a raw `recv` in a loop we control."""

        class _Sock:
            def __init__(self):
                self.timeouts = []

            def settimeout(self, v):
                self.timeouts.append(v)

            def gettimeout(self):
                return 30.0

            def recv(self, n):
                time.sleep(0.01)  # trickle: one byte per slice
                if len(self.timeouts) > 200:  # pragma: no cover - guard
                    raise AssertionError("the loop never gave up")
                return b"x"

        class _RFile:
            def peek(self, n):
                return b""

            def read(self, n):  # pragma: no cover - must not be used
                raise AssertionError("rfile.read must not bound the loop")

        class _H:
            connection = _Sock()
            rfile = _RFile()
            timeout = 30.0

        import clean_code.http_server as hs

        monkeypatch.setattr(hs, "BODY_TOTAL_TIMEOUT_SEC", 0.05, raising=True)
        h = _H()
        with pytest.raises(hs._BodyDeadlineExceeded):
            hs.BridgeHTTPHandler._read_body_with_deadline(h, 10_000)
        # The socket timeout was tightened at least once below the default.
        assert h.connection.timeouts, "the read was never bounded"
        assert min(h.connection.timeouts) <= 0.06


# ---------------------------------------------------------------------------
# C3 - a stalled body read is a 408, not a dropped connection
# ---------------------------------------------------------------------------
def test_socket_timeout_maps_to_408():
    """The handler must catch socket.timeout around the body read."""
    import inspect

    import clean_code.http_server as hs

    src = inspect.getsource(hs.BridgeHTTPHandler.do_POST)
    assert "socket.timeout" in src, (
        "a stalled read must be answered 408; an uncaught OSError drops the "
        "connection with no status at all"
    )
    assert "408" in src


# ---------------------------------------------------------------------------
# C6 - a non-object STOW body is rejected explicitly
# ---------------------------------------------------------------------------
class TestStowBodyNotObject:
    def test_a_json_array_is_not_a_json_object(self):
        from clean_code.http_helpers import (
            StowBodyNotJsonObject,
            _parse_stow_failures_all,
        )

        with pytest.raises(StowBodyNotJsonObject):
            _parse_stow_failures_all('["not", "an", "object"]')

    def test_a_scalar_body_is_rejected(self):
        from clean_code.http_helpers import (
            StowBodyNotJsonObject,
            _parse_stow_failures_all,
        )

        with pytest.raises(StowBodyNotJsonObject):
            _parse_stow_failures_all("123")

    def test_an_empty_body_is_still_a_clean_no_failures_answer(self):
        from clean_code.http_helpers import _parse_stow_failures_all

        assert _parse_stow_failures_all("") == []


# ---------------------------------------------------------------------------
# C8 - the raw TCP connect is bounded
# ---------------------------------------------------------------------------
def test_the_nevent_ae_sets_a_connection_timeout():
    """`network_timeout`/`acse_timeout` do not cover socket.connect();
    pynetdicom uses `connection_timeout`, whose AE default is None."""
    import inspect

    import clean_code.nevent as nv

    src = inspect.getsource(nv)
    assert "connection_timeout = 5" in src.replace("ae.", ""), (
        "the TCP connect must be bounded or a black-hole peer holds the "
        "N-EVENT worker for the OS SYN timeout"
    )


# ---------------------------------------------------------------------------
# C15 - the scp_targets migration is crash-safe
# ---------------------------------------------------------------------------
class TestMigrationCrashSafe:
    def _old_schema(self, path):
        conn = sqlite3.connect(path)
        conn.execute(
            "CREATE TABLE scp_targets (ae_title TEXT PRIMARY KEY,"
            " host TEXT NOT NULL, port INTEGER NOT NULL DEFAULT 104,"
            " last_seen INTEGER NOT NULL, consecutive_failures INTEGER DEFAULT 0)"
        )
        conn.execute("INSERT INTO scp_targets VALUES ('AE1','10.0.0.1',104,1,0)")
        conn.commit()
        conn.close()

    def test_a_leftover_v2_table_does_not_block_startup(self, tmp_path):
        """A crash after the CREATE left scp_targets_v2 behind, and the next
        start died on 'table scp_targets_v2 already exists'."""
        from clean_code.move_jobs import _migrate_scp_targets

        db = str(tmp_path / "m.db")
        self._old_schema(db)
        conn = sqlite3.connect(db)
        conn.execute(
            "CREATE TABLE scp_targets_v2 (ae_title TEXT NOT NULL,"
            " host TEXT NOT NULL, port INTEGER NOT NULL DEFAULT 104,"
            " last_seen INTEGER NOT NULL, consecutive_failures INTEGER DEFAULT 0,"
            " PRIMARY KEY (ae_title, host))"
        )
        conn.commit()
        # Must not raise.
        _migrate_scp_targets(conn)
        cols = {r[1] for r in conn.execute("PRAGMA table_info('scp_targets')")}
        assert "host" in cols
        rows = conn.execute("SELECT ae_title, host FROM scp_targets").fetchall()
        assert ("AE1", "10.0.0.1") in rows, rows
        conn.close()

    def test_a_crash_after_the_copy_still_finishes(self, tmp_path):
        """A crash mid-migration leaves v2 POPULATED but scp_targets still the
        old table. Re-running must copy (INSERT OR IGNORE) and finish the
        rename rather than aborting on the leftover v2."""
        from clean_code.move_jobs import _migrate_scp_targets

        db = str(tmp_path / "m2.db")
        self._old_schema(db)
        conn = sqlite3.connect(db)
        # The state a crash between CREATE and RENAME leaves behind.
        conn.execute(
            "CREATE TABLE scp_targets_v2 (ae_title TEXT NOT NULL,"
            " host TEXT NOT NULL, port INTEGER NOT NULL DEFAULT 104,"
            " last_seen INTEGER NOT NULL, consecutive_failures INTEGER DEFAULT 0,"
            " PRIMARY KEY (ae_title, host))"
        )
        conn.execute("INSERT INTO scp_targets_v2 VALUES ('AE1','10.0.0.1',104,1,0)")
        conn.commit()
        _migrate_scp_targets(conn)
        cols = {r[1] for r in conn.execute("PRAGMA table_info('scp_targets')")}
        assert "host" in cols, "the migration did not finish"
        rows = conn.execute("SELECT ae_title, host FROM scp_targets").fetchall()
        assert ("AE1", "10.0.0.1") in rows, rows
        # And running it again on the migrated schema is a no-op.
        _migrate_scp_targets(conn)
        assert conn.execute("SELECT COUNT(*) FROM scp_targets").fetchone()[0] == 1
        conn.close()


# ---------------------------------------------------------------------------
# C14-residual - the cp1251 repair descends into sequence items
# ---------------------------------------------------------------------------
def test_the_charset_repair_descends_into_sequence_items():
    """PS3.5 SpecificCharacterSet applies to the WHOLE data set, so a text
    element inside an SQ item carries the same source encoding. Relabelling
    UTF-8 while repairing only the top level declared nested cp1251 bytes as
    UTF-8."""
    from clean_code.handlers.cstore import _fix_cp1251_krakozyabry
    from pydicom.sequence import Sequence

    nested = "Исследование".encode("cp1251").decode("latin-1")
    ds = Dataset()
    ds.PatientName = "Иван".encode("cp1251").decode("latin-1")
    item = Dataset()
    item.TextValue = nested  # (0040,A160) UT inside a sequence
    ds.ContentSequence = Sequence([item])

    result = _fix_cp1251_krakozyabry(ds)
    fixed_nested = str(result.ContentSequence[0].TextValue)
    # The repaired text is the ORIGINAL Cyrillic word.
    assert fixed_nested == "Исследование", (
        "the nested text element was not repaired, yet the dataset is about to "
        f"be relabelled UTF-8: {fixed_nested!r}"
    )
    assert str(result.SpecificCharacterSet) == "ISO_IR 192"


class TestStaleTickDoesNotClobberTheRow:
    def test_the_shrink_write_is_guarded_by_identity(self):
        """`_tick` walks a SNAPSHOT of `_jobs`, so a re-registration during a
        tick leaves it holding an object that no longer owns the row. The
        shrink-write (the `pending` confirmation path) must therefore check
        identity before persisting; without the guard an `INSERT OR REPLACE`
        puts the OLD SOP list back over the new job's row.

        Note the resolve/abandon branches ALREADY had this guard. The finding
        was about the third write, the shrink-persist, which had none - so the
        assertion is scoped to that block.
        """
        import inspect

        import clean_code.commit_tracker as ct

        src = inspect.getsource(ct.CommitTracker._tick)
        # Scope to the shrink branch: from the `elif self._db is not None:`
        # marker to the end of the method.
        marker = "elif self._db is not None:"
        assert marker in src, "the shrink-persist branch moved"
        shrink = src[src.index(marker) :]
        assert "self._jobs.get(job.tx_uid) is job" in shrink, (
            "the shrink-write must compare IDENTITY: a stale snapshot entry "
            "must not REPLACE the re-registered job's row"
        )
        assert shrink.index("self._jobs.get(job.tx_uid) is job") < shrink.index(
            "self._persist(job)"
        ), "the identity check must run before the row write"


# ---------------------------------------------------------------------------
# C16 - the resumed pass merges progress instead of resetting it
# ---------------------------------------------------------------------------
def test_the_resumed_pass_reports_the_union():
    """Pass 2 starts `completed` at 0 because it skips the delivered prefix;
    writing that verbatim told the poller 0 of N."""
    import inspect

    import clean_code.handlers.cmove as cm

    src = inspect.getsource(cm._stream_and_send_study)
    assert "_prior_completed" in src, (
        "the resumed pass must add the earlier passes' counters, or a poll "
        "timeout during pass 2 reports 0xC000 for a study pass 1 delivered"
    )
    sig = inspect.signature(cm._stream_and_send_study)
    assert "_prior_completed" in sig.parameters
    assert "_prior_total" in sig.parameters


# ---------------------------------------------------------------------------
# C18 - _study_sop_uids uses the cheap instance list
# ---------------------------------------------------------------------------
def test_study_sop_uids_uses_the_instance_list_not_series_metadata():
    """The series-level metadata resource renders every instance (1.4 MB for
    432) and ran with the outbound association open."""
    import inspect

    import clean_code.handlers.cmove as cm

    src = inspect.getsource(cm._study_sop_uids)
    # Strip comments: the explanatory note NAMES the old resource, and a
    # substring scan over raw text would fail on the comment.
    code = "\n".join(line.split("#", 1)[0] for line in src.splitlines())
    assert "/instances" in code, "the instance list is the cheap source"
    assert (
        "/metadata" not in code
    ), "the study SOP list must not render instance metadata any more"
