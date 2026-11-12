# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""F-009: end-to-end MWL (UPS-RS worklist) test.

Full stack exercised against a LIVE Clarus server + a real pynetdicom bridge AE:

    pynetdicom MWL SCU
        -- C-FIND{ScheduledStationAETitle=STATION01} -->
            in-process bridge AE (_handle_mwl)
                -- GET /dicomweb/workitems?ScheduledStationNameCodeSequence.CodeValue=STATION01 ->
                    Clarus server (CBOR workitem store)
        <-- Pending(SCHEDULED workitem) <-- Success -- back to SCU

The server must be built (target/debug/clarus.exe, or CLARUS_SERVER_BIN). The
test skips gracefully when the binary is absent so `pytest -q` never hard-fails
in an environment without the Rust build.
"""

import os
import shutil
import socket
import subprocess
import tempfile
import time

import httpx
import pytest

# Bridge's real MWL translation handler (the MWL loop already reads
# ScheduledStationAETitle -> GET /dicomweb/workitems?ScheduledStationNameCodeSequence.CodeValue=).
from clean_code.handlers.cfind import _handle_mwl
from pydicom.dataset import Dataset

pynetdicom = pytest.importorskip("pynetdicom")
from pynetdicom import AE, evt
from pynetdicom.sop_class import ModalityWorklistInformationFind

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

REPO_ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))


def _server_bin():
    override = os.environ.get("CLARUS_SERVER_BIN")
    candidates = (
        [override]
        if override
        else [
            os.path.join(REPO_ROOT, "target", "debug", "clarus.exe"),
            os.path.join(REPO_ROOT, "target", "release", "clarus.exe"),
            # A build on a host without the suffix (or a POSIX CI runner)
            # produces the same binary under the bare name.
            os.path.join(REPO_ROOT, "target", "debug", "clarus"),
            os.path.join(REPO_ROOT, "target", "release", "clarus"),
        ]
    )
    for c in candidates:
        if c and os.path.exists(c):
            return c
    return None


def _free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def _wait_http(base, proc, log_path, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc.poll() is not None:
            # The binary EXISTED (the caller checked) and exited anyway: that
            # is a server startup regression, not an absent prerequisite. A
            # skip here would let CI report full MWL coverage while the server
            # never started.
            try:
                with open(log_path, encoding="utf-8", errors="replace") as handle:
                    out = handle.read()
            except OSError:
                out = ""
            pytest.fail(f"clarus server exited early: {out[-1200:]}")
        try:
            httpx.get(f"{base}/health", timeout=0.5)
            return
        except Exception:
            time.sleep(0.2)
    pytest.fail("clarus server did not become ready in time")


# ---------------------------------------------------------------------------
# Module-scoped live server + bridge AE
# ---------------------------------------------------------------------------


@pytest.fixture(scope="module")
def stack():
    """Boot one Clarus HTTP server and one in-process bridge AE (MWL)."""
    binary = _server_bin()
    if binary is None:
        pytest.skip("clarus server binary not built (run `cargo build`)")

    tmp = tempfile.mkdtemp(prefix="clarus_mwl_e2e_")
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

    # Server output goes to a FILE, not a pipe: nothing drains a pipe after
    # startup, so a chatty server fills the 64 KiB buffer, blocks on write and
    # stops answering - a hang that looks like a slow server. The file also
    # survives the shutdown, which is what makes a failed start diagnosable.
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
        # The child holds its own descriptor; ours must not outlive the spawn.
        log_file.close()
    base = f"http://127.0.0.1:{http_port}"
    # Everything the fixture creates is inside the guard below: a server that
    # never becomes ready raises out of _wait_http, and without the guard the
    # child process and the temp directory survive the failed start.
    #
    # In-process bridge AE serving the MWL model, translating to the live
    # Clarus HTTP server at `base`. `_handle_mwl` takes the full DICOMweb
    # base URL (scheme + host + /dicomweb), not a host/port pair - see
    # clean_code/handlers/cfind.py:360 and the production call at cfind.py:101.
    dicomweb_url = f"{base}/dicomweb"
    ae = None

    def _on_cfind(event):
        yield from _handle_mwl(
            event,
            dicomweb_url,
            verbose=False,
            max_responses=50,
        )

    try:
        _wait_http(base, proc, log_path)

        ae = AE()
        ae.add_supported_context(ModalityWorklistInformationFind)
        handlers = [(evt.EVT_C_FIND, _on_cfind)]
        ae.start_server(("127.0.0.1", dsp_port), block=False, evt_handlers=handlers)

        # Give the AE time to bind.
        for _ in range(200):
            try:
                with socket.create_connection(("127.0.0.1", dsp_port), timeout=0.2):
                    break
            except OSError:
                time.sleep(0.05)

        yield {"http": base, "dsp": ("127.0.0.1", dsp_port), "tmp": tmp}
    finally:
        # `ae` is created inside the guard, so a failed _wait_http leaves it
        # None - the teardown must still stop the server and drop the temp dir.
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


# ---------------------------------------------------------------------------
# The MWL e2e scenario
# ---------------------------------------------------------------------------


def _post_workitem(base, uid, station):
    study_uid = f"{uid}.study"
    body = {
        "00741000": {"vr": "CS", "Value": ["SCHEDULED"]},
        "00080060": {"vr": "CS", "Value": ["CT"]},
        # The station is carried ONLY by (0040,4025) Scheduled Station Name
        # Code Sequence, whose Code Value is the AE Title (PS3.4 Table
        # CC.2.5-3; AIW-I 4.81.4.1.2.1 Table 4.81.4.1.2.1-3). The server
        # refuses the vendor (0040,0001), so a create that sent it would be
        # rejected rather than quietly stored without a station.
        "00404025": {
            "vr": "SQ",
            "Value": [
                {
                    "00080100": {"vr": "SH", "Value": [station]},
                    "00080102": {"vr": "SH", "Value": ["99CLARUS"]},
                    "00080104": {"vr": "LO", "Value": ["AI station"]},
                }
            ],
        },
        # The type-1 set of PS3.4 Table CC.2.5-3 at N-CREATE.
        "00741200": {"vr": "CS", "Value": ["HIGH"]},
        "00741204": {"vr": "LO", "Value": ["MWL E2E"]},
        "00404005": {"vr": "DT", "Value": ["20260802103000"]},
        "00404041": {"vr": "CS", "Value": ["READY"]},
        # The input study and the accession are carried by (0040,A370)
        # Referenced Request Sequence - the standard carriage, which the
        # server renders and the bridge reads. (0040,4021) Input Information
        # Sequence names the study too; both are accepted on Create.
        "0040A370": {
            "vr": "SQ",
            "Value": [
                {
                    "00080050": {"vr": "SH", "Value": ["ACC-MWL-E2E"]},
                    "0020000D": {"vr": "UI", "Value": [study_uid]},
                }
            ],
        },
        "00404021": {
            "vr": "SQ",
            "Value": [{"0020000D": {"vr": "UI", "Value": [study_uid]}}],
        },
    }
    # PS3.18 11.4.1.1 Table 11.4.1-1: Create carries the Workitem UID as the
    # `workitem` QUERY parameter. A path segment means the workitem resource,
    # which is Update on POST and Search on GET.
    r = httpx.post(f"{base}/dicomweb/workitems?workitem={uid}", json=body, timeout=5)
    assert r.status_code in (
        201,
        409,
    ), f"create workitem failed: {r.status_code} {r.text[:300]}"


def test_mwl_cfind_through_bridge_returns_scheduled(stack):
    """A pynetdicom MWL SCU receives a CONFORMANT row for its station.

    The SCU asks the way a real modality does (PS3.4 Table K.6-1): the matching
    key lives inside the single item of (0040,0100) Scheduled Procedure Step
    Sequence, and the Optional return keys are asked for by naming them.
    """
    # Seed two workitems: one for STATION01, one for a different scheduler so
    # the station filter is actually exercised (not a vacuous all-match).
    base = stack["http"]
    station_uid = "1.2.826.0.1.3680043.10.9999.1"
    other_uid = "1.2.826.0.1.3680043.10.9999.2"
    _post_workitem(base, station_uid, "STATION01")
    _post_workitem(base, other_uid, "OTHERSTATION")

    dsp_host, dsp_port = stack["dsp"]
    ae_scu = AE()
    ae_scu.add_requested_context(ModalityWorklistInformationFind)

    # The standard MWL identifier: (0040,0100) with ONE item carrying the
    # matching key plus the SPS return keys asked for by naming them.
    ds = Dataset()
    ds.QueryRetrieveLevel = "WORKLIST"
    ds.AccessionNumber = ""
    ds.RequestedProcedureID = ""
    sps_query = Dataset()
    sps_query.ScheduledStationAETitle = "STATION01"
    sps_query.ScheduledProcedureStepID = ""
    sps_query.ScheduledProcedureStepStartDate = ""
    sps_query.ScheduledProcedureStepStartTime = ""
    ds.ScheduledProcedureStepSequence = [sps_query]

    assoc = ae_scu.associate(dsp_host, dsp_port, ae_title="MWL_SCU")
    assert assoc.is_established, "failed to associate with the bridge AE"
    try:
        responses = assoc.send_c_find(ds, ModalityWorklistInformationFind)
        matches = []
        statuses = []
        for status, identifier in responses:
            statuses.append(0x0000 if status is None else int(status.Status))
            if status and identifier:
                matches.append(identifier)

        assert matches, "MWL SCU received no workitem matches"
        # PS3.4 Table C.4.1-1: the answer ENDS with a terminal status, and
        # "Success" is the only one that means the list was complete. A bridge
        # that answered a valid Pending and then failed (0xA700, 0xC000) used
        # to pass every assertion below, because the final status was ignored.
        assert statuses, "the SCU received no C-FIND response at all"
        assert statuses[-1] == 0x0000, (
            f"{statuses[:5]}..., terminal {statuses[-1]:#06x})"
        )
        # The nested station key IS the filter: the bridge reads it out of
        # (0040,0100) and sends ?ScheduledStationNameCodeSequence.CodeValue=
        # STATION01, so the OTHERSTATION workitem must be filtered out by the
        # server. Exactly one match is the proof that the filter ran.
        assert len(matches) == 1, f"expected exactly 1 match, got {len(matches)}"
        row = matches[0]

        # --- the conformant shape (PS3.4 Table K.6-1) ---
        assert "ScheduledProcedureStepSequence" in row, (
            "the SPS attributes must live inside (0040,0100), not flat: "
            f"{[e.tag for e in row]}"
        )
        sps = row.ScheduledProcedureStepSequence
        assert len(sps) == 1, "Table K.6-1: the sequence holds exactly one Item"
        item = sps[0]

        station_out = str(getattr(item, "ScheduledStationAETitle", ""))
        assert station_out == "STATION01", f"expected STATION01, got {station_out!r}"

        # The SPS ID is the identity the modality couples MPPS to. Derived as
        # the tail of the workitem UID, computed here by the same rule so the
        # assertion is a derivation check, not a hardcoded constant.
        sps_id = str(getattr(item, "ScheduledProcedureStepID", ""))
        assert sps_id, "the modality must receive a Scheduled Procedure Step ID"
        assert sps_id == station_uid[-16:], f"SPS ID {sps_id!r} != workitem UID tail"

        # Requested Procedure ID: derived from the input study, non-empty.
        rp_id = str(getattr(row, "RequestedProcedureID", ""))
        assert rp_id, "the modality asked for RequestedProcedureID and got none"
        assert rp_id == f"{station_uid}.study"[-16:], f"RP ID {rp_id!r} unexpected"

        # Start Date/Time are Return keys Type 1: served from the DT split.
        assert str(getattr(item, "ScheduledProcedureStepStartDate", "")) == "20260802"
        assert str(getattr(item, "ScheduledProcedureStepStartTime", "")) == "103000"

        # (0074,1200) is a UPS attribute (PS3.3 C.30.1), not an MWL one. The
        # workitem render CARRIES it (HIGH); the bridge must drop it.
        assert (
            "ScheduledProcedureStepPriority" not in row
        ), "a UPS attribute must not appear in an MWL row"
        assert "ScheduledProcedureStepPriority" not in item

        # The MWL model has no SOP Common attributes, so the workitem UID must
        # not leak in as SOP Instance UID.
        assert "SOPInstanceUID" not in row, "MWL has no SOP Instance UID"
    finally:
        assoc.release()
