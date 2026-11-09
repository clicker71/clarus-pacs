# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Batch-7 C-MOVE byte-level regressions: implicit-VR UID scan and URL quoting."""

import ast
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import clean_code.handlers.cmove as cmove


def _implicit_dataset(entries):
    """An Implicit VR LE dataset written by pydicom itself.

    Hand-rolled bytes would not prove the encoding: the scan has to read what
    a real writer produces, which is exactly what the bridge receives for a
    stored Implicit VR instance.
    """
    from pydicom import Dataset
    from pydicom.filebase import DicomBytesIO
    from pydicom.filewriter import write_dataset

    ds = Dataset()
    ds.is_implicit_VR = True
    ds.is_little_endian = True
    for elem, value in entries:
        ds.add_new((0x0008, elem), "UI", value)

    fp = DicomBytesIO()
    fp.is_implicit_VR = True
    fp.is_little_endian = True
    write_dataset(fp, ds)
    return fp.getvalue()


class TestImplicitDatasetUidScan:
    def test_sop_uids_are_read_from_an_implicit_dataset(self):
        body = _implicit_dataset(
            [
                (0x0016, "1.2.840.10008.5.1.4.1.1.7"),
                (0x0018, "1.2.3.4.5.6.7"),
            ]
        )
        assert cmove._extract_dataset_ui(body, 0x0018, implicit_vr=True) == (
            "1.2.3.4.5.6.7"
        )
        # The explicit-VR read of the same bytes is the defect that was fixed:
        # it returns nothing, so the caller must not rely on it.
        assert not cmove._extract_dataset_ui(body, 0x0018, implicit_vr=False)

    def test_the_part_scan_resolves_the_syntax_before_the_uids(self):
        """The syntax decides how the dataset is read, so resolving it after
        the UID scans makes an Implicit VR instance unidentifiable."""
        source = open(
            os.path.join(
                os.path.dirname(__file__), "..", "clean_code", "handlers", "cmove.py"
            ),
            encoding="utf-8",
        ).read()
        tree = ast.parse(source)
        handler = next(
            n
            for n in ast.walk(tree)
            if isinstance(n, ast.FunctionDef) and n.name == "_stream_and_send_study"
        )
        syntax_line = None
        uid_line = None
        for node in ast.walk(handler):
            if isinstance(node, ast.Assign):
                targets = [getattr(t, "id", None) for t in node.targets]
                if "actual_ts" in targets and syntax_line is None:
                    syntax_line = node.lineno
                if "sop_uid" in targets and uid_line is None:
                    uid_line = node.lineno
        assert (
            syntax_line is not None and uid_line is not None
        ), f"syntax at {syntax_line}, uid at {uid_line}"
        assert syntax_line < uid_line, (
            "the transfer syntax must be resolved before the SOP UID scan "
            f"(syntax at {syntax_line}, UID at {uid_line})"
        )

    def test_the_syntax_reaches_the_uid_scans(self):
        """`actual_ts` on its own changes nothing: the value has to be passed
        into the scans as `implicit_vr`, or the byte walk still assumes
        explicit VR and returns no UID.

        Both shapes are accepted - an inline `implicit_vr=<comparison>` and a
        named local - but a constant is not: that is the defect.
        """
        source = open(
            os.path.join(
                os.path.dirname(__file__), "..", "clean_code", "handlers", "cmove.py"
            ),
            encoding="utf-8",
        ).read()
        tree = ast.parse(source)
        handler = next(
            n
            for n in ast.walk(tree)
            if isinstance(n, ast.FunctionDef) and n.name == "_stream_and_send_study"
        )
        # The per-part scan must pass the RESOLVED syntax: a Name bound to the
        # `actual_ts` comparison. An inline comparison at a call site reads the
        # syntax without having resolved it against the part header, and a
        # constant ignores it entirely - neither is accepted.
        syntax_names = {"actual_ts"}
        for node in ast.walk(handler):
            if isinstance(node, ast.Assign):
                targets = {
                    getattr(t, "id", None) for t in node.targets if hasattr(t, "id")
                }
                if isinstance(node.value, ast.Compare) and any(
                    getattr(c, "id", None) == "actual_ts"
                    for c in ([node.value.left] + list(node.value.comparators))
                ):
                    syntax_names |= targets

        honoured = []
        for node in ast.walk(handler):
            if isinstance(node, ast.Call):
                name = getattr(node.func, "id", None) or getattr(
                    node.func, "attr", None
                )
                if name != "_extract_dataset_ui":
                    continue
                # The refetch rescue resolves the syntax on its own line from
                # `actual_ts`; the per-part identity scan is the one that has
                # to carry the resolved local.
                if any(
                    isinstance(arg, ast.Constant) and arg.value == 0x000E
                    for arg in node.args
                ):
                    continue
                for kw in node.keywords:
                    if kw.arg != "implicit_vr":
                        continue
                    honoured.append(kw.value)
        assert honoured, "no _extract_dataset_ui call passes implicit_vr"
        assert all(
            isinstance(value, ast.Name) and value.id in syntax_names
            for value in honoured
        ), (
            "the per-part scan must pass the resolved transfer syntax; a "
            "constant or an inline comparison means an Implicit VR instance "
            "still yields no UID"
        )


class TestRefetchUrlQuoting:
    def test_every_uid_in_the_refetch_url_is_quoted(self):
        """`../x` or `1.2?a=b` in a UID would steer the authenticated GET at a
        different resource than the instance the peer asked for."""
        source = open(
            os.path.join(
                os.path.dirname(__file__), "..", "clean_code", "handlers", "cmove.py"
            ),
            encoding="utf-8",
        ).read()
        tree = ast.parse(source)
        target = None
        for node in ast.walk(tree):
            if isinstance(node, ast.Assign):
                targets = [getattr(t, "id", None) for t in node.targets]
                if "inst_url" in targets:
                    target = node
        assert target is not None, "the refetch URL assignment moved"
        dumped = ast.dump(target.value)
        assert dumped.count("_quote") >= 4, (
            "study, series, instance and transferSyntax must all be quoted: "
            f"{dumped}"
        )
