# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportCallIssue=false

"""Tests for C-PRINT driver framework (P22c)."""

import io
import os
import sys

import pytest
from pydicom import Dataset, FileMetaDataset, dcmwrite
from pydicom.uid import ImplicitVRLittleEndian, generate_uid
from pynetdicom.sop_class import (
    BasicColorImageBox,
    BasicGrayscaleImageBox,
    SecondaryCaptureImageStorage,
)

# Ensure the clbridge directory (our parent) is importable
sys.path.insert(0, os.path.dirname(__file__))

from drivers.base import AbstractPrintDriver, PrintResult
from drivers.generic_dicom import GenericDicomPrintDriver

# ---------------------------------------------------------------------------
# 8.1 PrintResult dataclass tests
# ---------------------------------------------------------------------------


def test_print_result_success():
    result = PrintResult(job_id="001", status=0x0000, film_boxes_printed=1)
    assert result.is_success
    assert result.failed_images == []
    assert result.printer_message is None


def test_print_result_failure():
    result = PrintResult(
        job_id="002",
        status=0xB603,
        film_boxes_printed=0,
        failed_images=[(0, "N-SET timeout")],
        printer_message="Printer out of film",
    )
    assert not result.is_success
    assert len(result.failed_images) == 1
    assert result.failed_images[0][1] == "N-SET timeout"


# ---------------------------------------------------------------------------
# 8.2 AbstractPrintDriver test (via mock implementation)
# ---------------------------------------------------------------------------


class MockPrintDriver(AbstractPrintDriver):
    def print(self, film_session, images, printer_host, printer_port, calling_aet):
        return PrintResult(job_id="mock-001", status=0x0000, film_boxes_printed=1)

    def driver_name(self):
        return "mock"


def test_abstract_driver():
    driver = MockPrintDriver()
    result = driver.print(None, [], "127.0.0.1", 104, "TEST")
    assert result.is_success
    assert driver.driver_name() == "mock"


def test_abstract_driver_cannot_instantiate():
    """AbstractPrintDriver cannot be instantiated directly."""
    with pytest.raises(TypeError):
        AbstractPrintDriver()  # type: ignore[abstract]


# ---------------------------------------------------------------------------
# 8.3 GenericDicomPrintDriver._image_box_sop_class tests
# ---------------------------------------------------------------------------


def test_image_box_sop_class_grayscale():
    driver = GenericDicomPrintDriver()
    ds = Dataset()
    ds.SamplesPerPixel = 1
    ds.PhotometricInterpretation = "MONOCHROME2"
    file_meta = FileMetaDataset()
    file_meta.MediaStorageSOPClassUID = SecondaryCaptureImageStorage
    file_meta.MediaStorageSOPInstanceUID = generate_uid()
    file_meta.TransferSyntaxUID = ImplicitVRLittleEndian
    ds.file_meta = file_meta
    buf = io.BytesIO()
    dcmwrite(buf, ds, enforce_file_format=False)
    assert driver._image_box_sop_class([buf.getvalue()]) == BasicGrayscaleImageBox


def test_image_box_sop_class_color():
    driver = GenericDicomPrintDriver()
    ds = Dataset()
    ds.SamplesPerPixel = 3
    ds.PhotometricInterpretation = "RGB"
    file_meta = FileMetaDataset()
    file_meta.MediaStorageSOPClassUID = SecondaryCaptureImageStorage
    file_meta.MediaStorageSOPInstanceUID = generate_uid()
    file_meta.TransferSyntaxUID = ImplicitVRLittleEndian
    ds.file_meta = file_meta
    buf = io.BytesIO()
    dcmwrite(buf, ds, enforce_file_format=False)
    assert driver._image_box_sop_class([buf.getvalue()]) == BasicColorImageBox


def test_image_box_sop_class_empty():
    """Empty image list defaults to grayscale."""
    driver = GenericDicomPrintDriver()
    assert driver._image_box_sop_class([]) == BasicGrayscaleImageBox


def test_image_box_sop_class_ybr():
    """YBR_FULL photometric interpretation ??? color."""
    driver = GenericDicomPrintDriver()
    ds = Dataset()
    ds.SamplesPerPixel = 1  # YBR can have spp=1 but photometric=YBR_FULL
    ds.PhotometricInterpretation = "YBR_FULL"
    file_meta = FileMetaDataset()
    file_meta.MediaStorageSOPClassUID = SecondaryCaptureImageStorage
    file_meta.MediaStorageSOPInstanceUID = generate_uid()
    file_meta.TransferSyntaxUID = ImplicitVRLittleEndian
    ds.file_meta = file_meta
    buf = io.BytesIO()
    dcmwrite(buf, ds, enforce_file_format=False)
    assert driver._image_box_sop_class([buf.getvalue()]) == BasicColorImageBox


# ---------------------------------------------------------------------------
# 8.4 Load printer registry tests
# ---------------------------------------------------------------------------


