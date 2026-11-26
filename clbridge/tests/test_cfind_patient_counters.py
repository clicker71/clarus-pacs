# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""C-FIND PATIENT level: the three NumberOfPatientRelated* counters.

PS3.4 C.6.1.1.2: (0020,1200) NumberOfPatientRelatedStudies, (0020,1202)
NumberOfPatientRelatedSeries and (0020,1204) NumberOfPatientRelatedInstances
are TOTALS over the patient, so the fold must sum the study rows' own
series/instance counts per PatientID before emitting one row per patient.
"""

from unittest.mock import patch

from clean_code.handlers.cfind import _handle_patient_level

from tests.conftest import make_fake_find_event

URL = "http://127.0.0.1:8024/dicomweb"


def _study_row(patient_id, series, instances):
    return {
        "00100010": {"vr": "PN", "Value": [{"Alphabetic": "Doe^Jane"}]},
        "00100020": {"vr": "LO", "Value": [patient_id]},
        "00201206": {"vr": "IS", "Value": [str(series)]},
        "00201208": {"vr": "IS", "Value": [str(instances)]},
    }


def test_patient_level_emits_related_counters():
    """Two studies of P1 (series 1+3, instances 2+4) and one of P2 (5, 6)
    fold into two rows whose counters are the per-patient totals."""
    study_rows = [
        _study_row("P1", 1, 2),
        _study_row("P1", 3, 4),
        _study_row("P2", 5, 6),
    ]

    event = make_fake_find_event()
    event.identifier.QueryRetrieveLevel = "PATIENT"

    with patch("clean_code.handlers.cfind._qido_request", return_value=study_rows):
        results = list(_handle_patient_level(event, URL, False, 50, 30))

    rows = [row for status, row in results if status == 0xFF00]
    by_patient = {str(row.PatientID): row for row in rows}
    assert set(by_patient) == {"P1", "P2"}, by_patient

    p1 = by_patient["P1"]
    assert int(p1.NumberOfPatientRelatedStudies) == 2
    assert int(p1.NumberOfPatientRelatedSeries) == 4
    assert int(p1.NumberOfPatientRelatedInstances) == 6

    p2 = by_patient["P2"]
    assert int(p2.NumberOfPatientRelatedStudies) == 1
    assert int(p2.NumberOfPatientRelatedSeries) == 5
    assert int(p2.NumberOfPatientRelatedInstances) == 6

    assert results[-1] == (0x0000, None), results[-1]
