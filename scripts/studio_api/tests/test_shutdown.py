"""Exercise real backend signal shutdown with disposable child processes."""
from __future__ import annotations

import asyncio
from contextlib import contextmanager
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import tempfile
import threading
import time
from types import ModuleType, SimpleNamespace
import unittest
from unittest.mock import patch

import httpx


def run_fixture(mode: str) -> None:
    """Run the real main and listeners without native workers or model calls."""
    import codex_canvas
    import studio_api.server
    from uvicorn.protocols.http.h11_impl import RequestResponseCycle

    def close_runtime() -> None:
        if mode == "cleanup":
            time.sleep(30)
        elif mode == "thread":
            threading.Thread(target=threading.Event().wait, daemon=False).start()
        print("runtime-closed", flush=True)

    runtime_module = ModuleType("codex_runtime")
    updates_module = ModuleType("codex_live_updates")
    listeners = []

    def make_runtime(_root):
        if mode == "startup":
            print(json.dumps({"ready": listeners[0].server_port}), flush=True)
            os.kill(os.getpid(), signal.SIGTERM)
            time.sleep(30)
        return SimpleNamespace(close=close_runtime, lock=threading.RLock())

    runtime_module.Runtime = make_runtime

    def start_updates(_runtime):
        print(json.dumps({"ready": listeners[0].server_port}), flush=True)

    updates_module.start = start_updates
    original_factory = codex_canvas.make_server
    original_send = RequestResponseCycle.send

    def make_server(*args, **kwargs):
        server = original_factory(*args, **kwargs)
        server.context._maintenance_last = time.monotonic()
        listeners.append(server)
        return server

    async def send_response(cycle, message):
        if mode != "idle" and message["type"] == "http.response.body" and cycle.scope["path"] == "/api/session":
            number = signal.SIGINT if mode == "sigint" else signal.SIGTERM
            os.kill(os.getpid(), number)
            if mode == "loop":
                time.sleep(30)
            elif mode == "repeat":
                await asyncio.sleep(0.1)
                os.kill(os.getpid(), number)
                await asyncio.sleep(0.1)
            elif mode == "grace":
                try:
                    await asyncio.sleep(30)
                finally:
                    print("response-cancelled", flush=True)
        await original_send(cycle, message)

    with patch.dict(sys.modules, {"codex_runtime": runtime_module, "codex_live_updates": updates_module}), \
            patch.object(codex_canvas, "make_server", make_server), \
            patch.object(codex_canvas, "SHUTDOWN_TIMEOUT_SECONDS", 1.5, create=True), \
            patch.object(studio_api.server, "GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS", 0.3, create=True), \
            patch.object(RequestResponseCycle, "send", send_response), \
            patch.object(sys, "argv", ["codex-canvas", "--port", "0"]):
        codex_canvas.main()


