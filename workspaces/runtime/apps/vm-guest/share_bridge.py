"""Carry the Mac's read-only share connections from vsock to the guest-local Samba.

The VM helper on the Mac listens on 127.0.0.1 and connects to this vsock port, so the
share never uses the VM network: macOS applies no Local Network rule to loopback, and
nothing on the VM network reaches Samba, which listens only on the guest loopback.
"""
from __future__ import annotations

import asyncio
import socket

PORT = 4052
SAMBA = ("127.0.0.1", 445)
MAX_CONNECTIONS = 64
CHUNK = 64 * 1024


async def pipe(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    try:
        while data := await reader.read(CHUNK):
            writer.write(data)
            await writer.drain()
    except OSError:
        pass
    finally:
        # The other direction ends when its peer sees this close.
        writer.close()


class Bridge:
    def __init__(self, target: tuple[str, int] = SAMBA, limit: int = MAX_CONNECTIONS):
        self.target = target
        self.slots = asyncio.Semaphore(limit)

    async def client(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        if self.slots.locked():
            writer.close()
            return
        async with self.slots:
            try:
                upstream = await asyncio.wait_for(asyncio.open_connection(*self.target), 10)
            except (OSError, asyncio.TimeoutError):
                writer.close()
                return
            await asyncio.gather(pipe(reader, upstream[1]), pipe(upstream[0], writer))


def vsock_listener(port: int = PORT) -> socket.socket:
    listener = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    listener.bind((socket.VMADDR_CID_ANY, port))
    listener.listen(MAX_CONNECTIONS)
    listener.setblocking(False)
    return listener


async def serve(listener: socket.socket, bridge: Bridge) -> None:
    server = await asyncio.start_server(bridge.client, sock=listener, limit=CHUNK)
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(serve(vsock_listener(), Bridge()))
