from __future__ import annotations

import asyncio
import http.client
import io
import logging
import subprocess
import sys
import threading
import unittest

from studio_api.context import ApiContext
from studio_api import server as server_module

BoundServer = server_module.BoundServer
_bind_tcp = server_module._bind_tcp


class ServerLifecycleTests(unittest.TestCase):
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
                    request_path = "/api/sync/stream" if clean_end else "/download"
                    connection.request("GET", request_path)
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
                    if clean_end:
                        self.assertEqual(response_body, b"")
                        logs = log_stream.getvalue()
                        self.assertNotIn("Exception in ASGI application", logs)
                        self.assertNotIn("LocalProtocolError", logs)
                    else:
                        self.assertIsNotNone(response_error, "truncated downloads remain client errors")
                    logs = log_stream.getvalue()
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
