from __future__ import annotations

import asyncio
import http.client
import threading
import unittest
from unittest.mock import patch

from studio_api.context import ApiContext
from studio_api import server as server_module

BoundServer = server_module.BoundServer
CanvasServer = server_module.CanvasServer
_bind_tcp = server_module._bind_tcp


class ServerLifecycleTests(unittest.TestCase):
    def test_tcp_listener_stays_open_until_timed_out_serve_thread_exits(self) -> None:
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
        shutdown_event_type = getattr(server_module, "ShutdownEvent", None)
        if shutdown_event_type is None:
            server = CanvasServer(tcp, None, context)
        else:
            server = CanvasServer(tcp, None, context, shutdown_event_type())
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
            with patch(
                "studio_api.server.SERVER_THREAD_JOIN_TIMEOUT_SECONDS",
                0.05,
                create=True,
            ):
                server.server_close()
            self.assertTrue(thread.is_alive(), "the request handler remains in flight")
            self.assertGreaterEqual(
                tcp.socket.fileno(), 0,
                "listener must stay open until its serving loop exits",
            )

            release.set()
            thread.join(timeout=5)
            self.assertFalse(thread.is_alive(), "serve thread exits after the handler is released")
            self.assertTrue(app_finished.wait(1))
            server.server_close()
            self.assertEqual(tcp.socket.fileno(), -1)
        finally:
            release.set()
            if thread.is_alive():
                try:
                    server.shutdown(force=True)
                except TypeError:
                    server.shutdown()
                thread.join(timeout=5)
            server.server_close()
            connection.close()


if __name__ == "__main__":
    unittest.main()
