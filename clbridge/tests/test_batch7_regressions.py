# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Batch-7 regressions: masking key publication, HTTP bridge input, C-FIND
metrics, protocol mapping, audit trail, bundle order and the lifecycle order.

Each test pins a defect that was fixed in the same change, so reverting the
fix turns the test red.
"""

import ast
import io
import json
import os
import shutil
import tempfile
import threading
from unittest.mock import MagicMock

from pydicom import Dataset
from pydicom.multival import MultiValue

# ---------------------------------------------------------------------------
# T1 - the masking key is never published empty
# ---------------------------------------------------------------------------


class TestMaskKeyPublication:
    def test_a_loser_never_adopts_an_empty_file(self):
        """A key file that holds no bytes must be REPAIRED, not adopted.

        A create-then-write publishes the name before the bytes, so a reader
        between the two sees an empty file. An empty HMAC key is an unkeyed
        digest of a low-entropy patient id, reversible by enumeration, so the
        load path checks the length and publishes a real key instead.
        """
        import clean_code.utils as utils

        root = tempfile.mkdtemp(prefix="clbridge-mask-race-")
        try:
            key_path = os.path.join(root, ".patient_id_mask.key")
            with open(key_path, "wb"):
                pass

            utils.set_mask_key_dir(root)
            utils.mask_patient_id("12345")

            with open(key_path, "rb") as handle:
                on_disk = handle.read()
            assert len(on_disk) == utils._MASK_KEY_BYTES, (
                "an empty key file must be repaired, not adopted: got "
                f"{len(on_disk)} bytes"
            )
        finally:
            utils.set_mask_key_dir("")
            shutil.rmtree(root, ignore_errors=True)

    def test_the_loser_of_the_create_reads_a_complete_key(self):
        """Direct unit on the loss path: `O_EXCL` fails, and what is read back
        must pass the length check rather than be cached as-is.

        The create is forced to lose by making the file appear between the
        pre-read and the create - the exact window the defect lived in.
        """
        import clean_code.utils as utils

        root = tempfile.mkdtemp(prefix="clbridge-mask-loser-")
        key_path = os.path.join(root, ".patient_id_mask.key")
        real_open = utils.os.open

        def _open(path, flags, *a, **k):
            if path.endswith(".patient_id_mask.key") and flags & os.O_EXCL:
                # The winner published an EMPTY file at this instant.
                with open(path, "wb"):
                    pass
            return real_open(path, flags, *a, **k)

        try:
            utils.set_mask_key_dir(root)
            utils.os.open = _open
            try:
                utils.mask_patient_id("12345")
            finally:
                utils.os.open = real_open

            with open(key_path, "rb") as handle:
                on_disk = handle.read()
            assert len(on_disk) >= utils._MASK_KEY_BYTES, (
                "the loser adopted a " f"{len(on_disk)}-byte key"
            )
        finally:
            utils.set_mask_key_dir("")
            shutil.rmtree(root, ignore_errors=True)

    def test_a_published_key_is_never_shorter_than_the_contract(self):
        import clean_code.utils as utils

        root = tempfile.mkdtemp(prefix="clbridge-mask-len-")
        try:
            utils.set_mask_key_dir(root)
            utils.mask_patient_id("12345")
            with open(os.path.join(root, ".patient_id_mask.key"), "rb") as handle:
                assert len(handle.read()) == 32
        finally:
            utils.set_mask_key_dir("")
            shutil.rmtree(root, ignore_errors=True)

    def test_concurrent_threads_agree_on_one_key(self):
        """Two threads in one process must not each publish their own key;
        that would split the log tokens of a single install in two."""
        import clean_code.utils as utils

        root = tempfile.mkdtemp(prefix="clbridge-mask-threads-")
        tokens = []
        try:
            utils.set_mask_key_dir(root)
            barrier = threading.Barrier(4)

            def worker():
                barrier.wait()
                tokens.append(utils.mask_patient_id("12345"))

            threads = [threading.Thread(target=worker) for _ in range(4)]
            for t in threads:
                t.start()
            for t in threads:
                t.join()
            assert (
                len(set(tokens)) == 1
            ), f"the key was published more than once: {tokens}"
        finally:
            utils.set_mask_key_dir("")
            shutil.rmtree(root, ignore_errors=True)


# ---------------------------------------------------------------------------
# m7 - the Implicit VR fallback is bound at module level
# ---------------------------------------------------------------------------


class TestImplicitVrFallback:
    def test_a_dataset_with_file_meta_but_no_transfer_syntax_measures(self):
        """The fallback TS is used OUTSIDE the `if not file_meta` block, so a
        local import left it unbound and raised NameError for exactly the
        dataset the fallback exists for."""
        from clean_code.utils import _measure_dataset_bytes

        ds = Dataset()
        ds.SOPClassUID = "1.2.840.10008.5.1.4.1.1.7"
        ds.SOPInstanceUID = "1.2.3.4"
        ds.PatientName = "TEST"
        ds.file_meta = Dataset()
        ds.file_meta.MediaStorageSOPClassUID = ds.SOPClassUID
        ds.file_meta.MediaStorageSOPInstanceUID = ds.SOPInstanceUID
        size = _measure_dataset_bytes(ds)
        assert size > 0


# ---------------------------------------------------------------------------
# m4 - the /print handler answers 400 instead of raising out of the thread
# ---------------------------------------------------------------------------


class TestPrintBodyValidation:
    def _handler(self, headers, body):
        from clean_code.http_server import BridgeHTTPHandler

        handler = BridgeHTTPHandler.__new__(BridgeHTTPHandler)
        handler.path = "/print"
        handler.headers = headers
        handler.rfile = io.BytesIO(body)
        handler.wfile = io.BytesIO()
        handler.send_error = MagicMock()
        handler.send_response = MagicMock()
        handler.send_header = MagicMock()
        handler.end_headers = MagicMock()
        handler.printer_registry = {}
        handler.driver_registry = {}
        handler.bridge_ae_title = "TEST"
        return handler

    def test_a_non_numeric_content_length_is_400(self):
        handler = self._handler(
            {"Content-Type": "application/json", "Content-Length": "abc"}, b"{}"
        )
        handler.do_POST()
        payload = json.loads(handler.wfile.getvalue() or b"{}")
        assert handler.send_response.call_args[0][0] == 400
        assert payload.get("error") == "Invalid Content-Length"

    def test_a_negative_content_length_is_400_and_does_not_read(self):
        handler = self._handler(
            {"Content-Type": "application/json", "Content-Length": "-1"}, b""
        )
        handler.do_POST()
        assert handler.send_response.call_args[0][0] == 400
        assert handler.rfile.tell() == 0, "a negative length must not start a read"

    def test_a_json_array_is_400(self):
        body = b"[1, 2, 3]"
        handler = self._handler(
            {"Content-Type": "application/json", "Content-Length": str(len(body))},
            body,
        )
        handler.do_POST()
        payload = json.loads(handler.wfile.getvalue() or b"{}")
        assert handler.send_response.call_args[0][0] == 400
        assert payload.get("error") == "Expected a JSON object"


# ---------------------------------------------------------------------------
# m6 - a multi-valued matching key becomes a comma-separated list
# ---------------------------------------------------------------------------


class TestMultiValuedQuery:
    def test_modalities_in_study_are_comma_separated(self):
        from clean_code.protocol import _build_qido_params

        ds = Dataset()
        ds.ModalitiesInStudy = MultiValue(str, ["CT", "MR"])
        params = _build_qido_params(ds)
        assert "CT,MR" in params.values(), (
            "a MultiValue must not reach the server as \"['CT', 'MR']\": " f"{params}"
        )


# ---------------------------------------------------------------------------
# m8 - one study UID, one precedence
# ---------------------------------------------------------------------------


class TestMwlStudyUidPrecedence:
    def test_the_sequence_wins_over_the_bare_tag(self):
        """The RP ID is derived from the same function, so a bare tag that
        disagrees must not become the StudyInstanceUID of the row."""
        from clean_code.protocol import _json_to_dataset_mwl

        workitem = {
            "0020000D": {"vr": "UI", "Value": ["1.2.3.BARE"]},
            "0040A370": {
                "vr": "SQ",
                "Value": [{"0020000D": {"vr": "UI", "Value": ["1.2.3.SEQ"]}}],
            },
            "00100010": {"vr": "PN", "Value": [{"Alphabetic": "TEST^PATIENT"}]},
            "00100020": {"vr": "LO", "Value": ["P1"]},
        }
        ds = _json_to_dataset_mwl(
            workitem, {"StudyInstanceUID", "RequestedProcedureID"}
        )
        assert (
            ds.StudyInstanceUID == "1.2.3.SEQ"
        ), "the Referenced Request Sequence is the standard carriage"

    def test_an_empty_bare_tag_does_not_drop_the_study(self):
        from clean_code.protocol import _json_to_dataset_mwl

        workitem = {
            "0020000D": {"vr": "UI", "Value": []},
            "0040A370": {
                "vr": "SQ",
                "Value": [{"0020000D": {"vr": "UI", "Value": ["1.2.3.SEQ"]}}],
            },
            "00100010": {"vr": "PN", "Value": [{"Alphabetic": "TEST^PATIENT"}]},
            "00100020": {"vr": "LO", "Value": ["P1"]},
        }
        ds = _json_to_dataset_mwl(workitem, {"StudyInstanceUID"})
        assert ds.StudyInstanceUID == "1.2.3.SEQ"


# ---------------------------------------------------------------------------
# m9 - the audit trail records the real original value
# ---------------------------------------------------------------------------


class TestAuditOriginalValue:
    def test_an_absent_patient_id_is_audited_as_empty(self):
        from clean_code.validation import sanitize_dataset

        ds = Dataset()
        sanitize_dataset(ds, write_audit=True, modifying_system="CLBRIDGE")
        item = ds.OriginalAttributesSequence[0].ModifiedAttributesSequence[0]
        recorded = item[(0x0010, 0x0020)].value
        assert recorded == "", (
            "the Modified Attributes Sequence must record the value the "
            f"attribute actually held, got {recorded!r}"
        )


# ---------------------------------------------------------------------------
# m10 - C-FIND counters are incremented before the terminal status
# ---------------------------------------------------------------------------


class TestFindResponseCounter:
    def test_the_increment_precedes_the_terminal_status(self):
        """The SCP stops iterating a generator at the first non-Pending status,
        so an increment placed after that yield never runs and the metric
        always reports zero. The assertion is structural because the failure
        mode is structural: no count reaches the counter at all."""
        source = open(
            os.path.join(
                os.path.dirname(__file__), "..", "clean_code", "handlers", "cfind.py"
            ),
            encoding="utf-8",
        ).read()
        tree = ast.parse(source)
        handler = next(
            n
            for n in ast.walk(tree)
            if isinstance(n, ast.FunctionDef) and n.name == "_handle_qido_search"
        )
        inc_line = None
        terminal_line = None
        for node in ast.walk(handler):
            if isinstance(node, ast.Expr) and isinstance(node.value, ast.Call):
                func = ast.dump(node.value.func)
                if "find_responses" in func and "inc" in func:
                    inc_line = node.lineno
            # A terminal status is yielded as a TUPLE whose first element is
            # the status code; the reactor's protocol is `yield <count>` for
            # the progress case, so only the tuple shape is terminal.
            if isinstance(node, ast.Yield) and isinstance(node.value, ast.Tuple):
                first = node.value.elts[0] if node.value.elts else None
                code = getattr(first, "value", None)
                if code in (0, 0xB001):
                    if terminal_line is None or node.lineno < terminal_line:
                        terminal_line = node.lineno
        assert inc_line is not None, "no find_responses increment in the handler"
        assert terminal_line is not None, "no terminal status yield in the handler"
        assert inc_line < terminal_line, (
            "the counter must move before the terminal status "
            f"(increment at {inc_line}, status at {terminal_line})"
        )


# ---------------------------------------------------------------------------
# m11 - a real PatientID is masked in the cp1251 debug line
# ---------------------------------------------------------------------------


class TestCp1251PatientIdMasking:
    def test_the_log_line_does_not_carry_the_raw_id(self, caplog):
        import clean_code.handlers.cstore as cstore

        # The defect: a modality PatientID whose bytes are cp1251 but were
        # decoded as Latin-1. The fixer logs the old and the fixed value, so
        # the log must carry the keyed token, never the identifier itself.
        # A Cyrillic id is what reaches the repair branch at all - the
        # heuristic only accepts a result with real Cyrillic in it.
        garbled = "Петров-77".encode("cp1251").decode("latin-1")
        ds = Dataset()
        ds.PatientID = garbled
        ds.PatientName = "TEST"
        with caplog.at_level("DEBUG"):
            cstore._fix_cp1251_krakozyabry(ds)
        text = "\n".join(r.getMessage() for r in caplog.records)
        assert "[CP1251]" in text, f"the fixer logged nothing: {text!r}"
        assert str(ds.PatientID) not in text, f"the raw PatientID leaked: {text}"


# ---------------------------------------------------------------------------
# T6 - bundle order reads imports with ast
# ---------------------------------------------------------------------------


class TestBundleOrderImports:
    def test_a_module_import_creates_the_edge(self):
        from clean_code._bundle_order import find_imports, resolve_imports

        imports = find_imports("import clean_code.config\n")
        assert resolve_imports(imports, {"config"}) == {"config"}

    def test_a_from_package_import_creates_the_module_edge(self):
        from clean_code._bundle_order import find_imports, resolve_imports

        imports = find_imports("from clean_code import config\n")
        assert resolve_imports(imports, {"config"}) == {"config"}

    def test_a_commented_import_creates_no_edge(self):
        from clean_code._bundle_order import find_imports, resolve_imports

        imports = find_imports("# from clean_code.config import VERSION\n")
        assert resolve_imports(imports, {"config"}) == set()

    def test_a_string_mentioning_an_import_creates_no_edge(self):
        from clean_code._bundle_order import find_imports, resolve_imports

        imports = find_imports('DOC = "from clean_code.config import X"\n')
        assert resolve_imports(imports, {"config"}) == set()

    def test_the_real_tree_still_orders(self):
        from clean_code._bundle_order import compute_order

        order = compute_order(
            os.path.join(os.path.dirname(__file__), "..", "clean_code")
        )
        names = [name for name, _ in order]
        assert names.index("config") < names.index("main")


# ---------------------------------------------------------------------------
# T5 - duplicate-instance checks run before any component starts
# ---------------------------------------------------------------------------


class TestStartupCheckOrder:
    def test_the_port_checks_precede_component_initialisation(self):
        """`_init_components` resets in-progress C-MOVE jobs in the shared
        database. Running it first let a second instance steal the live one's
        jobs before discovering the ports were taken."""
        source = open(
            os.path.join(os.path.dirname(__file__), "..", "clean_code", "main.py"),
            encoding="utf-8",
        ).read()
        tree = ast.parse(source)
        main_fn = next(
            node
            for node in ast.walk(tree)
            if isinstance(node, ast.FunctionDef) and node.name == "main"
        )

        order = []
        for node in ast.walk(main_fn):
            if isinstance(node, ast.Call):
                name = getattr(node.func, "id", None) or getattr(
                    node.func, "attr", None
                )
                if name in (
                    "_check_port_available",
                    "_init_components",
                    "_acquire_pid_file",
                    "_check_dimse_unique",
                ):
                    order.append((node.lineno, name))
        order.sort()
        names = [name for _, name in order]
        assert names.index("_init_components") > max(
            i for i, n in enumerate(names) if n == "_check_port_available"
        ), f"port checks must run first: {names}"
        assert names.index("_acquire_pid_file") < names.index("_init_components")


# ---------------------------------------------------------------------------
# m14 - a terminal job row is never rewritten
# ---------------------------------------------------------------------------


class TestTerminalJobRow:
    def test_a_failed_row_is_not_turned_into_success(self):
        import sqlite3

        from clean_code.move_jobs import MoveJobManager

        class _Db:
            def __init__(self):
                self.conn = sqlite3.connect(":memory:", check_same_thread=False)

            def locked(self):
                conn = self.conn

                class _L:
                    def __enter__(self):
                        return conn

                    def __exit__(self, *a):
                        return False

                return _L()

            def close(self):
                self.conn.close()

        db = _Db()
        manager = MoveJobManager(shared_db=db)
        manager.register_peer("PEER", "127.0.0.1", 11112)
        job_id = manager.create_job("1.2.3", "PEER", "127.0.0.1", 11112)
        manager.finish_job(job_id, success=False, error_message="poll timeout")
        manager.finish_job(job_id, success=True)
        _, _, status = manager.get_job_progress(job_id)
        assert status == "failed", (
            "the viewer was already told the first verdict; the worker must "
            "not overwrite it"
        )
        manager.close()
        db.close()
