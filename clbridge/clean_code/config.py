# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Bridge: nginx-style config parser - 0 dependencies, KISS."""

import logging
from urllib.parse import urlsplit

VERSION = "0.3.1-beta"


class ConfParser:
    """Parses clbridge.conf (nginx-style: [section], key = value, # comment)."""

    def __init__(self, path):
        self.values = {}  # (section, key) -> value
        self._parse(path)

    def _parse(self, path):
        section = "global"
        with open(path, "r", encoding="utf-8") as f:
            for lineno, line in enumerate(f, 1):
                line = line.strip()
                if not line or line.startswith("#") or line.startswith(";"):
                    continue
                if line.startswith("[") and line.endswith("]"):
                    section = line[1:-1].strip().lower()
                    continue
                if "=" in line:
                    key, _, value = line.partition("=")
                    key = key.strip().lower()
                    value = value.strip()
                    # Strip inline comment
                    if "#" in value:
                        value = value[: value.index("#")].strip()
                    self.values[(section, key)] = value
                else:
                    logging.warning(
                        "Config line %d: expected key=value, got: %s", lineno, line
                    )

    def get(self, section, key, default=None):
        return self.values.get((section.lower(), key.lower()), default)

    def get_int(self, section, key, default=None):
        v = self.get(section, key)
        if v is None or v == "":
            return default
        try:
            return int(v)
        except (TypeError, ValueError):
            # A bare "invalid literal for int()" leaves the operator to guess
            # which of a hundred keys broke the start-up. Name it.
            raise ValueError(
                f"[{section}] {key}: expected an integer, got {v!r}"
            ) from None

    def get_float(self, section, key, default=None):
        """Get a config value as float, with explicit range validation."""
        v = self.get(section, key)
        if v is None or v == "":
            return default
        try:
            return float(v)
        except ValueError:
            raise ValueError(
                f"clbridge.conf [{section}] {key}: '{v}' is not a valid number"
            ) from None

    def get_bool(self, section, key, default=False):
        v = self.get(section, key)
        if v is None or v == "":
            return default
        return v.lower() in ("true", "1", "yes", "on")


# ---------------------------------------------------------------------------
# One clarus_url knob for DICOMweb (replaces host/clarus_port)
# ---------------------------------------------------------------------------
def validate_clarus_url(cfg):
    """Validate the [dicomweb] clarus_url knob. Raises ValueError on failure.

    Binding rules:
      - NO backward compatibility: host/clarus_port are removed. A config
        that still carries them is a startup error.
      - clarus_url is REQUIRED: full DICOMweb base URL, scheme http|https,
        parsed with urllib.parse.urlsplit, trailing slash stripped.
      - TLS verify is ALWAYS on (httpx/requests defaults); there is NO knob
        to disable it. The mTLS pair (tls_client_cert + tls_client_key) is
        optional; one without the other is a startup error.

    Returns (clarus_url, tls_client_cert, tls_client_key).
    """
    # A removed key must be a hard startup error: silently ignoring it would
    # leave the operator pointing at an endpoint that is never contacted.
    for legacy_key in ("host", "port", "clarus_port"):
        if cfg.get("dicomweb", legacy_key) is not None:
            raise ValueError(
                "Configuration error: [dicomweb] host/clarus_port removed - "
                "use clarus_url (full DICOMweb base URL)"
            )

    clarus_url = cfg.get("dicomweb", "clarus_url")
    if not clarus_url:
        raise ValueError(
            "Configuration error: [dicomweb] clarus_url is required "
            "(e.g. clarus_url = http://127.0.0.1:8024/dicomweb)"
        )

    parts = urlsplit(clarus_url)
    if parts.scheme.lower() not in ("http", "https"):
        raise ValueError(
            f"Configuration error: [dicomweb] clarus_url '{clarus_url}' "
            "has invalid scheme (must be http or https)"
        )
    if not parts.hostname:
        raise ValueError(
            f"Configuration error: [dicomweb] clarus_url '{clarus_url}' "
            "has no host (expected scheme://host[:port]/dicomweb)"
        )

    # `urlsplit` accepts any text after ':' as the netloc, so `host:abc` and
    # `host:99999` pass the checks above and only fail when the first request
    # tries to connect. `parts.port` raises ValueError for a non-numeric port
    # and for one outside 0-65535, which is exactly the configuration error to
    # report here. A URL with NO port yields None and stays valid: the scheme
    # default (80/443) is implied by the HTTP client, and every caller here
    # passes the URL straight to it.
    try:
        port = parts.port
    except ValueError:
        raise ValueError(
            f"Configuration error: [dicomweb] clarus_url '{clarus_url}' "
            "has an invalid port (expected a number between 1 and 65535)"
        ) from None
    # `urlsplit` returns 0 for ':0' without raising. Port 0 means "pick any
    # free port" for a BIND, which is meaningless for a CLIENT endpoint, so
    # accepting it started the bridge with a URL it can never reach.
    if port == 0:
        raise ValueError(
            f"Configuration error: [dicomweb] clarus_url '{clarus_url}' "
            "has port 0 (expected a number between 1 and 65535)"
        )

    # Trailing slash is stripped (not an error).
    clarus_url = clarus_url.rstrip("/")

    tls_cert = cfg.get("dicomweb", "tls_client_cert")
    tls_key = cfg.get("dicomweb", "tls_client_key")
    if bool(tls_cert) != bool(tls_key):
        raise ValueError(
            "Configuration error: [dicomweb] tls_client_cert and "
            "tls_client_key must be set together (mTLS pair)"
        )

    return clarus_url, tls_cert, tls_key


