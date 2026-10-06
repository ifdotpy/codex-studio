from __future__ import annotations

import asyncio
import http.client
import io
import logging
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from studio_api.context import ApiContext
from studio_api import server as server_module

BoundServer = server_module.BoundServer
CanvasServer = server_module.CanvasServer
_bind_tcp = server_module._bind_tcp
_bind_unix = server_module._bind_unix


class ServerLifecycleTests(unittest.TestCase):
    def test_successful_server_close_waits_for_serve_thread_before_owned_close(self) -> None:
        release = threading.Event()
        close_observations: list[bool] = []

        async def app(scope, receive, send) -> None:
            del scope, receive
            await send({"type": "http.response.start", "status": 200, "headers": []})
            await send({"type": "http.response.body", "body": b"ok"})

        context = ApiContext.for_schema()
        sock = _bind_tcp(0)
        tcp = BoundServer(sock, app, context)
        original_close = tcp._close_bound_resources

        def record_close(*, close_socket: bool = True) -> bool:
            if close_socket:
                close_observations.append(thread.is_alive())
            return original_close(close_socket=close_socket)

        tcp._close_bound_resources = record_close
        original_serve = tcp.server.serve

        async def hold_thread_after_serve(*args, **kwargs) -> None:
            await original_serve(*args, **kwargs)
            await asyncio.to_thread(release.wait)

        tcp.server.serve = hold_thread_after_serve
        tcp.server.config.timeout_graceful_shutdown = 0.05
        thread = threading.Thread(target=tcp.serve_forever, daemon=True)
        thread.start()
        try:
            end = time.monotonic() + 3
            while not tcp.server.started and thread.is_alive() and time.monotonic() < end:
                threading.Event().wait(0.01)
            self.assertTrue(tcp.server.started, "Uvicorn started before shutdown")
            tcp.shutdown()
            releaser = threading.Timer(0.15, release.set)
            releaser.start()
            self.assertTrue(tcp.server_close())
            releaser.join(timeout=1)
            self.assertFalse(thread.is_alive())
            self.assertEqual(close_observations, [False])
            self.assertEqual(sock.fileno(), -1)
        finally:
            release.set()
            if thread.is_alive():
                tcp.shutdown()
                thread.join(timeout=3)
            tcp.server_close()
            context.close()

    def test_shutdown_cancellation_preserves_download_errors_and_closes_event_stream(self) -> None:
        cases = (
            ("chunked", [(b"content-type", b"application/octet-stream")], b"part", False),
            ("content-length", [(b"content-type", b"application/octet-stream"),
                                (b"content-length", b"100")], b"part", False),
            ("event-stream", [(b"content-type", b"text/event-stream")], b"data: first\n\n", True),
        )
        has_shutdown_wrapper = hasattr(server_module, "ShutdownAwareApp")
        for name, headers, first_body, clean_end in cases:
            with self.subTest(name=name):
                entered = threading.Event()
                shutdown_type = getattr(server_module, "ShutdownEvent", None)
                shutdown = shutdown_type() if shutdown_type is not None else None

                async def app(scope, receive, send) -> None:
                    del scope, receive
                    await send({"type": "http.response.start", "status": 200, "headers": headers})
                    await send({"type": "http.response.body", "body": first_body, "more_body": True})
                    entered.set()
                    await asyncio.Event().wait()

                context = ApiContext.for_schema()
                sock = _bind_tcp(0)
                app_to_serve = (
                    server_module.ShutdownAwareApp(app, shutdown)
                    if has_shutdown_wrapper
                    else app
                )
                shutdown_option = {"shutdown_requested": shutdown} if shutdown is not None else {}
                server = BoundServer(
                    sock,
                    app_to_serve,
                    context,
                    **shutdown_option,
                )
                server.server.config.timeout_graceful_shutdown = 0.05
                thread = threading.Thread(target=server.serve_forever, daemon=True)
                thread.start()
                connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=5)
                log_stream = io.StringIO()
                log_handler = logging.StreamHandler(log_stream)
                uvicorn_logger = logging.getLogger("uvicorn.error")
                uvicorn_logger.addHandler(log_handler)
                try:
                    connection.request("GET", "/download")
                    response = connection.getresponse()
                    self.assertEqual(response.status, 200)
                    self.assertTrue(entered.wait(2))
                    self.assertEqual(response.read(len(first_body)), first_body)

                    if shutdown is not None:
                        shutdown.set()
                    server.shutdown()
                    thread.join(timeout=5)
                    self.assertFalse(thread.is_alive(), "Uvicorn cancels the blocked response at the drain bound")
                    response_error = None
                    try:
                        response_body = response.read()
                    except (http.client.IncompleteRead, OSError) as error:
                        response_error = error
                        response_body = None
                    if has_shutdown_wrapper and clean_end:
                        self.assertEqual(response_body, b"")
                    elif has_shutdown_wrapper:
                        self.assertIsNotNone(response_error, "truncated downloads remain client errors")
                    else:
                        print(f"base shutdown response {name}: error={response_error is not None}")
                    logs = log_stream.getvalue()
                    if has_shutdown_wrapper:
                        self.assertNotIn("Exception in ASGI application", logs)
                        self.assertNotIn("LocalProtocolError", logs)
                    print(
                        f"shutdown response {name}: client_error={response_error is not None}, "
                        f"traceback={'Traceback' in logs}, protocol_error={'LocalProtocolError' in logs}"
                    )
                finally:
                    uvicorn_logger.removeHandler(log_handler)
                    log_handler.close()
                    connection.close()
                    if thread.is_alive():
                        shutdown.set()
                        server.shutdown()
                        thread.join(timeout=5)
                    server.server_close()
                    context.close()

    def test_timed_out_join_still_releases_resources_and_closes_context(self) -> None:
        entered = threading.Event()
        release = threading.Event()
        app_finished = threading.Event()

        async def app(scope, receive, send) -> None:
            del scope, receive
            await send({"type": "http.response.start", "status": 200, "headers": []})
            entered.set()
            try:
                await asyncio.to_thread(release.wait)
                await send({"type": "http.response.body", "body": b"done"})
            finally:
                app_finished.set()

        sock = _bind_tcp(0)
        context = ApiContext.for_schema()
        tcp = BoundServer(sock, app, context)
        socket_path = Path(tempfile.mkdtemp()) / "canvas.sock"
        unix_sock, socket_identity = _bind_unix(socket_path)
        unix = BoundServer(unix_sock, app, context, owned_unix=socket_identity)
        shutdown_event_type = getattr(server_module, "ShutdownEvent", None)
        if shutdown_event_type is None:
            server = CanvasServer(tcp, unix, context)
        else:
            server = CanvasServer(tcp, unix, context, shutdown_event_type())
        server.server.config.timeout_graceful_shutdown = 0.05
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=5)
        try:
            connection.request("GET", "/blocked")
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            self.assertTrue(entered.wait(2))
            if hasattr(tcp, "_serve_thread"):
                self.assertIs(tcp._serve_thread, thread)

            server.shutdown()
            with (
                patch("studio_api.server.SERVER_THREAD_JOIN_TIMEOUT_SECONDS", 0.05),
                patch.object(tcp, "_join_serve_thread", wraps=tcp._join_serve_thread) as join,
                patch.object(context, "close", wraps=context.close) as close_context,
            ):
                joined = server.server_close()
                join.assert_called_once_with()
                self.assertFalse(joined, "the deliberately blocked serve thread exceeds the join bound")
                self.assertTrue(thread.is_alive(), "the request handler remains in flight")
                self.assertGreaterEqual(
                    tcp.socket.fileno(), 0,
                    "a still-running selector keeps its descriptor until Uvicorn closes it",
                )
                self.assertFalse(socket_path.exists(), "an expired join still unlinks the owned Unix socket")
                close_context.assert_called_once_with()

            release.set()
            thread.join(timeout=5)
            self.assertFalse(thread.is_alive(), "serve thread exits after the handler is released")
            self.assertTrue(app_finished.wait(1))
            self.assertEqual(tcp.socket.fileno(), -1)
        finally:
            release.set()
            if thread.is_alive():
                server.shutdown()
                thread.join(timeout=5)
            if thread.is_alive():
                server.server_close()
            connection.close()

    def test_shutdown_event_signal_handler_never_waits_on_registration_lock(self) -> None:
        script = r"""
import signal
import threading
from types import SimpleNamespace
from studio_api.server import BoundServer, ShutdownEvent
event = ShutdownEvent()
bound = object.__new__(BoundServer)
bound.server = SimpleNamespace(shutdown_requested=event)
event._lock.acquire()
signal.signal(signal.SIGTERM, lambda _signum, _frame: bound.request_shutdown_from_signal())
signal.raise_signal(signal.SIGTERM)
event._lock.release()
assert event.is_set()
print('signal-safe')
"""
        result = subprocess.run(
            [sys.executable, "-B", "-c", script],
            capture_output=True,
            text=True,
            timeout=3,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("signal-safe", result.stdout)

    def test_shutdown_event_does_not_lose_concurrent_loop_registration(self) -> None:
        script = r"""
import asyncio
import sys
import threading
from studio_api.server import ShutdownEvent
sys.setswitchinterval(1e-6)
class CountingLock:
    def __init__(self):
        self.lock = threading.Lock()
        self.acquisitions = 0
    def __enter__(self):
        self.lock.acquire()
        self.acquisitions += 1
        return self
    def __exit__(self, *_):
        self.lock.release()
registry_lock = CountingLock()
for _ in range(3000):
    shutdown = ShutdownEvent()
    shutdown._lock = registry_lock
    barrier = threading.Barrier(3)
    failures = []
    def register():
        async def wait_for_shutdown():
            barrier.wait()
            await shutdown.async_event().wait()
        try:
            asyncio.run(wait_for_shutdown())
        except BaseException as error:
            failures.append(error)
    workers = [threading.Thread(target=register) for _ in range(2)]
    for worker in workers: worker.start()
    barrier.wait()
    shutdown.set()
    for worker in workers: worker.join(timeout=1)
    assert not any(worker.is_alive() for worker in workers), 'a loop missed shutdown notification'
    assert not failures, failures
assert registry_lock.acquisitions == 9000, registry_lock.acquisitions
print('3000 two-loop registration races passed')
"""
        result = subprocess.run(
            [sys.executable, "-B", "-c", script],
            capture_output=True,
            text=True,
            timeout=60,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("3000 two-loop registration races passed", result.stdout)


if __name__ == "__main__":
    unittest.main()