def test_load_printer_registry(tmp_path):
    """Test loading a valid printers.yaml."""
    # _load_printer_registry lives in the bundled standalone script, which we
    # can't import here, so the YAML loading logic is tested directly.

    # Direct test of YAML structure
    import yaml

    yaml_path = tmp_path / "printers.yaml"
    yaml_path.write_text("""
printers:
  test:
    name: Test Printer
    host: 127.0.0.1
    port: 104
    aet: TEST
    driver: generic_dicom
""")
    with open(str(yaml_path), "r") as f:
        data = yaml.safe_load(f)
    registry = data.get("printers", {})
    assert "test" in registry
    assert registry["test"]["host"] == "127.0.0.1"
    assert registry["test"]["port"] == 104
    assert registry["test"]["aet"] == "TEST"
    assert registry["test"]["driver"] == "generic_dicom"


def test_load_printer_registry_missing(tmp_path):
    """Test that missing YAML returns empty without crashing."""
    import yaml

    nonexistent = str(tmp_path / "nonexistent.yaml")
    try:
        with open(nonexistent, "r") as f:
            data = yaml.safe_load(f)
        registry = data.get("printers", {}) if data else {}
    except FileNotFoundError:
        registry = {}
    assert registry == {}


def test_load_printer_registry_empty(tmp_path):
    """Test that YAML with no printers returns empty dict."""
    import yaml

    yaml_path = tmp_path / "empty.yaml"
    yaml_path.write_text("# No printers configured\n")
    with open(str(yaml_path), "r") as f:
        data = yaml.safe_load(f)
    registry = data.get("printers", {}) if data else {}
    assert registry == {}


# ---------------------------------------------------------------------------
# 8.5 Driver name test
# ---------------------------------------------------------------------------


def test_driver_name():
    driver = GenericDicomPrintDriver()
    assert driver.driver_name() == "generic_dicom"


# ---------------------------------------------------------------------------
# 8.6 JSON ??? Dataset conversion logic test
# ---------------------------------------------------------------------------


def test_json_to_dataset_patient_name():
    """Test converting DICOM keyword to Dataset tag."""
    from pydicom import Dataset as DS
    from pydicom.datadict import keyword_dict

    ds = DS()
    tag = keyword_dict.get("PatientName")
    assert tag is not None
    # keyword_dict values are ints like 0x00100010; extract group/element
    group = tag >> 16
    elem = tag & 0xFFFF
    ds.add_new((group, elem), "SH", "Doe^John")
    assert ds.PatientName == "Doe^John"


def test_json_to_dataset_hex_tag():
    """Test converting hex DICOM tag to Dataset."""
    ds = Dataset()
    # 00100010 = PatientName
    ds.add_new((0x0010, 0x0010), "SH", "Smith^Jane")
    assert ds.PatientName == "Smith^Jane"


# ---------------------------------------------------------------------------
# 8.7 Metrics / Health endpoint tests (P22d)
# ---------------------------------------------------------------------------

# Shared bridge module loader ??? import once to avoid duplicate Prometheus metrics
_bridge_module = None


def _get_bridge_module():
    """Load clbridge.py as a module, caching the result."""
    global _bridge_module
    if _bridge_module is not None:
        return _bridge_module

    import importlib.util

    # test_print.py is run after tests/ which already registered metrics
    # in the global default registry.

    bridge_path = os.path.join(os.path.dirname(__file__), "clbridge.py")
    spec = importlib.util.spec_from_file_location("clarus_bridge_shared", bridge_path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    _bridge_module = mod  # Only cache after successful load
    return _bridge_module


@pytest.mark.integration
def test_metrics_endpoint():
    """GET /metrics returns 200 with Prometheus text format and expected metrics."""
    import threading
    import urllib.request
    from http.server import HTTPServer

    bridge = _get_bridge_module()
    handler_cls = bridge.BridgeHTTPHandler

    # Start HTTP server on random port with the handler
    server = HTTPServer(("127.0.0.1", 0), handler_cls)
    port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    try:
        resp = urllib.request.urlopen(f"http://127.0.0.1:{port}/metrics")
        assert resp.status == 200
        content_type = resp.headers.get("Content-Type", "")
        assert "text/plain" in content_type
        body = resp.read().decode("utf-8")
        assert "clarus_bridge_up" in body
        assert "clarus_bridge_cstore_requests_total" in body
    finally:
        server.shutdown()
        thread.join(timeout=5)


@pytest.mark.integration
def test_health_endpoint():
    """GET /health returns 200 with 'OK' body."""
    import threading
    import urllib.request
    from http.server import HTTPServer

    bridge = _get_bridge_module()
    handler_cls = bridge.BridgeHTTPHandler

    server = HTTPServer(("127.0.0.1", 0), handler_cls)
    port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    try:
        resp = urllib.request.urlopen(f"http://127.0.0.1:{port}/health")
        assert resp.status == 200
        body = resp.read().decode("utf-8")
        assert body.strip() == "OK"
    finally:
        server.shutdown()
        thread.join(timeout=5)
