"""Prebound single-process Uvicorn listener facade used by legacy callers."""
from __future__ import annotations

import asyncio
from contextlib import contextmanager
import errno
import os
from pathlib import Path
import socket
import threading
import time
from collections.abc import Iterator
from typing import TYPE_CHECKING, cast

import uvicorn
from starlette.types import ASGIApp, Message, Receive, Scope, Send

# Snapshot disk identity before importing the application modules whose loaded
# Python code defines the OpenAPI document.
import codex_backend_identity

from studio_api.app import create_app
from studio_api.context import ApiContext, RemoteAccessContract

if TYPE_CHECKING:
    from codex_canvas import Canvas

LISTEN_HOST = "127.0.0.1"
LISTEN_BACKLOG = socket.SOMAXCONN
UNIX_SOCKET_NAME = "canvas.sock"
UNIX_SOCKET_MODE = 0o600
UVICORN_LOG_CONFIG = None
API_SCHEMA_HASH_START_DELAY_SECONDS = 0.25


class _StudioUvicornServer(uvicorn.Server):
    def __init__(self, config: uvicorn.Config, context: ApiContext) -> None:
        super().__init__(config)
        self.context = context
        self.schema_hash_start_scheduled = False

    @contextmanager
    def capture_signals(self) -> Iterator[None]:
        # codex_canvas owns the established SIGTERM-to-shutdown contract.
        yield

    async def on_tick(self, counter: int) -> bool:
        should_exit = await super().on_tick(counter)
        # Let the first ordinary read finish before the CPU-heavy OpenAPI build
        # starts. Gated requests can still start and await the shared future.
        if not self.schema_hash_start_scheduled:
            self.schema_hash_start_scheduled = True
            asyncio.get_running_loop().call_later(
                API_SCHEMA_HASH_START_DELAY_SECONDS,
                self.context.start_api_schema_hash,
            )
        if self.context.runtime is not None and time.monotonic() - self.context._maintenance_last >= 3600:
            await asyncio.to_thread(_run_maintenance, self.context)
        return should_exit


def _run_maintenance(context: ApiContext) -> None:
    """Run the legacy hourly runtime/audio cleanup once across both listeners."""
    if not context._maintenance_lock.acquire(blocking=False):
        return
    try:
        now = time.monotonic()
        if now - context._maintenance_last < 3600:
            return
        runtime = context.runtime
        if runtime is None:
            return
        context._maintenance_last = now
        from codex_execution import maintenance
        from codex_voice import prune_audio

        maintenance(runtime)
        try:
            prune_audio(context.canvas.root)
        except OSError as error:
            import sys

            print(f"Voice audio cleanup failed: {error}", file=sys.stderr)
    finally:
        context._maintenance_lock.release()


class UnixScopeApp:
    """Mark requests from the Unix listener for its distinct trust policy."""

    def __init__(self, app: ASGIApp) -> None:
        self.app = app

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        if scope["type"] == "http":
            marked = dict(scope)
            extensions = dict(scope.get("extensions", {}))
            extensions["studio.unix_socket"] = True
            marked["extensions"] = extensions
            scope = marked
        await self.app(scope, receive, send)


