"""Exact HTTPS origin policy for a loopback server behind Tailscale Serve."""
from __future__ import annotations

import ipaddress
import json
import os
import re
from pathlib import Path
from urllib.parse import urlsplit


def validate_origin(value):
    if not isinstance(value, str):
        raise ValueError("Use an HTTPS Tailscale origin")
    url = urlsplit(value)
    if (url.scheme != "https" or not url.hostname or not url.hostname.endswith(".ts.net")
            or url.username is not None or url.password is not None
            or url.path not in ("", "/") or url.query or url.fragment
            or not re.fullmatch(r"[a-z0-9.-]+", url.hostname)):
        raise ValueError("Use an HTTPS origin ending in .ts.net, without a path")
    port = url.port
    if port is not None and not 1 <= port <= 65535:
        raise ValueError("Invalid HTTPS port")
    return "https://" + url.hostname + (f":{port}" if port and port != 443 else "")


class RemoteAccess:
    def __init__(self, root, origin=None):
        self.path = Path(root) / "remote-access.json"
        configured = origin if origin is not None else os.environ.get("CODEX_CANVAS_PUBLIC_ORIGIN")
        self.override = validate_origin(configured) if configured else None

    def origin(self):
        if self.override:
            return self.override
        try:
            data = json.loads(self.path.read_text())
            if data.get("enabled") is not True:
                return None
            return validate_origin(data.get("origin"))
        except (OSError, ValueError, AttributeError):
            return None

    def request_origin(self, headers, peer, port):
        # The only public listener is Serve. Do not trust forwarded headers from
        # any other socket, or derive an allowed origin from the request itself.
        try:
            if not ipaddress.ip_address(peer).is_loopback:
                return None
        except ValueError:
            return None
        for name in ("Host", "Origin", "X-Forwarded-Host", "X-Forwarded-Proto", "X-Forwarded-For"):
            if len(headers.get_all(name, [])) > 1:
                return None
        host = headers.get("Host")
        local = {f"127.0.0.1:{port}", f"localhost:{port}"}
        forwarded = any(headers.get(name) is not None for name in
                        ("X-Forwarded-Host", "X-Forwarded-Proto", "X-Forwarded-For"))
        if not forwarded:
            return f"http://{host}" if host in local else None
        origin = self.origin()
        if not origin:
            return None
        authority = urlsplit(origin).netloc
        if (headers.get("X-Forwarded-Proto") != "https"
                or headers.get("X-Forwarded-Host") != authority
                or host not in local | {authority}):
            return None
        try:
            source = ipaddress.ip_address(headers.get("X-Forwarded-For", ""))
            if not (source in ipaddress.ip_network("100.64.0.0/10")
                    or source in ipaddress.ip_network("fd7a:115c:a1e0::/48")):
                return None
        except ValueError:
            return None
        return origin
