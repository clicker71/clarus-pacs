# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge  -  SOP Class and Printer registries.

YAML-driven SOP class acceptance. 13 built-in fallback classes.
Printer registry for C-PRINT (P22c).
"""

import logging


class RegistryConfigError(RuntimeError):
    """A custom registry file exists but yields nothing usable.

    Distinct from "no file at all": a missing file is a supported deployment
    (the built-ins are the documented fallback), while a file the operator
    wrote and the loader could not use is a configuration error. Startup fails
    on the latter rather than silently re-enabling what the file disables.
    """


def _strip_inline_comment(text):
    """Remove a trailing `#` comment, honouring quoted scalars.

    `name: "Film #2" # lobby` keeps its `#2`: the octothorpe inside a quoted
    value is data, not the start of a comment. A blind `split("#", 1)` cut it
    off, so the printer was registered as `Film`.

    An unclosed quote is treated as ordinary text: a malformed line must not
    swallow the rest of the file.
    """
    out = []
    quote = ""
    escaped = False
    for ch in text:
        if escaped:
            out.append(ch)
            escaped = False
            continue
        if ch == "\\" and quote:
            out.append(ch)
            escaped = True
            continue
        if quote:
            out.append(ch)
            if ch == quote:
                quote = ""
            continue
        if ch in ('"', "'"):
            quote = ch
            out.append(ch)
            continue
        if ch == "#":
            break
        out.append(ch)
    return "".join(out).rstrip()


# ---------------------------------------------------------------------------
# _parse_simple_yaml  -  minimal YAML subset parser (0 dependencies)
# ---------------------------------------------------------------------------
def _parse_simple_yaml(path):
    """Parse a minimal YAML subset: top-level key with nested dicts of name/accept.

    Only handles the exact format used by sop_classes.yaml and printers.yaml.
    No dependencies. Replaces PyYAML (~500 KB).
    """
    result = {}
    current_section = None
    current_uid = None
    current_entry = {}

    with open(path, "r", encoding="utf-8") as f:
        for line in f:
            # Skip empty lines and comments
            stripped = line.strip()
            if not stripped or stripped.startswith("#"):
                continue

            # Count leading spaces for indentation
            indent = len(line) - len(line.lstrip())

            if indent == 0:
                # Top-level section: "sop_classes:" or "printers:".
                #
                # The comment is stripped BEFORE the `endswith(":")` test, for
                # the same reason as an entry header: `sop_classes: # local
                # policy` ends on the comment, so the header was skipped and
                # every entry below it was dropped on the floor. The loader
                # then found no registry at all and fell back to the built-ins,
                # re-enabling a class the operator had switched off.
                header = _strip_inline_comment(stripped)
                if header.endswith(":"):
                    current_section = header[:-1].strip()
                    result[current_section] = {}
                    current_uid = None
                    current_entry = {}
                else:
                    # Not a section this parser understands (e.g. `MR: &policy`
                    # - a key with an anchor, not a section header). Treating
                    # it as noise and carrying on left the PREVIOUS section and
                    # entry active, so every field indented below it was written
                    # onto the previous class: one unsupported line silently
                    # rewrote its neighbour's `accept`.
                    current_section = None
                    current_uid = None
                    current_entry = {}
            elif indent == 2:
                # UID/name key: "  1.2.840...:". An inline comment after the
                # colon ("  1.2.840...:  # MR") is documentation, not part of
                # the key: strip it BEFORE the `endswith(":")` test, otherwise
                # the line ends on the comment, the header is skipped and the
                # NEXT `accept:` line lands on the PREVIOUS UID - re-enabling
                # a class the operator switched off.
                header = _strip_inline_comment(stripped)
                if not header.endswith(":"):
                    continue
                current_uid = header[:-1].strip().strip("\"'")
                current_entry = {}
                if current_section:
                    result[current_section][current_uid] = current_entry
            elif indent == 4 and ":" in stripped:
                # Key: value: "    name: CT Image Storage"
                key, _, value = stripped.partition(":")
                key = key.strip()
                if current_uid is None or not current_section:
                    # Between a skipped header and the next valid one there is
                    # no owning entry, so this field belongs to nobody. Writing
                    # it anyway would mutate the last dictionary created - the
                    # previous class - which is the corruption this parser must
                    # not commit.
                    continue
                # An inline comment is documentation, not part of the value:
                # `accept: false  # retired vendor` must stay a boolean, and a
                # quoted value must compare without its quotes. The comment is
                # stripped OUTSIDE quotes only, so `name: "Film #2" # lobby`
                # keeps its `#2`.
                value = _strip_inline_comment(value.strip()).strip().strip("\"'")
                if value.lower() == "true":
                    current_entry[key] = True
                elif value.lower() == "false":
                    current_entry[key] = False
                else:
                    current_entry[key] = value

    return result


# ---------------------------------------------------------------------------
# Bridge: SOP Class Registry  -  YAML-driven, no code changes needed
# ---------------------------------------------------------------------------
def _load_sop_registry(yaml_path="sop_classes.yaml"):
    """Load SOP class registry from YAML file (BR-009: no PyYAML dependency).

    Returns dict[uid -> dict] with keys: name, accept.
    A MISSING file falls back to the built-ins. Any other failure - unreadable,
    undecodable, a parse error - raises `RegistryConfigError`: those are cases
    where a file exists and was meant to be honoured, and silently installing
    the built-ins would re-enable a class that file switches off.
    """
    try:
        data = _parse_simple_yaml(yaml_path)
        registry = data.get("sop_classes", {}) if data else {}
    except FileNotFoundError:
        logging.info("[SOP-REG] Using built-in SOP defaults (no custom %s)", yaml_path)
        return _BUILTIN_SOP_CLASSES()
    except Exception as e:
        # Not a missing file: the operator wrote a registry and the bridge
        # could not read or understand it. Falling back to the built-ins is a
        # silent change of ADMISSION policy - a class the file disables comes
        # back enabled - so startup stops and names the fault instead.
        raise RegistryConfigError(
            f"{yaml_path}: custom SOP class registry cannot be read ({e}) - "
            "refusing to fall back to the built-ins, which would re-enable a "
            "class this file may disable"
        ) from e

    if registry:
        # A file that parses but yields an entry without `accept` means the
        # parser desynchronised for THAT entry. Accepting it as-is silently
        # turns an unreadable class into an accepted one, so the entry is
        # dropped and reported. Replacing the WHOLE registry with the built-ins
        # (the old behaviour) threw away every correct line as well - an
        # operator's `accept: false` was replaced by a built-in `accept: true`,
        # so a class they switched off came back on.
        broken = sorted(uid for uid, entry in registry.items() if "accept" not in entry)
        if broken:
            logging.error(
                "[SOP-REG] %s: %d SOP class entr%s without `accept` (%s) - "
                "those entries are ignored, the rest of the file is used",
                yaml_path,
                len(broken),
                "y" if len(broken) == 1 else "ies",
                ", ".join(broken[:3]) + (", ..." if len(broken) > 3 else ""),
            )
            for uid in broken:
                registry.pop(uid, None)
        # `accept` must be a real boolean. `_parse_simple_yaml` already turns
        # the two literals into bools, so anything else got here as a STRING
        # (`accept: yes`, `accept: 1`, `accept: on`) and `bool("no")` is True.
        # Coercing it would accept a class the operator wrote as refused, which
        # is the same silent-wrong-answer failure as a missing key, so such an
        # entry is dropped rather than corrected - never the whole file.
        mistyped = sorted(
            uid
            for uid, entry in registry.items()
            if not isinstance(entry["accept"], bool)
        )
        if mistyped:
            for uid in mistyped:
                logging.error(
                    "[SOP-REG] %s: SOP class %s has non-boolean accept=%r - "
                    "expected true or false; entry ignored",
                    yaml_path,
                    uid,
                    registry[uid]["accept"],
                )
            logging.error(
                "[SOP-REG] %d SOP class entr%s with a non-boolean `accept` - "
                "ignored; the other %d entr%s of the file are still used",
                len(mistyped),
                "y" if len(mistyped) == 1 else "ies",
                len(registry) - len(mistyped),
                "y" if len(registry) - len(mistyped) == 1 else "ies",
            )
            for uid in mistyped:
                registry.pop(uid, None)
        if not registry:
            # The file EXISTS and parsed, but nothing in it survived. Falling
            # back to the built-ins here is the same silent-wrong-answer the
            # per-entry rules above exist to prevent: an operator who wrote
            # `accept: false` for the only class they described would get that
            # class back, accepted, from the built-in table.
            #
            # A MISSING file still falls back (the historical, documented
            # behaviour for a deployment with no custom registry); a present
            # file that yields nothing is a configuration error, and startup
            # says so instead of guessing.
            raise RegistryConfigError(
                f"{yaml_path}: a custom SOP class registry was found but no "
                "entry in it is usable (missing or non-boolean `accept`) - "
                "refusing to fall back to the built-ins, which would re-enable "
                "a class this file disables"
            )
        logging.info(
            "[SOP-REG] Loaded %d SOP classes from %s", len(registry), yaml_path
        )
        return registry

    # A file that parses to zero entries is also a recognized-but-empty
    # registry: the operator wrote a file and got nothing applied.
    raise RegistryConfigError(
        f"{yaml_path}: a custom SOP class registry was found but describes no "
        "SOP class - refusing to fall back to the built-in defaults"
    )


def _BUILTIN_SOP_CLASSES():
    """Return 13 built-in SOP classes  -  always available, no dependencies (CMP-006).

    Both the current and the Retired UID are listed for the two classes whose
    UID was re-issued, because the fallback runs precisely when no
    sop_classes.yaml exists and the installed base is unknown: a cine loop
    arrives as 1.2.840.10008.5.1.4.1.1.3.1 on current equipment and as
    1.2.840.10008.5.1.4.1.1.3 on pre-2006 equipment.

    Retired classes kept for pre-2006 equipment still in active use:
    - MG For Processing (Retired): 1.2.840.10008.5.1.4.1.1.1.2.1
    - US Multi-frame (Retired): 1.2.840.10008.5.1.4.1.1.3, superseded by ...3.1
    - NM (Retired): 1.2.840.10008.5.1.4.1.1.5, superseded by ...20
    """
    return {
        "1.2.840.10008.1.1": {"name": "Verification", "accept": True},
        "1.2.840.10008.5.1.4.1.1.2": {"name": "CT Image Storage", "accept": True},
        "1.2.840.10008.5.1.4.1.1.4": {"name": "MR Image Storage", "accept": True},
        "1.2.840.10008.5.1.4.1.1.1": {
            "name": "Computed Radiography Image Storage",
            "accept": True,
        },
        "1.2.840.10008.5.1.4.1.1.1.1": {
            "name": "Digital X-Ray Image Storage - For Presentation",
            "accept": True,
        },
        "1.2.840.10008.5.1.4.1.1.1.2": {
            "name": "Digital Mammography X-Ray Image Storage - For Presentation",
            "accept": True,
        },
        "1.2.840.10008.5.1.4.1.1.7": {
            "name": "Secondary Capture Image Storage",
            "accept": True,
        },
        "1.2.840.10008.5.1.4.1.1.6.1": {
            "name": "Ultrasound Image Storage",
            "accept": True,
        },
        # Legacy Retired (pre-2006 equipment still in active use)
        "1.2.840.10008.5.1.4.1.1.1.2.1": {
            "name": "Digital Mammography X-Ray - For Processing (Retired)",
            "accept": True,
        },
        "1.2.840.10008.5.1.4.1.1.3": {
            "name": "Ultrasound Multi-frame Image Storage (Retired)",
            "accept": True,
        },
        "1.2.840.10008.5.1.4.1.1.5": {
            "name": "Nuclear Medicine Image Storage (Retired)",
            "accept": True,
        },
        # Current UIDs of the same two classes (PS3.6 Table A-1).
        "1.2.840.10008.5.1.4.1.1.3.1": {
            "name": "Ultrasound Multi-frame Image Storage",
            "accept": True,
        },
        "1.2.840.10008.5.1.4.1.1.20": {
            "name": "Nuclear Medicine Image Storage",
            "accept": True,
        },
    }


def _is_sop_accepted(sop_registry, context_uid):
    """Check if a SOP class UID is accepted by the Bridge.

    Args:
        sop_registry: dict from _load_sop_registry()
        context_uid: pynetdicom UID string (e.g. '1.2.840.10008.5.1.4.1.1.2')

    Returns:
        True if the SOP class is in registry and accept=True.
    """
    info = sop_registry.get(context_uid)
    if info and info.get("accept", False):
        return True
    return False


def _get_sop_registry_uids(sop_registry):
    """Return list of accepted SOP class UID strings from registry."""
    return [uid for uid, info in sop_registry.items() if info.get("accept", False)]


# ---------------------------------------------------------------------------
# Bridge: C-PRINT printer registry loader
# ---------------------------------------------------------------------------
def _load_printer_registry(yaml_path="printers.yaml"):
    """Load printer registry from YAML file (BR-009: no PyYAML dependency).

    Returns dict[str -> dict] with keys: name, host, port, aet, driver.
    Falls back to empty dict if YAML is missing.
    """
    try:
        data = _parse_simple_yaml(yaml_path)
        registry = data.get("printers", {}) if data else {}
        if registry:
            logging.info(
                "[PRINT-REG] Loaded %d printers from %s", len(registry), yaml_path
            )
            return registry
    except FileNotFoundError:
        logging.debug("[PRINT-REG] No custom printers.yaml - C-PRINT disabled")
    except Exception as e:
        logging.info("[PRINT-REG] Printers config parse error - %s: %s", yaml_path, e)

    return {}