class BoundServer:
    """Small make_server-compatible lifecycle around one already-bound socket."""

    daemon_threads = True
    unix_server: BoundServer | None = None

    def __init__(
        self,
        sock: socket.socket,
        app: ASGIApp,
        context: ApiContext,
        *,
        owned_unix: tuple[int, int] | None = None,
    ) -> None:
        self.socket = sock
        self.app = app
        self.context = context
        self.server = _StudioUvicornServer(uvicorn.Config(
            app,
            loop="asyncio",
            lifespan="off",
            log_config=UVICORN_LOG_CONFIG,
            access_log=False,
            proxy_headers=False,
            workers=1,
            timeout_keep_alive=5,
            limit_max_requests=None,
        ), context)
        self.server_address = sock.getsockname()
        self.server_port = int(sock.getsockname()[1]) if sock.family != socket.AF_UNIX else context.server_port
        self.address_family = sock.family
        self.owned_socket_identity = owned_unix
        self._closed = False
        self._lock = threading.Lock()

    @property
    def address(self) -> object:
        return self.server_address

    def serve_forever(self, poll_interval: float = 0.5) -> None:
        del poll_interval  # Uvicorn runs the event loop and polling itself.
        asyncio.run(self.server.serve(sockets=[self.socket]))

    def shutdown(self) -> None:
        self.server.should_exit = True

    def server_close(self) -> None:
        with self._lock:
            if self._closed:
                return
            self._closed = True
            try:
                self.socket.close()
            finally:
                if self.owned_socket_identity is not None:
                    socket_path = Path(str(self.server_address))
                    try:
                        stat = socket_path.stat()
                        if (stat.st_dev, stat.st_ino) == self.owned_socket_identity:
                            socket_path.unlink()
                    except FileNotFoundError:
                        pass


class CanvasServer(BoundServer):
    def __init__(self, tcp: BoundServer, unix: BoundServer | None, context: ApiContext) -> None:
        self.__dict__.update(tcp.__dict__)
        self.unix_server = unix
        self._tcp = tcp
        self._context = context

    def server_close(self) -> None:
        if self.unix_server is not None:
            self.unix_server.server_close()
        self._tcp.server_close()
        self._context.close()


def _bind_tcp(port: int) -> socket.socket:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        sock.bind((LISTEN_HOST, port))
        sock.listen(LISTEN_BACKLOG)
        sock.setblocking(False)
        return sock
    except BaseException:
        sock.close()
        raise


def _bind_unix(path: Path) -> tuple[socket.socket, tuple[int, int]]:
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        if not path.is_socket():
            raise OSError(f"Canvas socket path is occupied: {path}")
        probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        probe.settimeout(0.2)
        try:
            probe.connect(str(path))
        except OSError as error:
            if error.errno != errno.ECONNREFUSED:
                raise OSError(f"Canvas socket path is occupied: {path}") from error
            path.unlink()
        else:
            raise OSError(f"Canvas socket path is occupied: {path}")
        finally:
            probe.close()
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        sock.bind(str(path))
        os.chmod(path, UNIX_SOCKET_MODE)
        stat = path.stat()
        identity = (stat.st_dev, stat.st_ino)
        sock.listen(LISTEN_BACKLOG)
        sock.setblocking(False)
        return sock, identity
    except BaseException:
        sock.close()
        raise


def make_server(canvas: Canvas, port: int = 0, public_origin: str | None = None, unix_socket: bool = False) -> CanvasServer:
    """Bind sockets before Runtime construction, then expose the FastAPI app."""
    from codex_remote import RemoteAccess

    remote = cast(RemoteAccessContract, RemoteAccess(canvas.root, public_origin))
    context = ApiContext(
        canvas,
        remote=remote,
        unix_socket=unix_socket,
        backend_build=codex_backend_identity.BACKEND_BUILD,
    )
    app = create_app(context)
    tcp_socket = _bind_tcp(port)
    context.server_port = int(tcp_socket.getsockname()[1])
    unix_handle: BoundServer | None = None
    try:
        if unix_socket:
            unix_socket_fd, identity = _bind_unix(Path(canvas.root) / UNIX_SOCKET_NAME)
            unix_handle = BoundServer(unix_socket_fd, UnixScopeApp(app), context, owned_unix=identity)
        tcp = BoundServer(tcp_socket, app, context)
        tcp.unix_server = unix_handle
        context.initialize()
        # Hash generation starts in serve_forever after the bound sockets are ready.
        return CanvasServer(tcp, unix_handle, context)
    except BaseException:
        if unix_handle is not None:
            unix_handle.server_close()
        tcp_socket.close()
        context.close()
        raise
