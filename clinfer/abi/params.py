#!/usr/bin/env python3
"""Read the requester's scheduled processing parameters out of a manifest.

clinfer flattens the UPS Scheduled Processing Parameters Sequence
(0074,1210) into ``manifest["parameters"]``: an object whose keys are the
Concept Name Code Values. EVERY value is a LIST, and a single-valued
parameter is a list of one:

    {"parameters": {"CLIP_LIMIT": [2.0], "ALGORITHM": ["clahe"]}}

The list is not decoration. (0074,1210) is a Sequence of Content Items
(PS3.3 C.30.2) and two of its items may carry the SAME Concept Name with
different values - that is how a requester states a multi-valued selection,
such as three organ classes. A JSON object key cannot repeat, so the values
are collected under one key in sequence order. The object is ALWAYS present,
``{}`` when the requester stated nothing.

The elements keep the JSON kind of the parameter's Value Type: TEXT a
string, NUMERIC a number, CODE a ``"scheme:value"`` string, DATETIME a
string.

A producer uses this module so that every script reads its tunables the same
way and a hand-written workitem cannot break the run:

    import params
    ...
    manifest = json.loads(...)
    clip_limit = params.number(manifest, "CLIP_LIMIT", 2.0)
    organs = params.value_list(manifest, "Selected Organ Class")

The scalar helpers (``number``, ``text``, ``code``, ``count``, ``grid``)
read the FIRST element of the list: a parameter the requester stated once
reads exactly as it always did, and a script that wants the rest asks with
``value_list`` or ``raw``.

The rule for every reader is the same: a missing key, a value of the wrong
kind, or an unparseable value falls back to the default. It never raises - a
processor that refuses to run because a demo workitem carried a stray
parameter would be less useful than one that runs with its documented
default. Being a demo contract, silence is the point: the fallback IS the
documented behaviour, not an error path.

Standard library only, and no import of anything in this directory: the
scripts are staged into a work directory and run from there by hand.
"""


def _parameters(manifest):
    """The manifest's parameter object, or an empty mapping.

    A manifest written by an older clinfer has no ``parameters`` key at all,
    and the contract says such a run must behave exactly like ``{}``.
    """
    if not isinstance(manifest, dict):
        return {}
    found = manifest.get("parameters")
    return found if isinstance(found, dict) else {}


def _values(manifest, key):
    """The parameter's list, or ``None`` when the key carries none.

    `None` and an empty list are different answers: the first means the key
    is missing or was not written by clinfer, the second means the parameter
    is there with no values at all. A scalar helper treats both as "nothing
    usable", while `raw` hands the empty list on as it is.
    """
    value = _parameters(manifest).get(key)
    return value if isinstance(value, list) else None


def raw(manifest, key, default=None):
    """The parameter's values exactly as the manifest carried them.

    The elements are not inspected, so a list of mixed kinds comes back as
    it was written.
    """
    return _parameters(manifest).get(key, default)


def value_list(manifest, key, default=None):
    """The parameter's values as a fresh list, whether it was stated once or many times.

    A copy, because a caller that appended to the returned list would
    otherwise mutate the default every later call shares. A value that is not
    a list is not a parameter clinfer wrote, so it falls back rather than
    being wrapped into a list of one.
    """
    values = _values(manifest, key)
    if values is None:
        return [] if default is None else list(default)
    return list(values)


def _first(manifest, key):
    """The first element of a parameter's list, or ``None``."""
    values = _values(manifest, key)
    if not values:
        return None
    return values[0]


def number(manifest, key, default):
    """The first NUMERIC value as a float, or ``default``.

    NUMERIC arrives as a JSON number, so ``int`` and ``float`` are both
    normal. A CODE parameter (``"scheme:value"``) and a TEXT parameter
    (arbitrary text) are both strings here; neither is a number, so both fall
    back rather than being guessed at - the value part of a code is not
    interchangeable with a plain number even when it looks like one.
    """
    value = _first(manifest, key)
    if isinstance(value, bool):
        return default
    if isinstance(value, (int, float)):
        return float(value)
    return default


def count(manifest, key, default):
    """The first NUMERIC value as an int, or ``default``.

    Truncation is deliberate and matches Python's own ``int()``: a requester
    that sends 24.0 for a pixel count means 24. A value that is not a finite
    number in range for an int falls back.
    """
    value = number(manifest, key, None)
    if value is None:
        return default
    try:
        return int(value)
    except (OverflowError, ValueError):
        return default


def text(manifest, key, default):
    """The first TEXT value as a string, or ``default``.

    A NUMBER is NOT turned into a string: a requester that sends 8 for a
    text parameter stated something the script cannot honour, and coercing it
    would hide the mismatch behind a ``"8"`` that reads like a deliberate
    name.
    """
    value = _first(manifest, key)
    return value if isinstance(value, str) else default


def grid(manifest, key, default):
    """The first NUMERIC value as a square ``(n, n)`` tuple, or ``default``.

    OpenCV's tile grid is one integer applied to both axes, so the parameter
    carries one number rather than a pair. A value that is not a positive
    integer falls back: a zero-sized tile is a division by zero inside OpenCV
    rather than a parameter the script can honour.
    """
    value = count(manifest, key, None)
    if value is None or value < 1:
        return default
    return (value, value)


def code(manifest, key, default):
    """The first CODE value's value part (after the FIRST ``:``), or ``default``.

    Only for a script that genuinely needs the code's value: the full
    ``"scheme:value"`` string is what the contract carries, so a caller that
    wants the scheme too reads it with ``text``. The split is on the first
    colon because a code value may itself contain colons.
    """
    value = text(manifest, key, None)
    if value is None or ":" not in value:
        return default
    return value.split(":", 1)[1]
