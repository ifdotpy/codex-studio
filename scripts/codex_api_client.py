"""Shared local HTTP transport for authenticated Codex Studio CLI requests."""

from __future__ import annotations

import json
from urllib.request import Request, urlopen


def request_json(url: str, path: str, data: object = None, token: str = "", *, timeout: float = 90) -> object:
    """Preserve codex-control's JSON and session-token request behavior."""
    request = Request(
        url.rstrip("/") + path,
        data=json.dumps(data).encode() if data is not None else None,
        headers={"Content-Type": "application/json", "X-Canvas-Token": token},
    )
    with urlopen(request, timeout=timeout) as response:
        return json.load(response)