class BackendShutdownTests(unittest.TestCase):
    @contextmanager
    def backend(self, mode: str):
        # macOS Unix sockets cannot use the long default temporary path.
        with tempfile.TemporaryDirectory(prefix="studio-shutdown-", dir="/tmp") as directory:
            root = Path(directory)
            scripts = Path(__file__).resolve().parents[2]
            environment = os.environ.copy()
            environment.update({
                "CODEX_AGENTS_STATE_DIR": str(root / "state"),
                "CODEX_HOME": str(root / "profile"),
                "XDG_CACHE_HOME": str(root / "cache"),
                "CODEX_AGENTS_SUPERVISOR_MODE": "0",
                "PYTHONPATH": os.pathsep.join((str(scripts), environment.get("PYTHONPATH", ""))),
            })
            # The child sees only this fixture's state and empty native profile.
            for name in ("CODEX_BOARD_STATE_DIR", "CODEX_CANVAS_URL", "CODEX_AGENTS_SUPERVISOR_FALLBACK"):
                environment.pop(name, None)
            command = [sys.executable, "-B", "-c",
                       "from studio_api.tests.test_shutdown import run_fixture; "
                       "import sys; run_fixture(sys.argv[1])", mode]
            process = subprocess.Popen(command, env=environment, stdout=subprocess.PIPE,
                                       stderr=subprocess.PIPE, text=True)
            try:
                assert process.stdout is not None
                deadline = time.monotonic() + 10
                port = None
                while time.monotonic() < deadline:
                    readable, _, _ = select.select([process.stdout], [], [], max(0, deadline - time.monotonic()))
                    if not readable:
                        break
                    line = process.stdout.readline()
                    if not line:
                        break
                    if line.startswith('{"ready":'):
                        port = json.loads(line)["ready"]
                        break
                if port is None:
                    if process.poll() is None:
                        process.kill()
                    stdout, stderr = process.communicate(timeout=5)
                    self.fail(f"Backend did not bind its isolated listeners: {stdout} {stderr}")
                yield process, root / "state", port
            except Exception:
                if process.poll() is None:
                    process.kill()
                stdout, stderr = process.communicate(timeout=5)
                print(f"Fixture output: {stdout} {stderr}", file=sys.stderr)
                raise
            finally:
                if process.poll() is None:
                    process.kill()
                process.communicate(timeout=5)

    def assert_exit(self, process, *, code: int = 0):
        try:
            stdout, stderr = process.communicate(timeout=4)
        except subprocess.TimeoutExpired:
            process.kill()
            stdout, stderr = process.communicate(timeout=5)
            self.fail(f"Backend ignored its shutdown request: {stdout} {stderr}")
        self.assertEqual(process.returncode, code, stderr)
        self.assertNotIn("KeyboardInterrupt", stderr)
        records = [json.loads(line) for line in stderr.splitlines() if line.startswith('{"event": "backend_shutdown"')]
        self.assertEqual(len(records), 1, stderr)
        self.assertEqual(records[0]["pid"], process.pid)
        return stdout, stderr, records[0]

    def test_sigterm_during_response_finishes_and_stops_both_listeners(self):
        for listener in ("tcp", "unix"):
            with self.subTest(listener=listener), self.backend("response") as (process, state, port):
                transport = httpx.HTTPTransport(uds=str(state / "canvas.sock")) if listener == "unix" else None
                with httpx.Client(transport=transport, timeout=3) as client:
                    response = client.get(f"http://127.0.0.1:{port}/api/session")
                self.assertEqual(response.status_code, 200)
                self.assertIn("token", response.json())
                _, _, record = self.assert_exit(process)
                self.assertEqual(record["signal"], signal.SIGTERM)
                self.assertFalse((state / "canvas.sock").exists())

    def test_repeat_sigterm_and_sigint_allow_response_to_finish(self):
        for mode in ("repeat", "sigint"):
            with self.subTest(mode=mode), self.backend(mode) as (process, state, port):
                response = httpx.get(f"http://127.0.0.1:{port}/api/session", timeout=3)
                self.assertEqual(response.status_code, 200)
                self.assertIn("token", response.json())
                _, _, record = self.assert_exit(process)
                self.assertEqual(record["signal"], signal.SIGINT if mode == "sigint" else signal.SIGTERM)
                self.assertFalse((state / "canvas.sock").exists())

    def test_graceful_timeout_cancels_a_stalled_response(self):
        with self.backend("grace") as (process, state, port):
            with self.assertRaises(httpx.RemoteProtocolError):
                httpx.get(f"http://127.0.0.1:{port}/api/session", timeout=3)
            stdout, stderr, _ = self.assert_exit(process)
            self.assertIn("response-cancelled", stdout)
            self.assertIn("timeout graceful shutdown exceeded", stderr)
            self.assertFalse((state / "canvas.sock").exists())

    def test_hard_deadline_covers_loop_cleanup_and_interpreter_threads(self):
        for mode in ("loop", "cleanup", "thread"):
            with self.subTest(mode=mode), self.backend(mode) as (process, _state, port):
                started = time.monotonic()
                try:
                    response = httpx.get(f"http://127.0.0.1:{port}/api/session", timeout=3)
                except httpx.RemoteProtocolError:
                    self.assertEqual(mode, "loop")
                else:
                    self.assertEqual(response.status_code, 200)
                stdout, _, _ = self.assert_exit(process, code=1)
                self.assertLess(time.monotonic() - started, 3)
                if mode == "thread":
                    self.assertIn("runtime-closed", stdout)

    def test_sigterm_during_runtime_startup_obeys_hard_deadline(self):
        with self.backend("startup") as (process, _state, _port):
            started = time.monotonic()
            self.assert_exit(process, code=1)
            self.assertLess(time.monotonic() - started, 3)

    def test_idle_backend_serves_requests_until_external_sigterm(self):
        with self.backend("idle") as (process, state, port):
            with httpx.Client(base_url=f"http://127.0.0.1:{port}", timeout=3) as client:
                first = client.get("/api/session")
                second = client.get("/api/session")
            self.assertEqual((first.status_code, second.status_code), (200, 200))
            self.assertEqual(first.json()["token"], second.json()["token"])
            process.send_signal(signal.SIGTERM)
            self.assert_exit(process)
            self.assertFalse((state / "canvas.sock").exists())


if __name__ == "__main__":
    unittest.main()
