# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""1013 rabbit pass 6, B3 - the QA port was checked when the QA AE never starts.

`_init_dimse_ae` starts the QA listener only when BOTH `expose_port` and
`qa_url` are set; with `qa_url` empty it logs a warning and binds nothing. The
startup probe still ran, so a port held by an unrelated process aborted
startup with `[FATAL] ... Refusing to start` for a listener this configuration
would never create.
"""

import ast
import inspect
import os

from clean_code import main as main_mod


def _args(**kw):
    base = {"expose_port": None, "qa_url": "", "bridge_host": "127.0.0.1"}
    base.update(kw)
    return type("A", (), base)()


class TestB3QaPortWillBind:
    def test_both_set_means_it_binds(self):
        assert main_mod._qa_port_will_bind(
            _args(expose_port=11113, qa_url="http://x/qa")
        )

    def test_port_without_url_does_not_bind(self):
        """The regression: this configuration starts no QA listener, so the
        probe must not run either."""
        assert not main_mod._qa_port_will_bind(_args(expose_port=11113, qa_url=""))

    def test_url_without_port_does_not_bind(self):
        assert not main_mod._qa_port_will_bind(
            _args(expose_port=None, qa_url="http://x/qa")
        )

    def test_neither_set_does_not_bind(self):
        assert not main_mod._qa_port_will_bind(_args())

    def test_the_predicate_matches_the_ae_start_condition(self):
        """Guard against drift: `_init_dimse_ae` starts the QA listener under
        `if args.expose_port and args.qa_url`, so the predicate must be that
        exact conjunction."""
        src = inspect.getsource(main_mod._init_dimse_ae)
        assert (
            "if args.expose_port and args.qa_url:" in src
        ), "the AE start condition changed - update _qa_port_will_bind"


class TestB3ProbeCallSite:
    def test_the_probe_is_guarded_by_the_predicate(self):
        """A predicate nobody calls is vacuous. The `DIMSE QA` probe must sit
        inside a branch whose test is `_qa_port_will_bind(args)`."""
        src = inspect.getsource(main_mod.main)
        tree = ast.parse(_dedent(src))

        def _is_qa_probe(node):
            if not isinstance(node, ast.Call):
                return False
            name = getattr(node.func, "id", None) or getattr(node.func, "attr", None)
            if name != "_check_port_available":
                return False
            return any(
                isinstance(a, ast.Constant) and a.value == "DIMSE QA" for a in node.args
            )

        guarded = False
        for node in ast.walk(tree):
            if not isinstance(node, ast.If):
                continue
            test = ast.dump(node.test)
            if "_qa_port_will_bind" not in test:
                continue
            if any(_is_qa_probe(n) for n in ast.walk(node)):
                guarded = True
        assert (
            guarded
        ), "the 'DIMSE QA' port probe is not inside a _qa_port_will_bind branch"

    def test_the_probe_still_precedes_component_initialisation(self):
        """The existing ordering guarantee (a second instance must not reset
        live C-MOVE jobs before discovering a taken port) is unaffected."""
        src = inspect.getsource(main_mod.main)
        idx_check = src.find('"DIMSE QA"')
        idx_init = src.find("_init_components(args)")
        assert idx_check != -1 and idx_init != -1
        assert idx_check < idx_init


def _dedent(src):
    import textwrap

    return textwrap.dedent(src)


def test_module_imports_cleanly():
    assert os.path.basename(inspect.getfile(main_mod)) == "main.py"
