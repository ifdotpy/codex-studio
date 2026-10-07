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
from collections.abc import Callable, Iterator
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
GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS = 10
SERVER_THREAD_JOIN_TIMEOUT_SECONDS = GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS + 1


class ShutdownEvent:
    """A shutdown flag that can wake stream generators on each server loop."""

    def __init__(self) -> None:
        # Signal handlers only write _requested directly; they never take this
        # lock. Other callers synchronize registration with listener delivery.
        self._requested = False
        self._notified = False
        self._async_events: tuple[
            tuple[asyncio.AbstractEventLoop, asyncio.Event], ...
        ] = ()
        self._lock = threading.Lock()

    def is_set(self) -> bool:
        return self._requested

    def async_event(self) -> asyncio.Event:
        loop = asyncio.get_running_loop()
        with self._lock:
            listeners = self._async_events
            event = next((candidate for registered_loop, candidate in listeners
                          if registered_loop is loop), None)
            if event is None:
                event = asyncio.Event()
                self._async_events = (*listeners, (loop, event))
            requested = self._requested or self._notified
        if requested:
            event.set()
        return event

    def set(self) -> None:
        self._requested = True
        self.notify_listeners()

    def notify_listeners(self) -> None:
        with self._lock:
            if self._notified:
                return
            self._notified = True
            listeners = self._async_events
        for loop, event in listeners:
            try:
                loop.call_soon_threadsafe(event.set)
            except RuntimeError:
                pass


class ShutdownAwareApp:
    """End event streams cleanly and abort other responses on forced drain."""

    def __init__(self, app: ASGIApp, shutdown_requested: ShutdownEvent) -> None:
        self.app = app
        self.shutdown_requested = shutdown_requested

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        response_started = False
        response_complete = False
        event_stream = False

        async def tracked_send(message: Message) -> None:
            nonlocal response_started, response_complete, event_stream
            if message["type"] == "http.response.start":
                response_started = True
                content_type = next(
                    (value for name, value in message.get("headers", ())
                     if name.lower() == b"content-type"),
                    b"",
                )
                event_stream = (
                    content_type.split(b";", 1)[0].strip().lower()
                    == b"text/event-stream"
                )
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
            if event_stream and response_started and not response_complete:
                try:
                    await send({"type": "http.response.body", "body": b"", "more_body": False})
                except (OSError, RuntimeError):
                    pass
            # For all other responses, returning without the final body makes
            # Uvicorn close the transport. This preserves an observable client
            # error for truncated downloads without a protocol traceback.


class _StudioUvicornServer(uvicorn.Server):
    def __init__(self, config: uvicorn.Config, context: ApiContext) -> None:
        super().__init__(config)
        self.context = context
        self.schema_hash_start_scheduled = False
        self.shutdown_event: ShutdownEvent | None = None
        self.shutdown_requested: Callable[[], bool] | None = None

    @contextmanager
    def capture_signals(self) -> Iterator[None]:
        # codex_canvas owns SIGTERM and SIGINT for both listener loops.
        yield

    async def on_tick(self, counter: int) -> bool:
        if self.shutdown_requested is not None and self.shutdown_requested():
            if self.shutdown_event is not None:
                self.shutdown_event.set()
            self.should_exit = True
        should_exit = await super().on_tick(counter)
        if should_exit:
            return True
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
        shutdown_requested: ShutdownEvent | None = None,
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
        self.server.shutdown_event = shutdown_requested
        self.server_address = sock.getsockname()
        self.server_port = int(sock.getsockname()[1]) if sock.family != socket.AF_UNIX else context.server_port
        self.address_family = sock.family
        self.owned_socket_identity = owned_unix
        self._closed = False
        self._socket_path_unlinked = False
        self._lock = threading.Lock()
        self._serve_thread: threading.Thread | None = None

    @property
    def address(self) -> object:
        return self.server_address

    def serve_forever(
        self, poll_interval: float = 0.5, *, shutdown_requested: Callable[[], bool] | None = None,
    ) -> None:
        del poll_interval  # Uvicorn runs the event loop and polling itself.
        self._serve_thread = threading.current_thread()
        self.server.shutdown_requested = shutdown_requested
        async def serve() -> None:
            await self.server.serve(sockets=[self.socket])
        asyncio.run(serve())

    def shutdown(self) -> None:
        self.server.should_exit = True

    def request_shutdown_from_signal(self) -> None:
        if self.server.shutdown_event is not None:
            self.server.shutdown_event._requested = True

    def _join_serve_thread(self) -> bool:
        serving_thread = self._serve_thread
        if serving_thread is None or serving_thread is threading.current_thread():
            return True
        serving_thread.join(timeout=SERVER_THREAD_JOIN_TIMEOUT_SECONDS)
        return not serving_thread.is_alive()

    def _close_bound_resources(self, *, close_socket: bool = True) -> bool:
        with self._lock:
            if self._closed:
                return True
            if close_socket:
                self._closed = True
                self.socket.close()
            if not self._socket_path_unlinked and self.owned_socket_identity is not None:
                self._socket_path_unlinked = True
                socket_path = Path(str(self.server_address))
                try:
                    stat = socket_path.stat()
                    if (stat.st_dev, stat.st_ino) == self.owned_socket_identity:
                        socket_path.unlink()
                except FileNotFoundError:
                    pass
        return True

    def server_close(self) -> bool:
        joined = self._join_serve_thread()
        # Uvicorn owns the live descriptor while its selector is running. Its
        # shutdown closes that descriptor when a bounded join expires.
        self._close_bound_resources(close_socket=joined)
        return joined


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
        self._tcp.server.shutdown_event = shutdown_requested
        if unix is not None:
            unix.server.shutdown_event = shutdown_requested

    @property
    def shutdown_requested(self) -> ShutdownEvent:
        return self._shutdown_requested

    def shutdown(self) -> None:
        self._shutdown_requested.set()
        self._tcp.shutdown()
        if self.unix_server is not None:
            self.unix_server.shutdown()

    def serve_forever(
        self, poll_interval: float = 0.5, *, shutdown_requested: Callable[[], bool] | None = None,
    ) -> None:
        self._tcp._serve_thread = threading.current_thread()
        super().serve_forever(poll_interval=poll_interval, shutdown_requested=shutdown_requested)

    def server_close(self) -> bool:
        # The TCP serve thread is this caller when codex-canvas runs Uvicorn in
        # the main thread; the Unix listener has its own serving thread.
        tcp_joined = self._tcp._join_serve_thread()
        unix_joined = True
        if self.unix_server is not None:
            unix_joined = self.unix_server._join_serve_thread()
            self.unix_server._close_bound_resources(close_socket=unix_joined)
        self._tcp._close_bound_resources(close_socket=tcp_joined)
        self._context.close()
        return tcp_joined and unix_joined


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
                shutdown_requested=shutdown_requested,
            )
        tcp = BoundServer(
            tcp_socket, app, context, shutdown_requested=shutdown_requested,
        )
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