# ---------------------------------------------------------------------------
# peers.conf parser - nginx-style: one peer per line
# ---------------------------------------------------------------------------
def parse_peers_conf(path):
    """Parse peers.conf into list of (ae_title, host, port) tuples.

    Supported formats (one peer per line, # comments ignored):
      AE_TITLE  HOST:PORT           - colon-separated (preferred)
      AE_TITLE  HOST  PORT          - space-separated (backward compat)
      AE_TITLE  NETWORK/MASK:PORT   - CIDR subnet with colon
      AE_TITLE  NETWORK/MASK  PORT  - CIDR subnet, space port
      AE_TITLE  NETWORK  MASK  PORT - full mask, 4 tokens

    Returns list of (ae_title: str, host: str, port: int).
    For CIDR entries, host = "network/mask" string (resolved elsewhere).
    Logs warnings for malformed lines - does NOT abort.
    """
    peers = []
    try:
        with open(path, "r", encoding="utf-8") as f:
            for lineno, line in enumerate(f, 1):
                # An inline comment is documentation, not a token: without the
                # strip `AE 10.0.0.1:104  # scanner` splits into four tokens and
                # the line is rejected as malformed, so a commented peer list
                # silently yields no peers at all.
                line = line.split("#", 1)[0].strip()
                if not line:
                    continue

                tokens = line.split()
                if len(tokens) < 2:
                    logging.warning(
                        "[peers.conf:%d] Malformed (need at least 'AE HOST:PORT'): %s",
                        lineno,
                        line,
                    )
                    continue

                ae_title = tokens[0]

                try:
                    if len(tokens) == 2:
                        # AE_TITLE  HOST:PORT  (or NETWORK/MASK:PORT)
                        host, port_str = tokens[1].rsplit(":", 1)
                        port = int(port_str)
                    elif len(tokens) == 3:
                        # AE_TITLE  HOST  PORT  (space-separated, no colon)
                        # or AE_TITLE  NET/MASK  PORT
                        host = tokens[1]
                        port = int(tokens[2])
                    elif len(tokens) == 4:
                        # AE_TITLE  NETWORK  MASK  PORT
                        host = f"{tokens[1]}/{tokens[2]}"
                        port = int(tokens[3])
                    else:
                        logging.warning(
                            "[peers.conf:%d] Too many tokens (%d): %s",
                            lineno,
                            len(tokens),
                            line,
                        )
                        continue

                    if not (1 <= port <= 65535):
                        raise ValueError(f"port {port} out of range")

                    peers.append((ae_title, host, port))
                except (ValueError, IndexError):
                    logging.warning(
                        "[peers.conf:%d] Invalid address/port for AE '%s': %s",
                        lineno,
                        ae_title,
                        line,
                    )
    except FileNotFoundError:
        logging.warning(
            "[peers.conf] File not found: %s - no manual peers loaded", path
        )
    except OSError as e:
        logging.warning("[peers.conf] Cannot read %s: %s", path, e)
    return peers
