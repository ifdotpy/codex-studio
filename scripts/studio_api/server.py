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
GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS = 2
SERVER_THREAD_JOIN_TIMEOUT_SECONDS = GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS + 1


class ShutdownEvent:
    """A shutdown flag that can wake stream generators on each server loop."""

    def __init__(self) -> None:
        self._requested = threading.Event()
        self._lock = threading.Lock()
        self._async_events: dict[asyncio.AbstractEventLoop, asyncio.Event] = {}

    def is_set(self) -> bool:
        return self._requested.is_set()

    def async_event(self) -> asyncio.Event:
        loop = asyncio.get_running_loop()
        with self._lock:
            event = self._async_events.get(loop)
            if event is None:
                event = asyncio.Event()
                self._async_events[loop] = event
            requested = self._requested.is_set()
        if requested:
            event.set()
        return event

    def set(self) -> None:
        self._requested.set()
        with self._lock:
            listeners = tuple(self._async_events.items())
        for loop, event in listeners:
            try:
                loop.call_soon_threadsafe(event.set)
            except RuntimeError:
                pass


class ShutdownAwareApp:
    """Finish an in-flight response when Uvicorn bounds shutdown draining."""

    def __init__(self, app: ASGIApp, shutdown_requested: ShutdownEvent) -> None:
        self.app = app
        self.shutdown_requested = shutdown_requested

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        response_started = False
        response_complete = False

        async def tracked_send(message: Message) -> None:
            nonlocal response_started, response_complete
            if message["type"] == "http.response.start":
                response_started = True
            elif message["type"] == "http.response.body" and not message.get("more_body", False):
                response_complete = True
            await send(message)

        try:
            marked = dict(scope)
            state = dict(scope.get("state", {}))
            state["studio_shutdown_event"] = self.shutdown_requested
            marked["state"] = state
            await self.app(marked, receive, tracked_send)
        except asyncio.CancelledError:
            if not self.shutdown_requested.is_set():
                raise
            if response_started and not response_complete:
                try:
                    await send({"type": "http.response.body", "body": b"", "more_body": False})
                except (OSError, RuntimeError):
                    pass


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

        maintenance(runtime)  # type: ignore[no-untyped-call]
        try:
            prune_audio(context.canvas.root)  # type: ignore[no-untyped-call]
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
            timeout_graceful_shutdown=GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS,
            limit_max_requests=None,
        ), context)
        self.server_address = sock.getsockname()
        self.server_port = int(sock.getsockname()[1]) if sock.family != socket.AF_UNIX else context.server_port
        self.address_family = sock.family
        self.owned_socket_identity = owned_unix
        self._closed = False
        self._lock = threading.Lock()
        self._serve_thread: threading.Thread | None = None
        self._loop: asyncio.AbstractEventLoop | None = None

    @property
    def address(self) -> object:
        return self.server_address

    def serve_forever(self, poll_interval: float = 0.5) -> None:
        del poll_interval  # Uvicorn runs the event loop and polling itself.
        self._serve_thread = threading.current_thread()

        async def serve() -> None:
            self._loop = asyncio.get_running_loop()
            await self.server.serve(sockets=[self.socket])

        asyncio.run(serve())

    def shutdown(self, *, force: bool = False) -> None:
        self.server.should_exit = True
        if force:
            self.server.force_exit = True
            loop = self._loop
            if loop is not None:
                try:
                    loop.call_soon_threadsafe(self._abort_connections)
                except RuntimeError:
                    pass

    def _abort_connections(self) -> None:
        for connection in tuple(self.server.server_state.connections):
            transport = getattr(connection, "transport", None)
            if transport is not None:
                transport.abort()

    def _join_serve_thread(self) -> bool:
        serving_thread = self._serve_thread
        if serving_thread is None or serving_thread is threading.current_thread():
            return True
        serving_thread.join(timeout=SERVER_THREAD_JOIN_TIMEOUT_SECONDS)
        return not serving_thread.is_alive()

    def server_close(self) -> bool:
        if not self._join_serve_thread():
            return False
        with self._lock:
            if self._closed:
                return True
            self._closed = True
            self.socket.close()
            if self.owned_socket_identity is not None:
                socket_path = Path(str(self.server_address))
                try:
                    stat = socket_path.stat()
                    if (stat.st_dev, stat.st_ino) == self.owned_socket_identity:
                        socket_path.unlink()
                except FileNotFoundError:
                    pass
        return True


class CanvasServer(BoundServer):
    def __init__(
        self,
        tcp: BoundServer,
        unix: BoundServer | None,
        context: ApiContext,
        shutdown_requested: ShutdownEvent,
    ) -> None:
        self.__dict__.update(tcp.__dict__)
        self.unix_server = unix
        self._tcp = tcp
        self._context = context
        self._shutdown_requested = shutdown_requested

    @property
    def shutdown_requested(self) -> ShutdownEvent:
        return self._shutdown_requested

    def shutdown(self, *, force: bool = False) -> None:
        self._shutdown_requested.set()
        self._tcp.shutdown(force=force)
        if self.unix_server is not None:
            self.unix_server.shutdown(force=force)

    def serve_forever(self, poll_interval: float = 0.5) -> None:
        self._tcp._serve_thread = threading.current_thread()
        super().serve_forever(poll_interval=poll_interval)

    def server_close(self) -> None:
        if not self._join_serve_thread():
            return
        if self.unix_server is not None:
            if not self.unix_server.server_close():
                return
        if not self._tcp.server_close():
            return
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

    remote = cast(RemoteAccessContract, RemoteAccess(canvas.root, public_origin))  # type: ignore[no-untyped-call]
    context = ApiContext(
        canvas,
        remote=remote,
        unix_socket=unix_socket,
        backend_build=codex_backend_identity.BACKEND_BUILD,
    )
    shutdown_requested = ShutdownEvent()
    app = ShutdownAwareApp(create_app(context), shutdown_requested)
    tcp_socket = _bind_tcp(port)
    context.server_port = int(tcp_socket.getsockname()[1])
    unix_handle: BoundServer | None = None
    try:
        if unix_socket:
            unix_socket_fd, identity = _bind_unix(Path(canvas.root) / UNIX_SOCKET_NAME)
            unix_handle = BoundServer(
                unix_socket_fd, UnixScopeApp(app), context, owned_unix=identity,
            )
        tcp = BoundServer(tcp_socket, app, context)
        tcp.unix_server = unix_handle
        context.initialize()
        # Hash generation starts in serve_forever after the bound sockets are ready.
        return CanvasServer(tcp, unix_handle, context, shutdown_requested)
    except BaseException:
        if unix_handle is not None:
            unix_handle.server_close()
        tcp_socket.close()
        context.close()
        raise
