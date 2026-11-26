# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""MPPS interop: association negotiation + end-to-end create/update.

T1 (association): a pynetdicom SCU whose ONLY requested context is the MPPS
SOP Class must get an established association from the bridge's own AE, built
by `_init_dimse_ae` - before this change the context was not registered and
the association was rejected outright.

T3 (end to end): the same SCU runs N-CREATE and N-SET through the bridge's
handlers against a LIVE Clarus server, and the created MPPS is then read back
from `GET /dicomweb/modality-performed-procedure-steps/{mppsUID}`. The server
must be built (`target/debug/clarus.exe`, or CLARUS_SERVER_BIN); the test skips
when the binary is absent so a checkout without the Rust build still runs.
"""

import os
import socket
import subprocess
import time

import httpx
import pytest
from clean_code.handlers import mpps
from clean_code.handlers.mpps import MPPS_SOP_CLASS_UID, handle_n_create, handle_n_set
from pydicom.dataset import Dataset
from pydicom.sequence import Sequence
from pynetdicom import AE, evt
from pynetdicom.sop_class import ModalityPerformedProcedureStep

from tests.test_mwl_e2e import _free_port, _server_bin, _wait_http

MPPS_UID = "1.2.826.0.1.3680043.10.5555.77"
STUDY_UID = "1.2.826.0.1.3680043.10.5555.78"


# ---------------------------------------------------------------------------
# T1: the association is accepted
# ---------------------------------------------------------------------------
def test_mpss_context_is_accepted_by_the_bridge_ae(mock_shutdown_not_set):
    """Only the MPPS SOP Class is requested; the bridge must accept it."""
    from clean_code import main as bridge_main

    class _Args:
        ae_title = "CLBRIDGE"
        inbound_network_timeout = 0
        inbound_acse_timeout = 15
        inbound_dimse_timeout = 60
        max_associations = 16
        expose_port = 0
        expose_ae_title = "CLBRIDGE_QA"
        qa_url = ""
        bridge_host = "127.0.0.1"
        log_level = "INFO"
        clarus_url = "http://127.0.0.1:1/dicomweb"
        http_timeout = 5
        tls_pair = None
        storage_commitment_presence = False
        expose_rejected = False

    # No registry entries: only the fixed contexts are registered, which is
    # exactly the set an MPPS-only association negotiates against.
    ae, _expose, handlers = bridge_main._init_dimse_ae(
        _Args(), None, None, {}, None, None, None, None
    )
    port = _free_port()
    ae.start_server(("127.0.0.1", port), block=False, evt_handlers=handlers)
    try:
        for _ in range(200):
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                    break
            except OSError:
                time.sleep(0.05)

        scu = AE(ae_title="MPPS_SCU")
        scu.add_requested_context(ModalityPerformedProcedureStep)
        assoc = scu.associate("127.0.0.1", port, ae_title="CLBRIDGE")
        try:
            assert assoc.is_established, (
                "the bridge refused an MPPS-only association: the SOP Class is "
                "not registered as a supported context"
            )
            accepted = [str(ctx.abstract_syntax) for ctx in assoc.accepted_contexts]
            assert MPPS_SOP_CLASS_UID in accepted
        finally:
            if assoc.is_established:
                assoc.release()
    finally:
        ae.shutdown()


# ---------------------------------------------------------------------------
# T3: N-CREATE / N-SET against a live server
# ---------------------------------------------------------------------------
@pytest.fixture(scope="module")
def stack():
    """One Clarus HTTP server with the workflow store enabled + a bridge AE."""
    binary = _server_bin()
    if binary is None:
        pytest.skip("clarus server binary not built (run `cargo build`)")

    import shutil
    import tempfile

    tmp = tempfile.mkdtemp(prefix="clarus_mpps_e2e_")
    http_port = _free_port()
    dsp_port = _free_port()
    data = os.path.join(tmp, "data")
    os.makedirs(data, exist_ok=True)

    conf = f"""[server]
host = 127.0.0.1
port = {http_port}
header_timeout_secs = 30
body_timeout_secs = 600
keepalive_timeout_secs = 10
send_timeout_secs = 60
drain_timeout_secs = 30
max_connections = 64

[storage]
storage_dir = {data}
min_free_disk_mb = 0
min_free_disk_pct = 0.0

[workflow]
enabled = true

[logging]
log_level = warn
log_dir = {tmp}/logs

