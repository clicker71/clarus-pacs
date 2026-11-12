# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Tests for handle_c_find — C-FIND handler: dispatch and error paths.

P31: Tests model dispatch (Patient Root, Study Root, MWL),
unknown model rejection, and shutdown rejection.
"""

from unittest.mock import patch

from clean_code.handlers.cfind import handle_c_find
from clean_code.protocol import MWL_UID, PATIENT_ROOT_UID, STUDY_ROOT_UID

from tests.conftest import (
    make_fake_find_event,
)

URL = "http://127.0.0.1:8024/dicomweb"


class TestHandleCFindDispatch:

    def test_patient_root_dispatches(self, mock_shutdown_not_set):
        """Patient Root model UID dispatches to QIDO studies search."""
        event = make_fake_find_event(model_uid=PATIENT_ROOT_UID)
        # Mock _qido_request to return empty list (no results, but no error)
        with patch("clean_code.handlers.cfind._qido_request") as mock_qido:
            mock_qido.return_value = []
            gen = handle_c_find(
                event,
                URL,
                False,
                max_responses=50,
            )
            results = list(gen)
            # Should yield success status (0x0000, None) with empty results
            assert len(results) == 1
            assert results[0] == (0x0000, None)

    def test_study_root_dispatches(self, mock_shutdown_not_set):
        """Study Root model UID dispatches to QIDO studies search."""
        event = make_fake_find_event(model_uid=STUDY_ROOT_UID)
        with patch("clean_code.handlers.cfind._qido_request") as mock_qido:
            mock_qido.return_value = []
            gen = handle_c_find(
                event,
                URL,
                False,
                max_responses=50,
            )
            results = list(gen)
            assert len(results) == 1
            assert results[0] == (0x0000, None)

    def test_mwl_dispatches(self, mock_shutdown_not_set):
        """MWL model UID dispatches to UPS workitems search."""
        event = make_fake_find_event(model_uid=MWL_UID)
        with patch("clean_code.handlers.cfind._qido_request") as mock_qido:
            mock_qido.return_value = []
            gen = handle_c_find(
                event,
                URL,
                False,
                max_responses=50,
            )
            results = list(gen)
            assert len(results) == 1
            assert results[0] == (0x0000, None)

    def test_unknown_model_returns_a900(self, mock_shutdown_not_set):
        """Unknown abstract syntax → 0xA900 (Data Set does not match SOP Class)."""
        event = make_fake_find_event(model_uid="9.9.9.9.9")
        gen = handle_c_find(
            event,
            URL,
            False,
            max_responses=50,
        )
        results = list(gen)
        assert len(results) == 1
        assert results[0] == (0xA900, None)

    def test_rejects_during_shutdown(self, mock_shutdown_set):
        """During shutdown, handle_c_find returns 0xC000 immediately."""
        event = make_fake_find_event()
        gen = handle_c_find(
            event,
            URL,
            False,
            max_responses=50,
        )
        results = list(gen)
        assert len(results) == 1
        assert results[0] == (0xC000, None)

    def test_qido_connection_error_yields_a700(self, mock_shutdown_not_set):
        """QIDO transport failure -> 0xA700 (transient, retryable), not 0xC000.

        A broken TCP link must NOT read as a permanent refusal: 0xC000 told
        the SCU the query failed for good, so it would never retry."""
        import httpx

        event = make_fake_find_event(model_uid=PATIENT_ROOT_UID)
        with patch("clean_code.handlers.cfind._qido_request") as mock_qido:
            mock_qido.side_effect = httpx.ConnectError("refused")
            gen = handle_c_find(
                event,
                URL,
                False,
                max_responses=50,
            )
            results = list(gen)
            assert len(results) == 1
            assert results[0] == (0xA700, None)
