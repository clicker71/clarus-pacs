#!/usr/bin/env python3
"""Read the requester's scheduled processing parameters out of a manifest.

clinfer flattens the UPS Scheduled Processing Parameters Sequence
(0074,1210) into ``manifest["parameters"]``: an object whose keys are the
Concept Name Code Values and whose values are typed by the parameter's Value
Type (TEXT -> string, NUMERIC -> number, CODE -> ``"scheme:value"``). The
object is ALWAYS present, ``{}`` when the requester stated nothing.

A producer uses this module so that every script reads its tunables the same
way and a hand-written workitem cannot break the run:

    import params
    ...
    manifest = json.loads(...)
    clip_limit = params.number(manifest, "CLIP_LIMIT", 2.0)

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


def raw(manifest, key, default=None):
    """The parameter's value exactly as the manifest carried it."""
    return _parameters(manifest).get(key, default)


def number(manifest, key, default):
    """A NUMERIC parameter as a float, or ``default``.

    NUMERIC arrives as a JSON number, so ``int`` and ``float`` are both
    normal. A CODE parameter (``"scheme:value"``) and a TEXT parameter
    (arbitrary text) are both strings here; neither is a number, so both fall
    back rather than being guessed at - the value part of a code is not
    interchangeable with a plain number even when it looks like one.
    """
    value = _parameters(manifest).get(key)
    if isinstance(value, bool):
        return default
    if isinstance(value, (int, float)):
        return float(value)
    return default


def count(manifest, key, default):
    """A NUMERIC parameter as an int, or ``default``.

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
    """A TEXT parameter as a string, or ``default``.

    A NUMBER is NOT turned into a string: a requester that sends 8 for a
    text parameter stated something the script cannot honour, and coercing it
    would hide the mismatch behind a ``"8"`` that reads like a deliberate
    name.
    """
    value = _parameters(manifest).get(key)
    return value if isinstance(value, str) else default


def grid(manifest, key, default):
    """A NUMERIC parameter as a square ``(n, n)`` tuple, or ``default``.

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
    """A CODE parameter's value part (after the FIRST ``:``), or ``default``.

    Only for a script that genuinely needs the code's value: the full
    ``"scheme:value"`` string is what the contract carries, so a caller that
    wants the scheme too reads it with ``text``. The split is on the first
    colon because a code value may itself contain colons.
    """
    value = text(manifest, key, None)
    if value is None or ":" not in value:
        return default
    return value.split(":", 1)[1]
