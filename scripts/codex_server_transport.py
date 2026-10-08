"""Adapter to the owner-pairing service. No agent-provided credentials."""
from __future__ import annotations
from typing import Any, cast


class PairedServerTransport:
    def __init__(self, runtime: Any) -> None:
        self.runtime = runtime

    @property
    def local_server_id(self) -> str:
        return str(self.runtime.paired_access().local_server_id)

    def servers(self) -> list[dict[str, Any]]:
        return [row for row in self.runtime.paired_access().servers() if row.get('status') == 'paired']

    def request(self, server: str, envelope: dict[str, Any], *, timeout: int) -> dict[str, Any]:
        return cast(dict[str, Any], self.runtime.paired_access().request(server, 'POST', '/api/servers/orchestration', envelope,
            request_id=envelope['requestId'], timeout=timeout))
