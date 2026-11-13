# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false

"""1211 convergence batch 5 - print SCU conformance (R4-R7).

Each test fails when its fix is reverted: R4 keeps 0x0116 out of the failure
class, R5 puts the two missing Film Session attributes back, R6 refuses a film
the printer cannot fully populate and R7 stops counting an empty page as
printed.
"""

from pydicom import Dataset

from tests.test_1211_batch4 import (  # noqa: F401  (pytest fixtures)
    _FakeAssoc,
    _FakeStatus,
    _image_bytes,
    _run_print,
)


def _print_with(monkeypatch, film_session, images=None):
    """Run the driver with a caller-supplied Film Session data set.

    `_run_print` builds its own Film Session; the attribute set is what R5 is
    about, so the test has to hand the driver the real one.
    """
    import sys
    from pathlib import Path

    import clean_code  # noqa: F401  (ensures the package is importable)

    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
    from drivers import generic_dicom as gd

    assoc = _FakeAssoc()

    class _AE:
        def __init__(self, **k):
            self.ae_title = k.get("ae_title")

        def add_requested_context(self, *a, **k):
            pass

        def associate(self, host, port, ae_title=None):
            return assoc

    monkeypatch.setattr(gd, "AE", _AE)

    gd.GenericDicomPrintDriver().print(
        film_session=film_session,
        images=images if images is not None else [_image_bytes()],
        printer_host="10.0.0.9",
        printer_port=11112,
        calling_aet="CLARUS_BRIDGE",
        called_aet="PRINTER_AE",
    )
    return assoc, None


# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# R4 - 0x0116 "Attribute Value Out of Range" is a WARNING
# ---------------------------------------------------------------------------
def test_r4_0116_is_not_read_as_a_failure(monkeypatch):
    """PS3.7 Table C.1-1 lists 0x0116 in the Warning class, and PS3.4 H.4.1
    keeps "the film session was created but an attribute is out of range" out
    of the Failure class. Dropping it aborted a job the printer had accepted."""
    from drivers import generic_dicom as gd

    assert gd._is_success_or_warning(0x0116) is True

    assoc = _FakeAssoc()
    real_create = assoc.send_n_create

    def _warn(ds, class_uid, instance_uid=None, msg_id=1, meta_uid=None):
        _status, rsp = real_create(ds, class_uid, instance_uid, msg_id, meta_uid)
        return _FakeStatus(0x0116), rsp

    assoc.send_n_create = _warn
    result, assoc, _created = _run_print(monkeypatch, assoc=assoc)
    assert result.status == 0x0000
    assert (
        result.film_boxes_printed == 1
    ), "0x0116 is a warning: the Film Session was created"


# ---------------------------------------------------------------------------
# R5 - the Film Session carries SpecificCharacterSet and MemoryAllocation
# ---------------------------------------------------------------------------
def test_r5_the_film_session_keeps_charset_and_memory_allocation(monkeypatch):
    """PS3.4 Table H.4-1: both are Film SESSION attributes. Dropping
    SpecificCharacterSet re-encoded a Cyrillic FilmSessionLabel."""
    from drivers import generic_dicom as gd

    assert "SpecificCharacterSet" in gd._FILM_SESSION_ATTRIBUTES
    assert "MemoryAllocation" in gd._FILM_SESSION_ATTRIBUTES

    film_session = Dataset()
    film_session.NumberOfCopies = 1
    film_session.ImageDisplayFormat = "STANDARD\\1,1"
    film_session.SpecificCharacterSet = "ISO_IR 192"
    film_session.MemoryAllocation = 4096

    assoc, _created = _print_with(monkeypatch, film_session)

    fs_ds = [c for c in assoc.calls if c[0] == "n_create"][0][1][0]
    assert fs_ds.SpecificCharacterSet == "ISO_IR 192"
    assert fs_ds.MemoryAllocation == 4096


# ---------------------------------------------------------------------------
# R6 - a film with fewer cells than images is refused BEFORE printing
# ---------------------------------------------------------------------------
def test_r6_more_images_than_cells_is_not_a_successful_print(monkeypatch):
    """The surplus images were listed as failed_images while the film was
    printed and reported SUCCESS: the caller was told a complete film went
    out when it was incomplete by construction."""
    assoc = _FakeAssoc(image_box_uids=("9.9.9.1",))
    result, assoc, _created = _run_print(
        monkeypatch,
        assoc=assoc,
        images=[_image_bytes(), _image_bytes()],
    )

    assert result.status != 0x0000, "an incomplete film is not success"
    assert result.film_boxes_printed == 0
    assert [idx for idx, _reason in result.failed_images] == [1]
    assert not [
        c for c in assoc.calls if c[0] == "n_action"
    ], "the refusal must come BEFORE the print action"


def test_r6_a_film_that_exactly_fits_is_printed(monkeypatch):
    assoc = _FakeAssoc(image_box_uids=("9.9.9.1",))
    result, assoc, _created = _run_print(monkeypatch, assoc=assoc)
    assert result.status == 0x0000
    assert result.film_boxes_printed == 1


# ---------------------------------------------------------------------------
# R7 - 0xB603 (empty page) is not a printed film
# ---------------------------------------------------------------------------
def test_r7_an_empty_page_result_is_not_counted_as_printed(monkeypatch):
    """PS3.4 H.4.2.2.4: 0xB603 means the Film Box contains no Image Box to
    print. The primitive COMPLETED, so the warning class swallowed it and the
    driver reported SUCCESS with one film box - a blank sheet nobody knew
    about."""
    assoc = _FakeAssoc()
    assoc.send_n_action = lambda *a, **k: (_FakeStatus(0xB603), None)
    result, _assoc, _created = _run_print(monkeypatch, assoc=assoc)

    assert result.film_boxes_printed == 0
    assert result.status == 0xB603
    assert result.is_success is False


def test_r7_another_bxxx_warning_is_still_a_printed_film(monkeypatch):
    """The special case is 0xB603 only: the other print warnings mean the
    film was produced with a note."""
    assoc = _FakeAssoc()
    assoc.send_n_action = lambda *a, **k: (_FakeStatus(0xB600), None)
    result, _assoc, _created = _run_print(monkeypatch, assoc=assoc)

    assert result.film_boxes_printed == 1
    assert result.status == 0x0000
