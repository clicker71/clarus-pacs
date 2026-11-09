# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge  -  SOP Class and Printer registries.

YAML-driven SOP class acceptance. 11 built-in fallback classes.
Printer registry for C-PRINT (P22c).
"""

import logging


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

            if indent == 0 and stripped.endswith(":"):
                # Top-level section: "sop_classes:" or "printers:"
                current_section = stripped[:-1].strip()
                result[current_section] = {}
            elif indent == 2 and stripped.endswith(":"):
                # UID/name key: "  1.2.840...:"
                current_uid = stripped[:-1].strip().strip("\"'")
                current_entry = {}
                if current_section:
                    result[current_section][current_uid] = current_entry
            elif indent == 4 and ":" in stripped:
                # Key: value: "    name: CT Image Storage"
                key, _, value = stripped.partition(":")
                key = key.strip()
                # An inline comment is documentation, not part of the value:
                # `accept: false  # retired vendor` must stay a boolean, and a
                # quoted value must compare without its quotes. Without the
                # strip the comparison below sees "false  # retired vendor" and
                # stores a truthy string, so a class marked as not accepted is
                # accepted anyway.
                value = value.split("#", 1)[0].strip().strip("\"'")
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
    Falls back to built-in defaults if YAML is missing or unparseable.
    """
    try:
        data = _parse_simple_yaml(yaml_path)
        registry = data.get("sop_classes", {}) if data else {}
        if registry:
            logging.info(
                "[SOP-REG] Loaded %d SOP classes from %s", len(registry), yaml_path
            )
            return registry
    except FileNotFoundError:
        logging.info("[SOP-REG] Using built-in SOP defaults (no custom %s)", yaml_path)
    except Exception as e:
        logging.info(
            "[SOP-REG] Using built-in SOP defaults: %s parse error - %s", yaml_path, e
        )

    return _BUILTIN_SOP_CLASSES()


def _BUILTIN_SOP_CLASSES():
    """Return 11 built-in SOP classes  -  always available, no dependencies (CMP-006).

    Includes 3 legacy Retired classes still in active use on pre-2006 equipment:
    - MG For Processing (Retired): replaced by 1.2.840.10008.5.1.4.1.1.1.2.1
    - US Multi-frame (Retired): replaced by 1.2.840.10008.5.1.4.1.1.6.1
    - NM (Retired): replaced by 1.2.840.10008.5.1.4.1.1.20
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
