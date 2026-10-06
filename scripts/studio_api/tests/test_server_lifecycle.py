from __future__ import annotations

import asyncio
import http.client
import signal
import subprocess
import sys
import tempfile
import threading
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
from studio_api.server import ShutdownEvent
event = ShutdownEvent()
held = threading.Lock()
event._lock = held
held.acquire()
signal.signal(signal.SIGUSR1, lambda _signum, _frame: event.set())
signal.raise_signal(signal.SIGUSR1)
held.release()
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


if __name__ == "__main__":
    unittest.main()
