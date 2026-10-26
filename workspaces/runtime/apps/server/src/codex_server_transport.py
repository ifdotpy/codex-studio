"""Adapter to the owner-pairing service. No agent-provided credentials."""
from __future__ import annotations
from collections.abc import Callable
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

    def stream(self, server: str, path: str, *, timeout: float, sink: Callable[[bytes], None]) -> bool:
        """A raw file over the QUIC link (docs/peer-link.md); False when the server has no link."""
        return bool(self.runtime.paired_access().stream(server, path, timeout, sink))


def receive_export(transport: Any, server: str, name: str, offset: int, size: int,
                   write: Callable[[bytes], None], timeout: float) -> int:
    """Receive `server-exports/<name>` of a paired server from `offset` as one stream over the QUIC
    link (docs/peer-link.md). Returns the end offset of the bytes given to `write`. A server without
    a link, a refused or a lost stream ends early; the caller continues with its chunk actions."""
    sender = getattr(transport, 'stream', None)
    received = offset

    def sink(data: bytes) -> None:
        nonlocal received
        if received + len(data) > size:
            raise ValueError('The export stream is longer than the export')
        write(data)
        received += len(data)

    if sender is not None and offset < size:
        try:
            sender(server, f'/api/servers/exports/{name}?offset={offset}', timeout=timeout, sink=sink)
        except Exception:
            pass  # The bytes before the failure stay valid; the chunks continue from there.
    return received