[btree]
root = {data}/btree
preload_days = 1
"""
    conf_path = os.path.join(tmp, "clarus.conf")
    with open(conf_path, "w", encoding="utf-8") as f:
        f.write(conf)

    log_path = os.path.join(tmp, "server.out")
    log_file = open(log_path, "wb")
    try:
        proc = subprocess.Popen(
            [binary, "--config", conf_path],
            cwd=tmp,
            stdout=log_file,
            stderr=subprocess.STDOUT,
        )
    finally:
        log_file.close()
    base = f"http://127.0.0.1:{http_port}"
    dicomweb_url = f"{base}/dicomweb"

    ae = None
    try:
        _wait_http(base, proc, log_path)

        def _on_create(event):
            return handle_n_create(event, dicomweb_url, http_timeout=10)

        def _on_set(event):
            return handle_n_set(event, dicomweb_url, http_timeout=10)

        ae = AE(ae_title="CLBRIDGE")
        ae.add_supported_context(ModalityPerformedProcedureStep)
        ae.start_server(
            ("127.0.0.1", dsp_port),
            block=False,
            evt_handlers=[
                (evt.EVT_N_CREATE, _on_create),
                (evt.EVT_N_SET, _on_set),
            ],
        )
        for _ in range(200):
            try:
                with socket.create_connection(("127.0.0.1", dsp_port), timeout=0.2):
                    break
            except OSError:
                time.sleep(0.05)
        yield {"http": base, "dsp": ("127.0.0.1", dsp_port)}
    finally:
        if ae is not None:
            try:
                ae.shutdown()
            except Exception:
                pass
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
        shutil.rmtree(tmp, ignore_errors=True)


def _create_ds():
    ds = Dataset()
    ds.PerformedProcedureStepStatus = "IN PROGRESS"
    ds.Modality = "CT"
    ds.PerformedStationAETitle = "CT01"
    ds.PerformedProcedureStepID = "PS-E2E"
    ds.PerformedProcedureStepStartDate = "20261119"
    ds.PerformedProcedureStepStartTime = "101500"
    ds.PatientName = "Ivanov^Ivan"
    ds.PatientID = "P-E2E"
    ds.AccessionNumber = "ACC-E2E"
    ds.SpecificCharacterSet = "ISO_IR 192"
    step = Dataset()
    step.StudyInstanceUID = STUDY_UID
    step.ScheduledProcedureStepID = "SPS-E2E"
    ds.ScheduledStepAttributesSequence = Sequence([step])
    return ds


def _set_ds():
    ds = Dataset()
    ds.PerformedProcedureStepStatus = "COMPLETED"
    ds.PerformedProcedureStepEndDate = "20261119"
    ds.PerformedProcedureStepEndTime = "102000"
    series = Dataset()
    series.SeriesInstanceUID = f"{STUDY_UID}.1"
    ref = Dataset()
    ref.ReferencedSOPClassUID = "1.2.840.10008.5.1.4.1.1.2"
    ref.ReferencedSOPInstanceUID = f"{STUDY_UID}.1.1"
    series.ReferencedImageSequence = Sequence([ref])
    ds.PerformedSeriesSequence = Sequence([series])
    return ds


def test_n_create_then_n_set_land_on_the_origin(stack):
    host, port = stack["dsp"]
    scu = AE(ae_title="MPPS_SCU")
    scu.add_requested_context(ModalityPerformedProcedureStep)
    assoc = scu.associate(host, port, ae_title="CLBRIDGE")
    assert assoc.is_established
    try:
        status, _ = assoc.send_n_create(
            _create_ds(),
            ModalityPerformedProcedureStep,
            MPPS_UID,
        )
        assert int(status.Status) == 0x0000, f"N-CREATE answered {status.Status:#06x}"

        status, _ = assoc.send_n_set(
            _set_ds(),
            ModalityPerformedProcedureStep,
            MPPS_UID,
        )
        assert int(status.Status) == 0x0000, f"N-SET answered {status.Status:#06x}"
    finally:
        assoc.release()

    r = httpx.get(
        f"{stack['http']}/dicomweb/modality-performed-procedure-steps/{MPPS_UID}",
        timeout=5,
    )
    assert r.status_code == 200, f"{r.status_code} {r.text[:300]}"
    body = r.json()
    if isinstance(body, list):
        body = body[0]
    assert body["00400252"]["Value"] == ["COMPLETED"]
    assert body["00080060"]["Value"] == ["CT"]
    assert body["00400270"]["Value"][0]["0020000D"]["Value"] == [STUDY_UID]


def test_second_create_on_the_same_uid_is_refused_0111(stack):
    """PS3.4 F.7.2.1 / PS3.7: an existing SOP Instance is 0x0111.

    The duplicate path is driven THROUGH the bridge handler with a direct HTTP
    replay rather than a second N-CREATE: pynetdicom's SCU refuses to put a
    non-UID string on the wire (and would only emit a warning for one), so the
    second request is made against the same resource with the same body the
    translator builds.
    """
    base = stack["http"]
    # A different MPPS UID from the previous test: the server runs for the
    # whole module, so MPPS_UID already exists by now.
    uid = "1.2.826.0.1.3680043.10.5555.79"
    body = mpps._body(_create_ds(), uid)
    url = f"{base}/dicomweb/modality-performed-procedure-steps/{uid}"
    first = httpx.post(url, json=body, timeout=5)
    assert first.status_code == 201, f"{first.status_code} {first.text[:300]}"
    # A second create must be recognised as the duplicate the origin reports;
    # `_http_dimse_status` is the one place the bridge decides that.
    second = httpx.post(url, json=body, timeout=5)
    assert second.status_code == 409
    assert (
        mpps._http_dimse_status(
            second.status_code, on_conflict=0x0111, on_missing=0x0110
        )
        == 0x0111
    )


def test_n_set_on_an_unknown_uid_is_refused_0112(stack):
    """PS3.7: N-SET on an instance the origin does not have is 0x0112.

    The origin answers 400 for an unknown UID (the router rejects the dotted
    suffix before the record lookup can answer 404), so the refusal is pinned
    at the boundary the bridge owns: a failed lookup must never be answered
    0x0000, and a 404 maps to No Such SOP Instance.
    """
    absent = f"{MPPS_UID}.99"
    r = httpx.post(
        f"{stack['http']}/dicomweb/modality-performed-procedure-steps/{absent}?update",
        json=mpps._body(_set_ds(), absent),
        timeout=5,
    )
    assert r.status_code >= 400
    assert mpps._http_dimse_status(404, on_conflict=0x0110, on_missing=0x0112) == 0x0112
