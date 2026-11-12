# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false

"""1211 convergence batch 5, T1 sibling - a JSON number stays a number.

T1 fixed the fixture; the converter was the actual cause of the pydicom
warning: `str(value)` turned every scalar into a string, so a US attribute
carried a string value that pydicom had to rescue.
"""

import warnings


def _convert(payload):
    from clean_code.http_server import _json_print_to_dataset

    return _json_print_to_dataset(payload)


def test_a_json_number_reaches_a_us_attribute_as_a_number():
    """PS3.5 Table 6.2-1: US takes a binary value. A string value on that VR
    makes pydicom warn and a strict SCP read the wrong type."""
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        ds = _convert({"ReferencedImageBoxSequence": [{"ImageBoxPosition": 1}]})

    item = ds[(0x2010, 0x0510)].value[0]
    assert item.ImageBoxPosition == 1
    assert not isinstance(item.ImageBoxPosition, str), (
        "the number must not be coerced to a string"
    )


def test_a_json_string_is_still_accepted_for_a_numeric_vr():
    """A caller may format IS/DS itself; the gate is the warning, not a ban."""
    ds = _convert({"NumberOfCopies": "2"})
    assert int(ds.NumberOfCopies) == 2


def test_a_string_vr_keeps_its_string_value():
    ds = _convert({"FilmSessionLabel": "CHEST 2 VIEWS"})
    assert str(ds.FilmSessionLabel) == "CHEST 2 VIEWS"


def test_a_multi_valued_numeric_attribute_keeps_numbers():
    """VM>1 on a numeric VR: every item is passed through as a number."""
    from clean_code.http_server import _print_attr_value

    assert _print_attr_value("US", 1) == 1
    assert _print_attr_value("IS", 2) == 2
    assert isinstance(_print_attr_value("US", 1), int), (
        "the value must reach pydicom as a number, not a string"
    )


def test_a_boolean_is_not_treated_as_a_number():
    from clean_code.http_server import _print_attr_value

    assert _print_attr_value("US", True) == "True", (
        "bool is an int subclass; it must keep the string form"
    )
